// A file of a thread's workspace, opened in a tab: a picture as it is, Markdown drawn,
// text as text, a web page as a page (page.mjs puts in what it refers to from the
// workspace; it runs in a sandboxed frame, preview.html, with no way to Spindle or its
// keys, and reaches the web only when the user lets it), anything else to download.
import { CHANNEL, keepPreviewChoice, previewChoice, startEngine } from './shared.mjs';
import { containment, kindOf, pageOf, previewFrame, previewNote, saveFile, typeOf } from './page.mjs';
import { mdNodes } from './markdown.mjs';
import { markdown } from './text.js';
import { shownName, shownPath } from './steps.mjs';

const query = new URLSearchParams(location.search);
const thread = query.get('thread') ?? '';
const path = query.get('path') ?? '';
const me = crypto.randomUUID();
const channel = new BroadcastChannel(CHANNEL);
const waiting = new Map();
let next = 0;

channel.onmessage = ({ data }) => {
  if (data?.to !== me) return;
  const answer = waiting.get(data.id);
  if (!answer) return;
  waiting.delete(data.id);
  clearTimeout(answer.timer);
  if (data.ok) answer.resolve(data.result);
  else answer.reject(new Error(data.error.message));
};

function ask(op, args, timeout = 60_000) {
  const id = ++next;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      waiting.delete(id);
      reject(Object.assign(new Error("Spindle's engine did not answer"), { kind: 'timeout' }));
    }, timeout);
    waiting.set(id, { resolve, reject, timer });
    channel.postMessage({ to: 'engine', from: me, id, op, args });
  });
}

/** The first answer once the engine runs (it may be starting now). */
async function first(op, args) {
  await startEngine();
  for (let attempt = 0; ; attempt++) {
    try {
      return await ask(op, args, 3_000);
    } catch (error) {
      if (error.kind !== 'timeout' || attempt >= 20) throw error;
    }
  }
}

const read = async (name) => (await ask('read', { id: thread, path: name })).bytes;
/** Text shown at most (the rest is the download's). */
const MAX_TEXT = 2 * 1024 * 1024;
/** The newest `show`: an earlier one (Reload clicked again, say) that ends after it
 * shows nothing, its success or its failure. */
let showing = 0;

/** What is shown, in place of what was (one view, whatever came before; a frame left
 * outside the box is taken out too). */
function commit(view) {
  for (const stray of document.querySelectorAll('#view')) if (stray.parentElement?.id !== 'view-box') stray.remove();
  document.querySelector('#view-box').replaceChildren(...(view ? [view] : []));
}

const ms = (from, to) => `${Math.round(to - from)} ms`;

/** Shows the file; a page as the user let its version show (`choice`: what they just
 * chose, kept for the version it shows; else what was kept). */
async function show(choice = null) {
  const generation = ++showing;
  const newest = () => generation === showing;
  document.title = `${shownName(path)} · Spindle`;
  document.querySelector('#view-path').textContent = shownPath(path);
  const note = document.querySelector('#view-note');
  note.hidden = true;
  // For a page: whether it can run here without reaching anything, found meanwhile.
  const contained = kindOf(path) === 'page' ? containment() : null;
  // Where the time goes, told on the console and on the path's tooltip: the engine
  // starting, the workspace listed, the files the page puts in read, the page made
  // ready, and its frame written in.
  const time = { start: performance.now(), reading: 0, reads: 0, bytes: 0 };
  const timedRead = async (name) => {
    const at = performance.now();
    try {
      const bytes = await read(name);
      time.reads += 1;
      time.bytes += bytes.length;
      return bytes;
    } finally {
      time.reading += performance.now() - at;
    }
  };
  try {
    await first('ping', {});
    time.engine = performance.now();
    const { files } = await ask('files', { id: thread });
    time.listed = performance.now();
    if (!newest()) return;
    const file = files.find((entry) => entry.path === path);
    if (!file) throw new Error(`the thread has no file ${path}`);
    const kind = kindOf(path);
    if (kind === 'page') {
      const allowed = choice ?? await previewChoice(thread, path);
      const page = await pageOf({ path, files, read: timedRead, allowed, contained: await contained });
      time.prepared = performance.now();
      const reading = time.reading;
      if (!newest()) return;
      // Kept only for the version it was made on (one changed meanwhile shows as before).
      if (choice && page.version && (choice.version ?? page.version) === page.version) await keepPreviewChoice(thread, path, page.version, choice).catch(() => {});
      const told = () => {
        const said = `engine ${ms(time.start, time.engine)}, workspace listed ${ms(time.engine, time.listed)}, `
          + `${time.reads} file(s) read ${ms(0, reading)} (${time.bytes} bytes), page made ${ms(0, time.prepared - time.listed - reading)}, `
          + `frame ${ms(time.prepared, performance.now())}`;
        console.info(`Spindle preview of ${path}: ${said}`);
        if (newest()) document.querySelector('#view-path').title = said;
      };
      const frame = previewFrame(page, path, { shown: told, refused: () => newest() && show() });
      frame.id = 'view';
      commit(frame);
      note.hidden = !previewNote(note, page, await contained, (made) => show(made));
      return;
    }
    const box = Object.assign(document.createElement('div'), { id: 'view', className: `view-${kind ?? 'bytes'}` });
    let cut = false;
    if (kind === 'picture') {
      const image = Object.assign(document.createElement('img'), { alt: path });
      image.src = URL.createObjectURL(new Blob([await read(path)], { type: typeOf(path) }));
      box.append(image);
    } else if (kind === 'markdown' || kind === 'text') {
      const bytes = await read(path);
      const text = new TextDecoder().decode(bytes.subarray(0, MAX_TEXT));
      if (kind === 'markdown') mdNodes(Object.assign(box, { className: 'view-markdown md' }), markdown(text));
      else box.append(Object.assign(document.createElement('pre'), { className: 'code', textContent: text }));
      cut = bytes.length > MAX_TEXT;
    } else {
      box.append(Object.assign(document.createElement('p'), { className: 'note', textContent: `${path} is not a page, picture or text: download it to open it.` }));
    }
    if (!newest()) return;
    commit(box);
    if (cut) {
      note.textContent = 'Only the beginning shows here: download it to read it all.';
      note.hidden = false;
    }
  } catch (error) {
    if (!newest()) return;
    commit(null);
    note.textContent = `This file could not be shown: ${error.message}`;
    note.hidden = false;
  }
}

document.querySelector('#view-reload').onclick = () => show();
document.querySelector('#view-download').onclick = async () => {
  try {
    await saveFile(await read(path), path.split('/').pop(), typeOf(path));
  } catch (error) {
    const note = document.querySelector('#view-note');
    note.textContent = `This file could not be downloaded: ${error.message}`;
    note.hidden = false;
  }
};
show();
