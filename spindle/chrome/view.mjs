// A file of a thread's workspace, opened in a tab: a picture as it is, Markdown drawn,
// text as text, a web page as a page (page.mjs puts in what it refers to from the
// workspace; it runs in a sandboxed frame, preview.html, with no way to Spindle or its
// keys, and reaches the web only when the user lets it), anything else to download.
import { CHANNEL, startEngine } from './shared.mjs';
import { kindOf, pageOf, previewFrame, saveFile, typeOf } from './page.mjs';
import { mdNodes } from './markdown.mjs';
import { markdown } from './text.js';

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
/** Whether the user let this page load from the web. */
let web = false;
/** Text shown at most (the rest is the download's). */
const MAX_TEXT = 2 * 1024 * 1024;

async function show() {
  document.title = `${path.split('/').pop()} · Spindle`;
  document.querySelector('#view-path').textContent = path;
  const note = document.querySelector('#view-note');
  const allow = document.querySelector('#view-web');
  allow.hidden = true;
  document.querySelector('#view')?.remove();
  note.hidden = true;
  try {
    const { files } = await first('files', { id: thread });
    const file = files.find((entry) => entry.path === path);
    if (!file) throw new Error(`the thread has no file ${path}`);
    const kind = kindOf(path);
    if (kind === 'page') {
      const page = await pageOf({ path, files, read, web });
      const frame = previewFrame(page.html, path);
      frame.id = 'view';
      document.body.append(frame);
      if (page.web.length && !web) {
        note.textContent = `This page would load from ${page.web.join(', ')}; it reaches nothing outside the workspace unless you let it.`;
        note.hidden = false;
        allow.hidden = false;
      }
      return;
    }
    const box = Object.assign(document.createElement('div'), { id: 'view', className: `view-${kind ?? 'bytes'}` });
    if (kind === 'picture') {
      const image = Object.assign(document.createElement('img'), { alt: path });
      image.src = URL.createObjectURL(new Blob([await read(path)], { type: typeOf(path) }));
      box.append(image);
    } else if (kind === 'markdown' || kind === 'text') {
      const bytes = await read(path);
      const text = new TextDecoder().decode(bytes.subarray(0, MAX_TEXT));
      if (kind === 'markdown') mdNodes(Object.assign(box, { className: 'view-markdown md' }), markdown(text));
      else box.append(Object.assign(document.createElement('pre'), { className: 'code', textContent: text }));
      if (bytes.length > MAX_TEXT) {
        note.textContent = 'Only the beginning shows here: download it to read it all.';
        note.hidden = false;
      }
    } else {
      box.append(Object.assign(document.createElement('p'), { className: 'note', textContent: `${path} is not a page, picture or text: download it to open it.` }));
    }
    document.body.append(box);
  } catch (error) {
    note.textContent = `This file could not be shown: ${error.message}`;
    note.hidden = false;
  }
}

document.querySelector('#view-reload').onclick = show;
document.querySelector('#view-web').onclick = () => {
  web = true;
  show();
};
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
