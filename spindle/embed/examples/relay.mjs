// A local relay for using the package in a browser. It serves a folder (the kit by
// default) so the page, its worker and the wasm come from one origin, http://localhost
// (which browsers treat as secure: OPFS and Web Locks work there), and it forwards the
// page's model requests to the provider, putting the key in. The key never reaches the
// page, and no provider needs to allow the page's origin: the browser only talks here.
//
//   SPINDLE_KEY=... node relay.mjs [--port N] [--catalog FILE] [--glue FILE]
//                                  [--root DIR] [--report FILE]
//
// Nothing about any provider is built in. Where requests may go comes from the provider
// catalog (`--catalog`, default ~/.dext/providers.json when present, otherwise the
// package's built-in catalog, read through the package's Node glue): only POSTs to a
// catalog provider's base URL are forwarded. The page fills the provider's
// authentication headers with a marker this relay draws at random when it starts (it
// serves it at /relay/config), and the relay puts the key where the marker is. Requests
// from any other origin are refused, so another site open in the browser cannot use the
// relay. It listens on 127.0.0.1 only, on a free port unless `--port` names one.
//
// `--report FILE` (for tests) appends each POST /report body to FILE as a line, and
// GET /stats answers how many model requests went upstream and how many are open.
import { randomUUID } from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import { createRequire } from 'node:module';
import { homedir } from 'node:os';
import { extname, join, normalize, relative, resolve, sep } from 'node:path';
import { Readable } from 'node:stream';
import { fileURLToPath } from 'node:url';

const options = {};
const args = process.argv.slice(2);
while (args.length) {
  const arg = args.shift();
  if (!arg.startsWith('--') || !args.length) throw new Error(`expected --name value, got ${arg}`);
  options[arg.slice(2)] = args.shift();
}
const key = process.env.SPINDLE_KEY;
if (!key) {
  console.error('set SPINDLE_KEY to the key of the provider you will use, in the same command:\n  SPINDLE_KEY=your-key node examples/relay.mjs');
  process.exit(2);
}
const kit = fileURLToPath(new URL('..', import.meta.url));
const root = resolve(options.root ?? kit);
// The browser page that sits next to this relay, as served from `root` (when it is).
const pageDir = fileURLToPath(new URL('browser/', import.meta.url));
const page = pageDir.startsWith(root + sep) && fs.existsSync(join(pageDir, 'index.html'))
  ? `/${relative(root, pageDir).split(sep).join('/')}/`
  : null;

// The catalog: the app's own file, or the package's built-ins (asked of the package).
const catalogFile = options.catalog ?? join(homedir(), '.dext', 'providers.json');
const catalogText = fs.existsSync(catalogFile) ? fs.readFileSync(catalogFile, 'utf8') : undefined;
const wasm = createRequire(import.meta.url)(resolve(options.glue ?? join(kit, 'node', 'spindle_embed.js')));
const probe = new wasm.Embedded('memory', '', catalogText);
const catalog = JSON.parse(probe.catalog());
probe.free();
const allowed = new Set(
  catalog.providers.flatMap((provider) => {
    try {
      return [new URL(provider.base_url).origin];
    } catch {
      return [];
    }
  }),
);
const marker = `relay-${randomUUID()}`;
const stats = { requests: 0, open: 0 };

const types = {
  '.html': 'text/html; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.wasm': 'application/wasm',
  '.json': 'application/json',
  '.css': 'text/css; charset=utf-8',
};
// Request headers that are the browser's own, not the model request's.
const browserOwn = new Set(['host', 'origin', 'referer', 'cookie', 'connection', 'content-length',
  'accept-encoding', 'user-agent', 'accept-language']);

let port = 0;
/** Whether a request comes from this relay's own page (or from no browser at all). */
function ownPage(request) {
  const hosts = [`localhost:${port}`, `127.0.0.1:${port}`];
  const origin = request.headers.origin;
  return hosts.includes(request.headers.host) && (!origin || hosts.some((host) => origin === `http://${host}`));
}

