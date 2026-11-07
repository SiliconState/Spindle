// A file of a thread's workspace made ready to show: its type, whether it can be shown
// as a page, a picture, Markdown, text, a PDF, a document or audio and video, and a web
// page with what it refers to by relative address put in (its scripts, styles and
// pictures, and the small data files it fetches), for preview.html, a sandboxed page
// that cannot reach Spindle or its keys and, where Chrome holds it to a
// Connection-Allowlist, nothing else either (`POLICY`). view.html (a file in a tab) and
// the panel (a file in the conversation) share it.

const TYPES = {
  png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg', gif: 'image/gif', webp: 'image/webp', svg: 'image/svg+xml',
  ico: 'image/x-icon', css: 'text/css', js: 'text/javascript', mjs: 'text/javascript', json: 'application/json',
  csv: 'text/csv', tsv: 'text/tab-separated-values', txt: 'text/plain', md: 'text/markdown', xml: 'application/xml',
  geojson: 'application/geo+json', html: 'text/html', htm: 'text/html', woff2: 'font/woff2', woff: 'font/woff',
  mp4: 'video/mp4', webm: 'video/webm', mp3: 'audio/mpeg', wav: 'audio/wav', pdf: 'application/pdf',
  avif: 'image/avif', bmp: 'image/bmp', apng: 'image/apng', m4v: 'video/mp4', ogv: 'video/ogg', mov: 'video/quicktime',
  m4a: 'audio/mp4', aac: 'audio/aac', ogg: 'audio/ogg', oga: 'audio/ogg', opus: 'audio/ogg', flac: 'audio/flac',
  docx: 'application/vnd.openxmlformats-officedocument.wordprocessingml.document',
  docm: 'application/vnd.ms-word.document.macroEnabled.12',
  xlsx: 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet',
  xlsm: 'application/vnd.ms-excel.sheet.macroEnabled.12',
  pptx: 'application/vnd.openxmlformats-officedocument.presentationml.presentation',
  pptm: 'application/vnd.ms-powerpoint.presentation.macroEnabled.12',
};
export const typeOf = (name) => TYPES[name.split('.').pop().toLowerCase()] ?? 'application/octet-stream';

/** How a file shows: 'page', 'picture', 'markdown', 'text', 'pdf' (Chrome's own viewer),
 * 'document' (Word, Excel, PowerPoint: drawn as a page, documents.mjs), 'media' (audio
 * or video) or null (bytes only). */
export function kindOf(name) {
  if (/\.html?$/i.test(name)) return 'page';
  if (/\.(png|jpe?g|gif|webp|svg|avif|bmp|ico|apng)$/i.test(name)) return 'picture';
  if (/\.pdf$/i.test(name)) return 'pdf';
  if (/\.(docx|docm|xlsx|xlsm|pptx|pptm)$/i.test(name)) return 'document';
  if (/\.(mp4|m4v|webm|ogv|mov|mp3|m4a|aac|wav|ogg|oga|opus|flac)$/i.test(name)) return 'media';
  if (/\.(md|markdown)$/i.test(name)) return 'markdown';
  if (/\.(txt|css|js|mjs|cjs|ts|tsx|jsx|json|csv|tsv|xml|ya?ml|toml|ini|py|rb|go|rs|java|c|h|cpp|hpp|sh|bash|sql|log|geojson|svelte|vue)$/i.test(name)
    || !/\.[^/]+$/.test(name)) return 'text';
  return null;
}

/** Kinds that show in a preview frame, as a page does (taller when asked). */
export const FRAMED = new Set(['page', 'document']);

/** Where a thread's PDF shows: the address Spindle's service worker answers with the
 * file (background.js), opened as a tab of its own in Chrome's own PDF viewer (its name
 * last, which the viewer shows). Never in a frame of Spindle's: the viewer takes the page
 * around it to a link the user clicks in the PDF, which would take the side panel away. */
export function pdfAddress(thread, path) {
  return chrome.runtime.getURL(`file/${thread}/${path.split('/').map(encodeURIComponent).join('/')}`);
}

/** Whether PDFs show here: Spindle's service worker answers their addresses where Chrome
 * routes requests to it (123 and later, as previews need). */
export function pdfShows() {
  const brand = navigator.userAgentData?.brands?.find((entry) => /chrom/i.test(entry.brand));
  const version = Number(brand?.version ?? /Chrom(?:e|ium)\/(\d+)/.exec(navigator.userAgent)?.[1] ?? 0);
  return version >= 123;
}

