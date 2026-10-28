// A file of a thread's workspace made ready to show: its type, whether it can be shown
// as a page, a picture, Markdown or text, and a web page with what it refers to by
// relative address put in (its scripts, styles and pictures, and the small data files
// it fetches), for preview.html, a sandboxed page that cannot reach Spindle or its keys.
// view.html (a file in a tab) and the panel (a file in the conversation) share it.

const TYPES = {
  png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg', gif: 'image/gif', webp: 'image/webp', svg: 'image/svg+xml',
  ico: 'image/x-icon', css: 'text/css', js: 'text/javascript', mjs: 'text/javascript', json: 'application/json',
  csv: 'text/csv', tsv: 'text/tab-separated-values', txt: 'text/plain', md: 'text/markdown', xml: 'application/xml',
  geojson: 'application/geo+json', html: 'text/html', htm: 'text/html', woff2: 'font/woff2', woff: 'font/woff',
  mp4: 'video/mp4', webm: 'video/webm', mp3: 'audio/mpeg', wav: 'audio/wav', pdf: 'application/pdf',
};
export const typeOf = (name) => TYPES[name.split('.').pop().toLowerCase()] ?? 'application/octet-stream';

/** How a file shows: 'page', 'picture', 'markdown', 'text' or null (bytes only). */
export function kindOf(name) {
  if (/\.html?$/i.test(name)) return 'page';
  if (/\.(png|jpe?g|gif|webp|svg)$/i.test(name)) return 'picture';
  if (/\.(md|markdown)$/i.test(name)) return 'markdown';
  if (/\.(txt|css|js|mjs|cjs|ts|tsx|jsx|json|csv|tsv|xml|ya?ml|toml|ini|py|rb|go|rs|java|c|h|cpp|hpp|sh|bash|sql|log|geojson|svelte|vue)$/i.test(name)
    || !/\.[^/]+$/.test(name)) return 'text';
  return null;
}

/** What a page may reach from its frame: nothing past itself (what it refers to in the
 * workspace is put in), or, when the user lets it, the web over HTTPS too. Either way no
 * form goes anywhere. The sandbox's own policy (manifest.json) is the most it may ever
 * do; a page's narrows it, first thing in the page, before any of its scripts. */
const SHUT = "default-src 'none'; script-src 'unsafe-inline' 'unsafe-eval' data: blob:; style-src 'unsafe-inline' data: blob:; "
  + "img-src data: blob:; font-src data: blob:; media-src data: blob:; connect-src data: blob:; worker-src data: blob:; "
  + "frame-src data: blob:; form-action 'none'; base-uri 'none'";
const WEB = "default-src 'none'; script-src 'unsafe-inline' 'unsafe-eval' https: data: blob:; style-src 'unsafe-inline' https: data: blob:; "
  + 'img-src https: data: blob:; font-src https: data: blob:; media-src https: data: blob:; connect-src https: data: blob:; '
  + "worker-src data: blob:; frame-src https: data: blob:; form-action 'none'; base-uri 'none'";

/** Data a page may fetch: small text files (by relative address), within a whole. */
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

/** The page at `path` (`html`: its text, if already at hand) with what it refers to by
 * relative address put in: {html, web}, `web` the sites it seems to load from (closed
 * to it unless `web`). `files` lists the workspace ({path, size}); `read(path)` gives a
 * file's bytes. */
export async function pageOf({ path, files, read, html = null, web = false }) {
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
  const doc = new DOMParser().parseFromString(html ?? text(await read(path)), 'text/html');
  for (const script of doc.querySelectorAll('script[src]')) {
    const name = local(script.getAttribute('src'));
    if (!name) continue;
    script.removeAttribute('src');
    script.textContent = text(await read(name)).replace(/<\/script/gi, '<\\/script');
  }
  for (const link of doc.querySelectorAll('link[rel~="stylesheet"][href]')) {
    const name = local(link.getAttribute('href'));
    if (!name) continue;
    const style = doc.createElement('style');
    style.textContent = text(await read(name));
    link.replaceWith(style);
  }
  for (const element of doc.querySelectorAll('img[src], source[src], video[src], audio[src], link[rel~="icon"][href], image[href]')) {
    const attribute = element.hasAttribute('src') ? 'src' : 'href';
    const name = local(element.getAttribute(attribute));
    if (!name || known.get(name).size > MAX_EMBED) continue;
    element.setAttribute(attribute, `data:${typeOf(name)};base64,${base64(await read(name))}`);
  }
  // What the page fetches by relative address, from the workspace. A page that may reach
  // the web gets only the files it names (its markup or scripts mention the file's name),
  // so it cannot send away unrelated ones; one shut off from the web may have any.
  const said = web ? doc.documentElement.outerHTML : '';
  const named = (path) => !web || said.includes(path.split('/').pop());
  const data = {};
  let total = 0;
  for (const file of files) {
    if (!DATA.test(file.path) || file.size > MAX_DATA_FILE || total + file.size > MAX_DATA || !named(file.path)) continue;
    data[file.path] = text(await read(file.path));
    total += file.size;
  }
  // The sites it would load from (by address in its elements, styles and scripts).
  const sites = new Set();
  const site = (ref) => {
    const found = /^\s*(?:https?:)?\/\/([^/?#\s'"`)]+)/i.exec(ref ?? '');
    if (found) sites.add(found[1].toLowerCase());
  };
  for (const element of doc.querySelectorAll('[src], link[href], image[href]')) site(element.getAttribute('src') ?? element.getAttribute('href'));
  for (const code of doc.querySelectorAll('style, script')) {
    for (const found of code.textContent.matchAll(/(?:https?:)?\/\/[a-z0-9-]+(?:\.[a-z0-9-]+)+(?::\d+)?/gi)) site(found[0]);
  }
  const shim = doc.createElement('script');
  shim.textContent = `(() => {
  const files = ${JSON.stringify(data).replace(/</g, '\\u003c')};
  const types = ${JSON.stringify(Object.fromEntries(Object.keys(data).map((name) => [name, typeOf(name)])))};
  const dir = ${JSON.stringify(dir)};
  const real = window.fetch.bind(window);
  window.fetch = (input, init) => {
    const ref = typeof input === 'string' ? input : input?.url ?? String(input);
    if (!/^([a-z][a-z0-9+.-]*:|\\/\\/)/i.test(ref)) {
      try {
        const name = decodeURIComponent(new URL(ref, 'https://workspace.invalid/' + dir).pathname.slice(1));
        if (name in files) return Promise.resolve(new Response(files[name], { headers: { 'content-type': types[name] } }));
      } catch {}
    }
    return real(input, init);
  };
})();`;
  const policy = doc.createElement('meta');
  policy.httpEquiv = 'Content-Security-Policy';
  policy.content = web ? WEB : SHUT;
  (doc.head ?? doc.documentElement).prepend(policy, shim);
  return { html: `<!doctype html>\n${doc.documentElement.outerHTML}`, web: [...sites].sort() };
}

/** A frame showing `html` in preview.html (sent once the frame has loaded; writing the
 * page in loads the frame again). */
export function previewFrame(html, title) {
  const frame = document.createElement('iframe');
  frame.src = 'preview.html';
  frame.title = title;
  frame.onload = () => {
    frame.onload = null;
    frame.contentWindow.postMessage({ html }, '*');
  };
  return frame;
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