async function relay(request, response, to) {
  let target = null;
  try {
    target = new URL(to);
  } catch {}
  if (request.method !== 'POST' || !target || !allowed.has(target.origin)) {
    response.writeHead(403, { 'content-type': 'text/plain' }).end('only POSTs to a catalog provider are relayed');
    return;
  }
  const parts = [];
  for await (const part of request) parts.push(part);
  const headers = {};
  for (const [name, value] of Object.entries(request.headers)) {
    if (browserOwn.has(name) || name.startsWith('sec-')) continue;
    headers[name] = String(value).replaceAll(marker, key);
  }
  // The page stopping the request (a stop, a deadline) ends it upstream too.
  const controller = new AbortController();
  stats.requests += 1;
  stats.open += 1;
  response.on('close', () => {
    stats.open -= 1;
    if (!response.writableFinished) controller.abort();
  });
  try {
    const upstream = await fetch(target, {
      method: 'POST', headers, body: Buffer.concat(parts), signal: controller.signal, redirect: 'error',
    });
    const out = { 'content-type': upstream.headers.get('content-type') ?? 'application/octet-stream', 'cache-control': 'no-store' };
    const retry = upstream.headers.get('retry-after');
    if (retry) out['retry-after'] = retry;
    response.writeHead(upstream.status, out);
    if (upstream.body) Readable.fromWeb(upstream.body).on('error', () => response.destroy()).pipe(response);
    else response.end();
    console.log(`${new Date().toISOString()} ${upstream.status} ${target.origin}${target.pathname}`);
  } catch (error) {
    console.log(`${new Date().toISOString()} failed ${target.origin}${target.pathname}: ${error.message}`);
    if (!response.headersSent) response.writeHead(502, { 'content-type': 'text/plain' }).end(error.message);
    else response.destroy();
  }
}

function json(response, value) {
  response.writeHead(200, { 'content-type': 'application/json', 'cache-control': 'no-store' }).end(JSON.stringify(value));
}

const server = http.createServer((request, response) => {
  const url = new URL(request.url, 'http://localhost');
  if (url.pathname.startsWith('/relay') || url.pathname === '/report' || url.pathname === '/stats') {
    if (!ownPage(request)) return void response.writeHead(403, { 'content-type': 'text/plain' }).end('not this relay\'s page');
  }
  if (url.pathname === '/relay') return void relay(request, response, url.searchParams.get('to'));
  if (url.pathname === '/relay/config') return void json(response, { marker });
  if (url.pathname === '/relay/catalog') return void json(response, catalog);
  if (options.report && url.pathname === '/report' && request.method === 'POST') {
    const parts = [];
    request.on('data', (part) => parts.push(part));
    request.on('end', () => {
      fs.appendFileSync(options.report, `${Buffer.concat(parts).toString().replace(/\n/g, ' ')}\n`);
      response.writeHead(204).end();
    });
    return;
  }
  if (options.report && url.pathname === '/stats') return void json(response, stats);
  if (request.method !== 'GET') return void response.writeHead(405).end();
  let path = decodeURIComponent(url.pathname);
  if (path === '/' && page && !fs.existsSync(join(root, 'index.html'))) {
    return void response.writeHead(302, { location: page }).end();
  }
  if (path.endsWith('/')) path += 'index.html';
  const file = normalize(join(root, path));
  if (!file.startsWith(root + sep) || !fs.existsSync(file) || !fs.statSync(file).isFile()) {
    return void response.writeHead(404, { 'content-type': 'text/plain' }).end('not found');
  }
  response.writeHead(200, { 'content-type': types[extname(file)] ?? 'application/octet-stream', 'cache-control': 'no-store' });
  fs.createReadStream(file).pipe(response);
});
server.listen(Number(options.port ?? 0), '127.0.0.1', () => {
  port = server.address().port;
  console.log(`relay on http://localhost:${port}/ (${allowed.size} provider origin(s) from the catalog)`);
  if (page) console.log(`open http://localhost:${port}${page} in Chrome or Edge`);
});