/** An SVG file as a picture takes it: with the SVG namespace on its root element (a
 * picture is read as XML, and one written without it, as models often write SVG for a
 * page, draws nothing), and the xlink one where it uses xlink. */
function svgPicture(bytes) {
  const text = svgText(bytes);
  // Its root: the first tag, past any declaration, comment, instruction or doctype, as
  // written without a prefix (`<svg:svg>` has its namespace by that prefix); a `>` in
  // a quoted value does not end it.
  const lead = /^(?:\s|<\?[\s\S]*?\?>|<!--[\s\S]*?-->|<!DOCTYPE[^[>]*(?:\[[\s\S]*?\])?\s*>)*/i.exec(text)[0];
  const found = /^<svg(?=[\s/>])(?:[^>"']|"[^"]*"|'[^']*')*>/i.exec(text.slice(lead.length));
  const root = found && Object.assign(found, { index: lead.length });
  let tag = root?.[0] ?? '';
  if (root && !/\sxmlns\s*=/.test(tag)) tag = tag.replace(/^<svg/i, '<svg xmlns="http://www.w3.org/2000/svg"');
  if (root && /\bxlink:/.test(text) && !/\sxmlns:xlink\s*=/.test(tag)) tag = tag.replace(/^<svg/i, '<svg xmlns:xlink="http://www.w3.org/1999/xlink"');
  // Nothing to put right: its bytes as they are (in whatever encoding it declares).
  if (!root || tag === root[0]) return new Blob([bytes], { type: 'image/svg+xml' });
  // Written again in UTF-8, so no other encoding is declared.
  const fixed = text.slice(0, root.index) + tag + text.slice(root.index + root[0].length);
  return new Blob([fixed.replace(/^(\s*<\?xml[^>]*?)\s+encoding\s*=\s*["'][^"']*["']/i, '$1')], { type: 'image/svg+xml' });
}

/** An SVG's text, read in the encoding its XML declaration names (UTF-8 otherwise). */
function svgText(bytes) {
  const declared = /^\s*<\?xml[^>]*\sencoding\s*=\s*["']([\w.:-]+)["']/i.exec(new TextDecoder('latin1').decode(bytes.subarray(0, 200)))?.[1];
  try {
    return new TextDecoder(declared ?? 'utf-8').decode(bytes);
  } catch {
    return new TextDecoder().decode(bytes);
  }
}

/** A picture's bytes as a Blob to show (an SVG's namespace made sure of, `svgPicture`). */
export function pictureBlob(name, bytes) {
  return /\.svg$/i.test(name) ? svgPicture(bytes) : new Blob([bytes], { type: typeOf(name) });
}

/** An SVG that is no picture (its markup does not read as XML: an HTML entity, a stray
 * `&`) as a page, drawn as a browser draws SVG inside HTML, which forgives that; nothing
 * in it runs (`documentFrame`). */
export function svgPage(bytes) {
  // Its XML declaration and doctype (an internal subset too) out: HTML would show them.
  const text = svgText(bytes).replace(/^\uFEFF/, '').replace(/<\?xml[^>]*\?>/i, '')
    .replace(/<!DOCTYPE[^[>]*(?:\[[\s\S]*?\])?\s*>/i, '');
  return '<!doctype html>\n<html><head><meta charset="utf-8"><style>html,body{margin:0;background:#fff}'
    + 'body>svg{display:block;max-width:100%;height:auto;margin:0 auto}</style></head><body>' + text + '</body></html>';
}

/** A frame showing `html`, a page made here (a document drawn, an SVG), static: no
 * script, no frame, no link out, nothing from the web (`pageOf`'s 'static'). */
export async function documentFrame(html, title) {
  const page = await pageOf({ path: 'document.html', files: [], read: async () => new Uint8Array(), html, contained: false });
  return previewFrame(page, title);
}

/** How a page shows (`pageOf`'s `mode`), and the policy it shows under.
 *
 * A page's policy (CSP) cannot keep its scripts from sending what it shows out of the
 * browser: not through WebRTC, a resource hint (a `prerender` link fetches its address
 * whole) or a navigation (the browser looks the host up, and connects, before the frame
 * refuses it). Chrome (152 and later) can: a frame served under a Connection-Allowlist
 * makes no connection the list does not name, of any kind, nor do the frames and workers
 * it makes. Spindle's service worker serves preview.html so (background.js), and
 * preview.js tells whether it holds there (`containment`).
 * - 'contained': its scripts run, and it connects to nothing (`SHUT`: what it refers to
 *   in the workspace is put in). How a page with scripts shows where previews can be
 *   contained.
 * - 'web': it reaches the web over HTTPS too (`WEB`), as the user let this version of it.
 * - 'scripts': where previews cannot be contained, its scripts run as the user let this
 *   version of it (`SHUT`, which keeps it from the web but not from WebRTC and the like).
 * - 'static': nothing runs (`STATIC`): a page without scripts, or one where previews
 *   cannot be contained, until the user lets it (no link leaves it then either).
 * No form goes anywhere. The sandbox's own policy (manifest.json) is the most it may ever
 * do; a page's narrows it, first thing in the page, before any of its scripts. */
const STATIC = "default-src 'none'; script-src 'none'; style-src 'unsafe-inline' data: blob:; img-src data: blob:; "
  + "font-src data: blob:; media-src data: blob:; frame-src 'none'; form-action 'none'; base-uri 'none'";
const SHUT = "default-src 'none'; script-src 'unsafe-inline' 'unsafe-eval' data: blob:; style-src 'unsafe-inline' data: blob:; "
  + "img-src data: blob:; font-src data: blob:; media-src data: blob:; connect-src data: blob:; worker-src data: blob:; "
  + "frame-src data: blob:; form-action 'none'; base-uri 'none'";
const WEB = "default-src 'none'; script-src 'unsafe-inline' 'unsafe-eval' https: data: blob:; style-src 'unsafe-inline' https: data: blob:; "
  + 'img-src https: data: blob:; font-src https: data: blob:; media-src https: data: blob:; connect-src https: data: blob:; '
  + "worker-src data: blob:; frame-src https: data: blob:; form-action 'none'; base-uri 'none'";
const POLICY = { contained: SHUT, web: WEB, scripts: SHUT, static: STATIC };

/** Data a page may fetch: small text files (by relative address), within a whole read
 * for it. */
const DATA = /\.(json|csv|tsv|txt|md|xml|geojson|svg|html?)$/i;
const MAX_DATA_FILE = 1024 * 1024;
const MAX_DATA = 8 * 1024 * 1024;
/** Pictures and media put into a page, each at most this big. */
const MAX_EMBED = 16 * 1024 * 1024;

export function base64(bytes) {
  let binary = '';
  for (let at = 0; at < bytes.length; at += 0x8000) binary += String.fromCharCode(...bytes.subarray(at, at + 0x8000));
  return btoa(binary);
}

/** A hex SHA-256 of `text`. */
async function digestOf(text) {
  const bytes = new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text)));
  return [...bytes].map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

const hex = (buffer) => [...new Uint8Array(buffer)].map((byte) => byte.toString(16).padStart(2, '0')).join('');

/** The digest a workspace lists a file's bytes under (Spindle's `object_digest`): what a
 * file read here is checked against, so what runs is what was listed. */
async function objectDigest(bytes) {
  const head = new TextEncoder().encode('spindle-object-v1\0');
  const all = new Uint8Array(head.length + 8 + bytes.length);
  all.set(head);
  new DataView(all.buffer).setBigUint64(head.length, BigInt(bytes.length));
  all.set(bytes, head.length + 8);
  return hex(await crypto.subtle.digest('SHA-256', all));
}

/** Hints that reach a host before anything asks for it (a preconnect looks a name up and
 * connects, scripts or not). */
const HINTS = /^(preconnect|dns-prefetch|prefetch|prerender|preload|modulepreload)$/i;
/** Whether following a link to `ref` leaves the page for a host (not within it, nor
 * mail, a call or data it holds), as the browser reads the address against the frame's
 * own (an extension page's: `//name` stays an extension's): it drops tabs and line breaks
 * anywhere, and control characters and spaces around it, first. */
const HERE = 'chrome-extension://spindle/preview.html';
const LOCAL = new Set(['chrome-extension:', 'mailto:', 'tel:', 'sms:', 'data:', 'blob:', 'about:', 'javascript:']);
function leaves(ref) {
  try {
    return !LOCAL.has(new URL(ref, HERE).protocol);
  } catch {
    // An address the browser cannot read goes nowhere.
    return false;
  }
}

/** Every element under `root` that `selector` matches, those in its templates too: a
 * declarative shadow root is a template the frame puts in the page as written. */
function everywhere(root, selector) {
  const found = [];
  const visit = (node) => {
    found.push(...node.querySelectorAll(selector));
    for (const template of node.querySelectorAll('template')) visit(template.content);
  };
  visit(root);
  return found;
}

/** Takes out of `doc` what would reach a host by itself: resource hints, refreshes (a
 * frame refuses the navigation, but the browser looks the host up first) and a page's
 * own say over DNS prefetching (one that turns it off may stay); unless it `runs`, its
 * scripts (their noscript fallbacks shown) and frames (one written in place would run on
 * its own); and with `links`, the addresses links leave by (kept as their title), their
 * pings, and animations that would set one. Whether it took anything out. */
function scrub(doc, { runs = true, links = false } = {}) {
  let took = false;
  const drop = (node) => {
    node.remove();
    took = true;
  };
  for (const meta of everywhere(doc, 'meta[http-equiv]')) {
    const kind = meta.getAttribute('http-equiv').trim().toLowerCase();
    if (kind === 'refresh' || (kind === 'x-dns-prefetch-control' && meta.getAttribute('content')?.trim().toLowerCase() !== 'off')) drop(meta);
  }
  for (const link of everywhere(doc, 'link[rel]')) {
    if (link.getAttribute('rel').split(/\s+/).some((rel) => HINTS.test(rel))) drop(link);
  }
  if (!runs) {
    for (const script of everywhere(doc, 'script')) drop(script);
    for (const fallback of everywhere(doc, 'noscript')) {
      fallback.replaceWith(...fallback.childNodes);
      took = true;
    }
    for (const frame of everywhere(doc, 'iframe, frame')) drop(frame);
  }
  if (links) {
    for (const link of everywhere(doc, 'a, area')) {
      if (link.hasAttribute('ping')) {
        link.removeAttribute('ping');
        took = true;
      }
      for (const name of ['href', 'xlink:href']) {
        const ref = link.getAttribute(name);
        if (ref == null || !leaves(ref)) continue;
        if (!link.hasAttribute('title')) link.setAttribute('title', ref.trim());
        link.removeAttribute(name);
        took = true;
      }
    }
    for (const animation of everywhere(doc, 'set, animate')) {
      if (/^(?:xlink:)?href$/i.test(animation.getAttribute('attributeName')?.trim() ?? '')) drop(animation);
    }
  }
  return took;
}

/** How many times a page is read again, as its frame will read it, to find markup that
 * reads otherwise the second time (a form in MathML, say) before it shows as text. */
const READINGS = 4;

/** The page at `path` (`html`: its text, if already at hand) with what it refers to by
 * relative address put in: {html, mode, held, sites, scripted, serve, version, lapsed}.
 * `mode` (`POLICY`): how it shows, given whether its frame can be `contained` and what
 * `allowed` says (`{scripts, web, version}`, a choice the user made: `web` lets it reach
 * the web over HTTPS, `scripts` lets it run where it cannot be contained). A choice holds
 * for the `version` it was made for only: the page, the bytes of its own scripts and
 * stylesheets, and the data files it or its scripts name (the only ones it is given);
 * `lapsed`: the choice (`'web'` or `'scripts'`) made for another one. `version` is always
 * told: a choice is offered, and kept, for it (one that takes nothing away holds without
 * one). `held`: it is made to show only in a frame that is contained (it runs, or keeps
 * its links). `sites`: those it seems to load from; `scripted`: whether
 * it has anything to run; `serve(name)`: the text of a data file its scripts fetch (null:
 * not given to it), for `previewFrame`. `files` lists the workspace ({path, size,
 * digest}); `read(path)` gives a file's bytes, checked against its digest. */
export async function pageOf({ path, files, read, html = null, allowed = null, contained = false }) {
  const known = new Map(files.map((file) => [file.path, file]));
  const dir = path.includes('/') ? path.slice(0, path.lastIndexOf('/') + 1) : '';
  const local = (ref) => {
    if (!ref || /^([a-z][a-z0-9+.-]*:|\/\/|#)/i.test(ref.trim())) return null;
    try {
      const name = decodeURIComponent(new URL(ref.trim(), `https://workspace.invalid/${dir}`).pathname.slice(1));
      return known.has(name) ? name : null;
    } catch {
      return null;
    }
  };
  const text = (bytes) => new TextDecoder().decode(bytes);
  /** A workspace file's bytes, as listed (null: it changed since, or was never listed). */
  const checked = async (name) => {
    const bytes = await read(name);
    const listed = known.get(name)?.digest;
    return listed && (await objectDigest(bytes)) === listed ? bytes : null;
  };
  const source = html ?? text(await read(path));
  // Its inline handlers (onclick=...) are read here under a name of no meaning, and get
  // their own back only in the page made (`page`): Edge checks one against the policy of
  // the page reading it (the panel's or the file tab's, which run none) as soon as it is read, and lists
  // each as an error of Spindle's, though nothing here would ever run it. The mark is
  // taken out of all the text again, so the page is as its frame would read it unmarked.
  const mark = `data-spindle-${hex(crypto.getRandomValues(new Uint8Array(6)))}-`;
  const doc = new DOMParser().parseFromString(source.replace(/([\s/"'])(on[a-z]+\s*=)/gi, `$1${mark}$2`), 'text/html');
  const all = (selector) => everywhere(doc, selector);
  // What could run: scripts, event handlers, javascript: addresses.
  const scripted = all('script').length > 0 || all('*').some((element) =>
    [...element.attributes].some((attribute) => attribute.name.startsWith(mark) || /^on/i.test(attribute.name) || /^\s*javascript:/i.test(attribute.value)));
  // The sites it would load from (by address in its elements, styles and scripts): as
  // written, and as its own files put in say.
  const sites = new Set();
  const site = (ref) => {
    const found = /^\s*(?:https?:)?\/\/([^/?#\s'"`)]+)/i.exec(ref ?? '');
    if (found) sites.add(found[1].toLowerCase());
  };
  const scan = () => {
    for (const element of doc.querySelectorAll('[src], link[href], image[href]')) site(element.getAttribute('src') ?? element.getAttribute('href'));
    for (const code of doc.querySelectorAll('style, script')) {
      for (const found of code.textContent.matchAll(/(?:https?:)?\/\/[a-z0-9-]+(?:\.[a-z0-9-]+)+(?::\d+)?/gi)) site(found[0]);
    }
  };
  // Nothing reaches a host by itself (`scrub`).
  scrub(doc);
  scan();
  // Its scripts and stylesheets, as its version counts them: their bytes, read once and
  // put in as read.
  const inlined = new Map();
  const styles = new Map();
  let whole = true;
  for (const script of doc.querySelectorAll('script[src]')) {
    const name = local(script.getAttribute('src'));
    if (!name || inlined.has(name)) continue;
    const bytes = await checked(name);
    if (!bytes) whole = false;
    else inlined.set(name, bytes);
  }
  for (const link of doc.querySelectorAll('link[rel~="stylesheet"][href]')) {
    const name = local(link.getAttribute('href'));
    if (name && !styles.has(name)) styles.set(name, await read(name));
  }
  // The data files it may fetch: those its page or its scripts name.
  const said = [source, ...[...inlined.values()].map(text)].join('\n');
  const named = files.filter((file) => DATA.test(file.path) && file.size <= MAX_DATA_FILE && file.path !== path && said.includes(file.path.split('/').pop()));
  const version = await digestOf([
    source,
    ...[...inlined].map(([name, bytes]) => `${name}\0${bytes.length}\0${text(bytes)}`),
    ...[...styles].map(([name, bytes]) => `style\0${name}\0${bytes.length}\0${text(bytes)}`),
    ...named.map((file) => `${file.path}\0${file.digest}`),
  ].join('\0'));
  // A script of its own not as listed (changed since): nothing runs. What the user let it
  // do holds for the version it names only; a choice that gives it nothing holds for any.
  const holds = allowed?.version === version || !(allowed?.scripts || allowed?.web);
  let mode = !whole ? 'static'
    : allowed?.web && holds ? 'web'
    : scripted && contained ? 'contained'
    : scripted && allowed?.scripts && holds ? 'scripts'
    : 'static';
  const lapsed = whole && !holds ? (allowed.web ? 'web' : allowed.scripts && !contained ? 'scripts' : null) : null;
  let runs = scripted && mode !== 'static';
  // Where nothing holds its frame back, no link leaves it either: the browser looks a
  // link's host up when it is followed, before the frame refuses to go.
  const links = !contained && mode !== 'web';
  scrub(doc, { runs, links });
  for (const script of doc.querySelectorAll('script[src]')) {
    const name = local(script.getAttribute('src'));
    if (!name || !inlined.has(name)) continue;
    script.removeAttribute('src');
    script.textContent = text(inlined.get(name)).replace(/<\/script/gi, '<\\/script');
  }
  for (const link of doc.querySelectorAll('link[rel~="stylesheet"][href]')) {
    const name = local(link.getAttribute('href'));
    if (!name) continue;
    const style = doc.createElement('style');
    // A `</style` in it would end the element, and what follows would be markup.
    style.textContent = text(styles.get(name)).replace(/<\/style/gi, '<\\/style');
    link.replaceWith(style);
  }
  for (const element of doc.querySelectorAll('img[src], source[src], video[src], audio[src], link[rel~="icon"][href], image[href]')) {
    const attribute = element.hasAttribute('src') ? 'src' : 'href';
    const name = local(element.getAttribute(attribute));
    if (!name || known.get(name).size > MAX_EMBED) continue;
    element.setAttribute(attribute, `data:${typeOf(name)};base64,${base64(await read(name))}`);
  }
  // The files its scripts fetch by relative address, from the workspace, read when they
  // ask (a workspace may hold many a page never reads): only those it names, which its
  // version counts, each as listed.
  const data = new Map(runs ? named.map((file) => [file.path, file.size]) : []);
  const served = new Map();
  let total = 0;
  const serve = async (name) => {
    if (!data.has(name)) return null;
    if (!served.has(name)) {
      if (total + data.get(name) > MAX_DATA) return null;
      total += data.get(name);
      served.set(name, checked(name).then((bytes) => (bytes ? text(bytes) : null)));
    }
    return served.get(name);
  };
  scan();
  const shim = doc.createElement('script');
  // The page asks its parent for a file it fetches (the parent gives only what `serve`
  // does); its answer comes back by id.
  shim.textContent = `(() => {
  const types = ${JSON.stringify(Object.fromEntries([...data.keys()].map((name) => [name, typeOf(name)]))).replace(/</g, '\\u003c')};
  const dir = ${JSON.stringify(dir).replace(/</g, '\\u003c')};
  const real = window.fetch.bind(window);
  const asked = new Map();
  let next = 0;
  addEventListener('message', ({ data, source }) => {
    if (source !== parent || typeof data?.spindleFile !== 'number' || !asked.has(data.spindleFile)) return;
    asked.get(data.spindleFile)(data.text);
    asked.delete(data.spindleFile);
  });
  const file = (name) => new Promise((done) => {
    asked.set(++next, done);
    parent.postMessage({ spindleFetch: next, name }, '*');
  });
  window.fetch = (input, init) => {
    const ref = typeof input === 'string' ? input : input?.url ?? String(input);
    if (!/^([a-z][a-z0-9+.-]*:|\\/\\/)/i.test(ref)) {
      let name = null;
      try {
        name = decodeURIComponent(new URL(ref, 'https://workspace.invalid/' + dir).pathname.slice(1));
      } catch {}
      if (name != null && Object.hasOwn(types, name)) {
        return file(name).then((text) => {
          if (typeof text !== 'string') throw new TypeError('Failed to fetch ' + ref + ': not given to this preview');
          return new Response(text, { headers: { 'content-type': types[name] } });
        });
      }
    }
    return real(input, init);
  };
})();`;
  const policy = doc.createElement('meta');
  policy.httpEquiv = 'Content-Security-Policy';
  policy.content = POLICY[mode];
  // First of all: no name is looked up ahead of a fetch (it cannot be turned on again).
  const prefetch = doc.createElement('meta');
  prefetch.httpEquiv = 'x-dns-prefetch-control';
  prefetch.content = 'off';
  (doc.head ?? doc.documentElement).prepend(...(runs ? [prefetch, policy, shim] : [prefetch, policy]));
  // Read again as its frame will read it: what reads otherwise the second time is scrubbed
  // again, until nothing more comes out; a page that never settles shows as its text.
  let page = `<!doctype html>\n${doc.documentElement.outerHTML}`;
  for (let reading = 0; ; reading++) {
    const again = new DOMParser().parseFromString(page, 'text/html');
    if (!scrub(again, { runs, links })) break;
    if (reading === READINGS) {
      page = asText(source);
      mode = 'static';
      runs = false;
      break;
    }
    page = `<!doctype html>\n${again.documentElement.outerHTML}`;
  }
  page = page.replaceAll(mark, '');
  const held = contained && mode !== 'web';
  return { html: page, mode, held, sites: [...sites].sort(), scripted, serve: runs ? serve : async () => null, version, lapsed };
}

/** A page shown as its own text, nothing in it read as markup. */
function asText(source) {
  const escaped = source.replace(/&/g, '&amp;').replace(/</g, '&lt;');
  return `<!doctype html>\n<html><head><meta http-equiv="x-dns-prefetch-control" content="off"><meta http-equiv="Content-Security-Policy" content="${STATIC}"></head><body><pre style="white-space: pre-wrap">${escaped}</pre></body></html>`;
}

/** Preview frames (preview.html) and what each one is answered by: `ready`, once it says
 * whether it is contained; then its page's fetches (`serve`) and `shown`. */
const frames = new Set();
let listening = false;

/** Lets go of frames gone from the document (or never put in it, a while after they
 * were made). */
function sweep() {
  for (const entry of frames) {
    if (!entry.frame.isConnected && (entry.loaded || Date.now() - entry.made > 60_000)) frames.delete(entry);
  }
}

/** What frames say to this document: that one is ready, and whether it is contained
 * (told once, before any page is in it), a file its page fetches (answered by that
 * frame's `serve`, by no other's), and that its page was written in (told once). */
function listen() {
  if (listening) return;
  listening = true;
  addEventListener('message', ({ data, source }) => {
    sweep();
    for (const entry of frames) {
      if (!source || entry.frame.contentWindow !== source) continue;
      if (data?.spindleReady === true) {
        const ready = entry.ready;
        entry.ready = null;
        entry.loaded = true;
        ready?.(data.contained === true);
      } else if (data?.spindleShown === true) {
        const shown = entry.shown;
        entry.shown = null;
        shown?.();
      } else if (typeof data?.spindleFetch === 'number' && typeof data.name === 'string') {
        Promise.resolve(entry.serve?.(data.name)).catch(() => null)
          .then((text) => source.postMessage({ spindleFile: data.spindleFetch, text: typeof text === 'string' ? text : null }, '*'));
      }
      return;
    }
  });
}

/** A frame loading `src` (preview.html) once it is put in the document, answered by
 * `handlers` (`ready(contained)`, `serve`, `shown`). */
function frameOf(src, title, handlers) {
  listen();
  sweep();
  const frame = document.createElement('iframe');
  frame.title = title;
  // Sandboxed as preview.html's own policy sandboxes it (manifest.json), whatever the frame
  // goes to after (that policy goes with its document; this stays with the frame): no
  // popup, form, download or navigation of the page around it. Modals only so that it
  // may print (Chrome counts printing as one); preview.js keeps the page's alert,
  // confirm, prompt and "Leave site?" as they were without it (nothing shows), and its
  // printing to what the user asks. Its origin is that policy's to take away
  // (preview.html's is opaque): Chrome does not send a frame sandboxed without
  // allow-same-origin through Spindle's service worker, so it would never be contained
  // (`containment`).
  frame.setAttribute('sandbox', 'allow-scripts allow-same-origin allow-modals');
  frames.add({ frame, ...handlers, loaded: false, made: Date.now() });
  frame.src = src;
  return frame;
}

/** How long a frame may take to say it is ready (Spindle's service worker may be
 * starting) before it counts as not contained. */
const READY_MS = 10_000;
/** How long previews count as not contained once a frame showing a page said so, or never
 * said (its service worker slow to start, say), before they are asked again. */
const DOUBT_MS = 60_000;
let containing = null;
let doubtedUntil = 0;

/** Whether previews here are contained (see `POLICY`): asked of a hidden frame of its
 * own, once, and again after a no (Spindle's service worker may not have been serving
 * yet); a while after a frame showing a page said otherwise (`previewFrame`), no. */
export function containment() {
  if (Date.now() < doubtedUntil) return Promise.resolve(false);
  containing ??= new Promise((resolve) => {
    const done = (contained) => {
      clearTimeout(timer);
      frame.remove();
      resolve(contained);
    };
    const frame = frameOf('preview.html', 'Spindle preview check', { ready: done });
    const timer = setTimeout(() => done(false), READY_MS);
    frame.hidden = true;
    document.body.append(frame);
  }).then((contained) => {
    if (!contained) containing = null;
    return contained;
  });
  return containing;
}

/** A frame showing `page` (pageOf's) in preview.html, served to reach the web for a page
 * let to, else to reach nothing. The page goes in once the frame is ready, and one made
 * for a contained frame (`held`) only into a frame that is one: else (or when the frame
 * never says, once it is in the document) `refused` is told, and previews here count as
 * not contained for a while (`DOUBT_MS`). Its fetches of workspace files are answered by
 * its `serve`, and `shown` is told once it is written in. */
export function previewFrame(page, title, { shown = null, refused = null } = {}) {
  const needs = page.held;
  let timer = 0;
  let answered = false;
  const ready = (contained) => {
    if (answered) return;
    answered = true;
    clearTimeout(timer);
    if (needs && !contained) {
      doubtedUntil = Date.now() + DOUBT_MS;
      containing = null;
      refused?.();
      return;
    }
    frame.contentWindow?.postMessage({ html: page.html, contained: needs }, '*');
  };
  const frame = frameOf(page.mode === 'web' ? 'preview.html?reach=web' : 'preview.html', title, { serve: page.serve, shown, ready });
  // A frame in the document that never says it is ready is not contained (one never
  // put in it is let be).
  if (needs) timer = setTimeout(() => frame.isConnected && ready(false), READY_MS);
  return frame;
}

/** What a page's preview leaves out, said in `note` (a paragraph, emptied first) with
 * links that change it for this version of the page (`change(choice)`: the caller shows
 * it, and keeps it only for the version it names, the one shown). Previews stay offline
 * unless the user lets a page reach the web; where they cannot be `contained`, a page's
 * scripts wait for the user too (`POLICY`). False when there is nothing to say. */
export function previewNote(note, page, contained, change) {
  const sites = page.sites.slice(0, 3).join(', ') + (page.sites.length > 3 ? ` and ${page.sites.length - 3} more` : '');
  const said = [];
  const offers = [];
  const offer = (label, title, choice) => offers.push({ label, title, choice });
  if (page.mode === 'web') {
    said.push(`It loads from ${sites || 'the web'}.`);
    offer('Go offline', 'Show this version offline again', { scripts: false, web: false });
  } else {
    if (page.scripted && page.mode === 'scripts') {
      said.push('Its scripts run, and this browser cannot keep them offline.');
      offer('Stop them', 'Show this version without running its scripts', { scripts: false, web: false });
    } else if (page.scripted && !contained) {
      said.push(page.lapsed === 'scripts' ? 'It changed since you ran its scripts.' : 'Its scripts are off: this browser cannot keep a running page offline.');
      offer('Run them', "Run this version's scripts here: they could send what the page shows out of the browser", { scripts: true, version: page.version });
    }
    if (page.sites.length) {
      said.push(`It ${page.lapsed === 'web' ? 'changed since you let it load' : 'would load'} from ${sites}; previews stay offline.`);
      offer('Load from the web', contained
        ? 'Let this version load from the web over HTTPS: it could then send what it shows to sites on the web (never to Spindle)'
        : "Let this version load from the web over HTTPS and run its scripts here: they could send what the page shows out of the browser (never to Spindle)", { scripts: true, web: true, version: page.version });
    }
  }
  note.replaceChildren(`${said.join(' ')} `);
  offers.forEach(({ label, title, choice }, at) => {
    const link = Object.assign(document.createElement('button'), { type: 'button', className: 'link-like', textContent: label, title });
    link.onclick = () => change(choice);
    note.append(...(at ? [' · ', link] : [link]));
  });
  return said.length > 0;
}

/** A file name Chrome's downloads take: no separators, reserved or control characters,
 * no leading dot (`.env` -> `_env`), no trailing dot or space, no name Windows keeps. */
export function downloadName(name) {
  let safe = name.replace(/[\\/:*?"<>|\x00-\x1f\x7f]+/g, '_').replace(/^\.+/, '_').replace(/[.\s]+$/, '');
  if (/^(con|prn|aux|nul|com\d|lpt\d)(\.|$)/i.test(safe)) safe = `_${safe}`;
  return safe.trim() || 'download';
}

/** Saves `bytes` as `name` in the user's downloads: through Chrome's downloads API (a
 * download a page starts after an await may be held back otherwise), else (no API, or
 * a name it still refuses) a link. */
export async function saveFile(bytes, name, type = 'application/octet-stream') {
  const url = URL.createObjectURL(new Blob([bytes], { type }));
  try {
    if (chrome?.downloads?.download) {
      try {
        await chrome.downloads.download({ url, filename: downloadName(name), saveAs: false });
        return;
      } catch (error) {
        console.warn(`Chrome's downloads did not take ${name}:`, error);
      }
    }
    const link = Object.assign(document.createElement('a'), { href: url, download: downloadName(name) });
    document.body.append(link);
    link.click();
    link.remove();
  } finally {
    setTimeout(() => URL.revokeObjectURL(url), 60_000);
  }
}
