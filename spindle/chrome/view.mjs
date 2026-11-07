// A file of a thread's workspace, opened in a tab: a picture as it is, Markdown drawn,
// text as text, a web page as a page (page.mjs puts in what it refers to from the
// workspace; it runs in a sandboxed frame, preview.html, with no way to Spindle or its
// keys, and reaches the web only when the user lets it), a PDF as Chrome's own viewer
// (the tab becomes it), a Word, Excel or PowerPoint document drawn as a page
// (documents.mjs; nothing in it runs), audio and video in a player, anything else to
// download.
import { keepPreviewChoice, previewChoice, startEngine, REQUESTS, answersFor } from './shared.mjs';
import { containment, documentFrame, kindOf, pageOf, pdfAddress, pdfShows, pictureBlob, previewFrame, previewNote, saveFile, svgPage, typeOf } from './page.mjs';
import { MAX_DOCUMENT, documentPage } from './documents.mjs';
import { BROWSER } from './page-tools.mjs';
import { mdNodes } from './markdown.mjs';
import { markdown } from './text.js';
import { shownName, shownPath } from './steps.mjs';

const query = new URLSearchParams(location.search);
const thread = query.get('thread') ?? '';
const path = query.get('path') ?? '';
const me = crypto.randomUUID();
// Requests go to the engine alone, and its answers come back to this page alone (it
// takes no events: the shared channel's traffic never reaches it).
const requests = new BroadcastChannel(REQUESTS);
const answers = new BroadcastChannel(answersFor(me));
const waiting = new Map();
let next = 0;

answers.onmessage = ({ data }) => {
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
    requests.postMessage({ to: 'engine', from: me, id, op, args });
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

/** The object address of what shows now (a picture, a player's), let go of when
 * something else shows. */
let shownUrl = null;

/** What is shown, in place of what was (one view, whatever came before; a frame left
 * outside the box is taken out too). */
function commit(view, url = null) {
  for (const stray of document.querySelectorAll('#view')) if (stray.parentElement?.id !== 'view-box') stray.remove();
  document.querySelector('#view-box').replaceChildren(...(view ? [view] : []));
  // A page (a document drawn as one) prints itself, asked by Print or Ctrl+P (preview.js):
  // the tab around it would print only the part of it in sight. Told the key is its once
  // its frame has loaded (it keeps that through the page being written in).
  const framed = view instanceof HTMLIFrameElement;
  document.querySelector('#view-print').hidden = !framed;
  if (framed) view.addEventListener('load', () => view.contentWindow?.postMessage({ spindleKeys: true }, '*'));
  if (shownUrl) URL.revokeObjectURL(shownUrl);
  shownUrl = url;
}

/** A frame as the view (a document's, an SVG's drawn as a page). */
const asView = (frame) => Object.assign(frame, { id: 'view' });

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
    if (kind === 'pdf') {
      if (!pdfShows()) throw new Error(`PDFs open here in ${BROWSER} 123 and later: download it to open it`);
      // The tab becomes Chrome's own viewer of it (from Spindle's service worker,
      // background.js), with its own download, print and search: a link clicked in the
      // PDF goes on in this tab, as any PDF's does. Where Chrome is set to download PDFs
      // instead, it saves it and this note stays.
      note.textContent = `Opening ${shownName(path)} in ${BROWSER}'s PDF viewer… (if ${BROWSER} is set to download PDFs, it is in your downloads instead).`;
      note.hidden = false;
      commit(null);
      location.replace(pdfAddress(thread, path));
      return;
    }
    if (kind === 'document') {
      // Too large to draw: said before its bytes are read at all.
      if (file.size > MAX_DOCUMENT) throw new Error(`it is ${Math.round(file.size / 1048576)} MB: documents up to ${MAX_DOCUMENT / 1048576} MB show here; download it to open it`);
      const { html, notes } = await documentPage(path, await read(path));
      const frame = await documentFrame(html, path);
      if (!newest()) return;
      commit(asView(frame));
      if (notes.length) {
        note.textContent = `${notes.join(' ')} Download it for the document as it is.`;
        note.hidden = false;
      }
      return;
    }
    const box = Object.assign(document.createElement('div'), { id: 'view', className: `view-${kind ?? 'bytes'}` });
    let cut = false;
    let url = null;
    if (kind === 'picture') {
      const bytes = await read(path);
      const image = Object.assign(document.createElement('img'), { alt: path });
      // An SVG whose markup a picture cannot read shows as a page instead.
      if (/\.svg$/i.test(path)) {
        image.onerror = async () => {
          if (!newest() || !image.isConnected) return;
          const frame = await documentFrame(svgPage(bytes), path);
          if (newest() && image.isConnected) commit(asView(frame));
        };
      }
      url = URL.createObjectURL(pictureBlob(path, bytes));
      image.src = url;
      box.append(image);
    } else if (kind === 'media') {
      const type = typeOf(path);
      const player = Object.assign(document.createElement(type.startsWith('audio/') ? 'audio' : 'video'), { controls: true, preload: 'metadata' });
      player.setAttribute('aria-label', path);
      player.onerror = () => {
        if (!newest()) return;
        note.textContent = 'This browser cannot play it: download it to open it.';
        note.hidden = false;
      };
      url = URL.createObjectURL(new Blob([await read(path)], { type }));
      player.src = url;
      box.append(player);
    } else if (kind === 'markdown' || kind === 'text') {
      const bytes = await read(path);
      const text = new TextDecoder().decode(bytes.subarray(0, MAX_TEXT));
      if (kind === 'markdown') mdNodes(Object.assign(box, { className: 'view-markdown md' }), markdown(text));
      else box.append(Object.assign(document.createElement('pre'), { className: 'code', textContent: text }));
      cut = bytes.length > MAX_TEXT;
    } else {
      box.append(Object.assign(document.createElement('p'), { className: 'note', textContent: `${path} is not a page, picture, document or text: download it to open it.` }));
    }
    if (!newest()) {
      if (url) URL.revokeObjectURL(url);
      return;
    }
    commit(box, url);
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
/** Prints what shows, when it is a page (its frame prints itself: preview.js). */
const printView = () => document.querySelector('iframe#view')?.contentWindow?.postMessage({ spindlePrint: true }, '*');
// Ctrl+P (⌘P on a Mac; keyCode: the P key on any layout, as preview.js) pressed here, not
// in the frame (which hears it itself), prints the page too, not this tab.
const MAC = /Mac/i.test(navigator.userAgentData?.platform ?? navigator.platform);
const printButton = document.querySelector('#view-print');
printButton.onclick = printView;
printButton.title += ` (${MAC ? '⌘P' : 'Ctrl+P'})`;
addEventListener('keydown', (event) => {
  const key = event.keyCode === 80 && !event.altKey && !event.shiftKey
    && (MAC ? event.metaKey && !event.ctrlKey : event.ctrlKey && !event.metaKey);
  if (!key || printButton.hidden) return;
  event.preventDefault();
  printView();
});
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
