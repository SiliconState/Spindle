// The browser Linux's network. Its network card ends here, in v86's small TCP/IP stack,
// and nothing a command opens goes on unless `allowed()` says its thread has network.
//
// With no relay, Chrome makes the requests: this gateway (the router's port 3128, and
// any address's port 80 for plain HTTP) reads HTTP/1.1 requests (absolute addresses
// from proxy clients; or, once the guest's proxy names a site it opened over HTTPS,
// that site's paths), fetches each (Chrome's own TLS, never the user's cookies) and
// answers as the response streams in. This computer and its local network are refused
// by address and by name (Chrome resolves names, so a public name pointing into the
// network is not caught). Redirects are never followed here: checking where a request
// landed would be too late to prevent requests into the local network.
//
// With the user's relay (a WISP server), every connection goes there as it is, and so
// do name lookups (over DNS-over-HTTPS) while a command with network runs; meanwhile a
// command without network gets made-up answers that lead nowhere.

const ROUTER = '192.168.86.1';
export const GATEWAY_PORT = 3128;
/** A request's head, and its body, are at most this long. */
const MAX_HEAD = 64 * 1024;
const MAX_BODY = 64 * 1024 * 1024;
/** Response bytes waiting for the guest before the gateway reads on. */
const HIGH_WATER = 1024 * 1024;
/** What a relayed connection may hold for the guest (WISP sends without asking). */
const RELAY_MOST = 64 * 1024 * 1024;
/** Request headers that are the connection's (or Chrome's to set), not the request's. */
const HOP = new Set([
  'connection', 'keep-alive', 'proxy-connection', 'proxy-authorization', 'te', 'trailer', 'transfer-encoding',
  'upgrade', 'host', 'content-length', 'expect',
]);
/** Response headers that describe what Chrome received, not what the guest gets. */
const RECEIVED = new Set(['content-encoding', 'content-length', 'transfer-encoding', 'connection', 'keep-alive']);
const REASONS = {
  100: 'Continue', 200: 'OK', 204: 'No Content', 206: 'Partial Content', 301: 'Moved Permanently', 302: 'Found',
  303: 'See Other', 304: 'Not Modified', 307: 'Temporary Redirect', 308: 'Permanent Redirect', 400: 'Bad Request',
  401: 'Unauthorized', 403: 'Forbidden', 404: 'Not Found', 413: 'Content Too Large', 429: 'Too Many Requests',
  500: 'Internal Server Error', 502: 'Bad Gateway', 503: 'Service Unavailable',
};

const utf8 = new TextEncoder();
const fromUtf8 = new TextDecoder();
/** Text whose characters are bytes (an HTTP head) as those bytes. */
const bytes = (text) => Uint8Array.from(text, (char) => char.charCodeAt(0) & 0xff);
const text = (data) => String.fromCharCode(...data);
const pause = (ms) => new Promise((done) => setTimeout(done, ms));
/** How long a segment the guest has not acknowledged waits before it is sent again. */
const RESEND_MS = 300;

/** Makes one of v86's TCP connections hold up under load. Its stack sends one segment
 * at a time, as large as the card's frames, and never again: a segment the guest drops
 * (its receive window was full) would stall the stream for good. Here each segment fits
 * the window the guest last offered (v86 offers no window scaling, so it is the plain
 * 16-bit one); an acknowledgement that takes nothing, or none in time, sends it again. */
function steady(conn) {
  const frame = conn.send_chunk_buf;
  const pump = conn.pump.bind(conn);
  const process = conn.process.bind(conn);
  let timer = 0;
  let window = conn.last?.tcp?.winsize ?? frame.length;
  let duplicates = 0;
  const live = () => !conn.stopped && conn.state !== 'closed' && conn.net.tcp_conn[conn.tuple] === conn;
  const resend = () => {
    conn.pending = false;
    conn.pump();
  };
  conn.pump = () => {
    if (conn.pending || !conn.send_buffer.length || !live()) return;
    // A closed window opens with the guest's next acknowledgement.
    if (!window) return;
    conn.send_chunk_buf = frame.subarray(0, Math.min(frame.length, window));
    pump();
    conn.send_chunk_buf = frame;
    clearTimeout(timer);
    const sent = conn.last_received_ackn;
    timer = setTimeout(() => {
      if (conn.pending && conn.last_received_ackn === sent && live()) resend();
    }, RESEND_MS);
  };
  conn.process = (packet) => {
    const before = conn.last_received_ackn;
    // The window the guest offers now, before v86 sends what the packet lets it.
    const opened = packet.tcp.ack && packet.tcp.winsize > window;
    if (packet.tcp.ack) window = packet.tcp.winsize;
    process(packet);
    if (!packet.tcp.ack) return;
    if (conn.last_received_ackn !== before) {
      duplicates = 0;
      return;
    }
    if (packet.tcp_data?.length) return;
    // An acknowledgement that took nothing: the window opening (after the guest dropped
    // what overran it), or a second one in a row, sends what is pending again. A single
    // one may only answer a copy sent before; resending on it would send all twice.
    duplicates += 1;
    if (conn.pending && (opened || duplicates >= 2)) {
      duplicates = 0;
      resend();
    }
  };
  conn.stop_steady = () => {
    conn.stopped = true;
    clearTimeout(timer);
  };
  return conn;
}

/** Why a request to `url` that did not go through could not: it is the browser's own
 * extension store (Chrome's, or Edge's), which the browser keeps from every extension
 * whatever sites it may reach (Chrome tells it as a CORS refusal). Null otherwise. */
export function storeRefusal(url) {
  let address;
  try {
    address = new URL(url);
  } catch {
    return null;
  }
  const { hostname, pathname } = address;
  const store = hostname === 'chromewebstore.google.com'
    || (hostname === 'chrome.google.com' && pathname.startsWith('/webstore'))
    || (hostname === 'microsoftedge.microsoft.com' && pathname.startsWith('/addons'));
  return store
    ? `${hostname} is the browser's extension store, which the browser never lets an extension reach (whatever sites it may reach), so Spindle cannot fetch it: look for what you need somewhere else`
    : null;
}

/** Whether `hostname` (as URL gives it) names this computer or a local network: by
 * address, or by a name only a local network gives (no dot, or a local suffix). */
export function local(hostname) {
  const name = hostname.replace(/^\[|\]$/g, '').toLowerCase().replace(/\.$/, '');
  if (!name.includes('.') && !name.includes(':')) return true;
  if (/(^|\.)(localhost|local|internal|lan|localdomain|home\.arpa)$/.test(name)) return true;
  const v4 = /^(\d+)\.(\d+)\.(\d+)\.(\d+)$/.exec(name);
  if (v4) {
    const [a, b] = [Number(v4[1]), Number(v4[2])];
    return a === 0 || a === 10 || a === 127 || a >= 224 || (a === 169 && b === 254) || (a === 172 && b >= 16 && b <= 31)
      || (a === 192 && b === 168) || (a === 100 && b >= 64 && b <= 127);
  }
  if (name.includes(':')) {
    return name.startsWith('::') || /^f[cd]/.test(name) || /^fe[89ab]/.test(name) || name.startsWith('ff');
  }
  return false;
}

/** Wires the machine's network: through `relay` (a wisp:// or wisps:// address) or
 * Chrome. Returns `end()`, which cuts what the last command left open. */
export function wireNetwork(vm, { relay, allowed }) {
  const adapter = vm.network_adapter;
  if (relay && typeof adapter?.on_tcp_connection !== 'function') {
    throw new Error(`the Linux's network card is not wired to a WISP relay (${relay})`);
  }
  // A frame the adapter cannot take (a relay not connected yet) must not stop the machine.
  const send = adapter.send.bind(adapter);
  adapter.send = (data) => {
    // Through a relay, names are looked up over DNS-over-HTTPS: only while a command
    // with network runs (otherwise made-up answers, which lead nowhere).
    if (relay) adapter.dns_method = allowed() ? 'doh' : 'static';
    try {
      send(data);
    } catch (error) {
      console.warn('the browser Linux network dropped a frame:', error);
    }
  };
  const open = new Set();
  if (relay) {
    // A connection not taken is reset. The guest's own subnet is not the relay's to reach.
    const through = adapter.on_tcp_connection.bind(adapter);
    adapter.on_tcp_connection = (conn, packet) => {
      if (!allowed() || conn.psrc.join('.').startsWith('192.168.86.')) return false;
      open.add(steady(conn));
      // The relay sends without asking: a guest that does not keep up is cut off.
      const write = conn.write.bind(conn);
      conn.write = (data) => {
        if (conn.send_buffer.length + data.length > RELAY_MOST) {
          conn.write = () => {};
          conn.on_close?.();
          conn.release();
          return;
        }
        write(data);
      };
      return through(conn, packet);
    };
  } else {
    adapter.on_tcp_connection = (conn) => {
      const gateway = conn.psrc.join('.') === ROUTER && conn.sport === GATEWAY_PORT;
      // v86's own handler takes port 80 first; this one answers it instead.
      if (!allowed() || !(gateway || conn.sport === 80)) {
        if (adapter.tcp_conn[conn.tuple]) conn.release();
        return;
      }
      if (!adapter.tcp_conn[conn.tuple]) conn.accept();
      const handle = serve(steady(conn), gateway ? null : 'http', () => open.delete(handle));
      open.add(handle);
    };
  }
  return {
    /** Ends what the last command left open (`hard`: the machine is going, so nothing
     * of it is waited for). */
    end({ hard = false } = {}) {
      for (const conn of open) {
        if (conn.stop) conn.stop(hard);
        else {
          conn.stop_steady?.();
          // A relay's connection with data the guest never took is not waited on (what
          // would take it is gone): its stream at the relay ends with it.
          if (hard || conn.send_buffer?.length) {
            conn.on_close?.();
            conn.release();
          } else {
            conn.close();
          }
        }
      }
      open.clear();
    },
  };
}

/** One connection's requests, answered in turn. `scheme` names the site from the Host
 * header (plain HTTP to port 80); otherwise requests name it, or the guest's proxy did. */
function serve(conn, scheme, gone) {
  let chunks = [];
  let size = 0;
  /** The request whose body is still coming: its head, parsed. */
  let head = null;
  let origin = null;
  let busy = false;
  let ended = false;
  let closed = false;
  let fetching = null;

  /** Closes once what was written is sent; nothing more is written. */
  const done = () => {
    if (closed) return;
    closed = true;
    fetching?.abort();
    conn.close();
    gone();
  };
  /** The machine is going: nothing is sent, nothing waits. */
  const drop = () => {
    closed = true;
    fetching?.abort();
    conn.stop_steady?.();
    conn.release();
    gone();
  };
  conn.on('data', (data) => {
    if (closed) return;
    if (size + data.length > MAX_BODY + MAX_HEAD) {
      chunks = [];
      size = 0;
      reply(413, 'Too much request data is waiting for the gateway.', true);
      return;
    }
    chunks.push(new Uint8Array(data));
    size += data.length;
    pump();
  });
  // The guest is done sending: what it asked is answered, then the connection closes.
  conn.on('shutdown', () => {
    ended = true;
    if (!busy) done();
  });
  // Reset by the guest.
  conn.on('close', () => {
    closed = true;
    fetching?.abort();
    gone();
  });

  const flat = () => {
    if (chunks.length > 1) {
      const all = new Uint8Array(size);
      let at = 0;
      for (const chunk of chunks) {
        all.set(chunk, at);
        at += chunk.length;
      }
      chunks = [all];
    }
    return chunks[0] ?? new Uint8Array(0);
  };
  /** The last `count` bytes that came, as text. */
  const tail = (count) => {
    let found = '';
    for (let at = chunks.length - 1; at >= 0 && found.length < count; at--) {
      found = text(chunks[at].subarray(Math.max(0, chunks[at].length - (count - found.length)))) + found;
    }
    return found;
  };
  const take = (count) => {
    const all = flat();
    chunks = count < all.length ? [all.subarray(count)] : [];
    size -= count;
    return all.subarray(0, count);
  };
  const write = (data) => !closed && conn.write(data);
  const reply = (status, message, close = false) => {
    const body = utf8.encode(`${message}\n`);
    write(bytes(`HTTP/1.1 ${status} ${REASONS[status]}\r\ncontent-type: text/plain; charset=utf-8\r\n`
      + `content-length: ${body.length}\r\n${close ? 'connection: close\r\n' : ''}\r\n`));
    write(body);
    if (close) done();
  };

  async function pump() {
    while (!busy && !closed) {
      // The guest's proxy names the site it opened before the first request.
      if (!origin && !scheme && !head && size && flat()[0] === 0x53 /* S */) {
        const end = flat().indexOf(10);
        if (end < 0) {
          if (size > MAX_HEAD) reply(400, 'The request head is too long.', true);
          return;
        }
        const named = /^SPINDLE-ORIGIN (https:\/\/\S+)\r?$/.exec(text(flat().subarray(0, end)));
        if (named) {
          origin = named[1];
          take(end + 1);
        }
      }
      const request = next();
      if (!request) {
        if (ended && !closed) done();
        return;
      }
      busy = true;
      try {
        await answer(request);
      } catch (error) {
        reply(502, `Spindle could not answer: ${error.message}`, true);
      }
      busy = false;
      if (request.close) done();
    }
  }

  /** The next whole request (null until it has all come). */
  function next() {
    if (!head) {
      const all = flat();
      const end = headEnd(all);
      if (end < 0) {
        if (size > MAX_HEAD) reply(400, 'The request head is too long.', true);
        return null;
      }
      const headBytes = take(end + 4).subarray(0, end);
      const lines = text(headBytes).split('\r\n');
      // The request line as UTF-8 (a raw non-ASCII path), the headers as bytes.
      const first = headBytes.indexOf(13);
      const [method = '', target = '', version = ''] = fromUtf8.decode(headBytes.subarray(0, first < 0 ? headBytes.length : first)).split(' ');
      const headers = [];
      for (const line of lines.slice(1)) {
        const at = line.indexOf(':');
        if (at > 0) headers.push([line.slice(0, at).trim(), line.slice(at + 1).trim()]);
      }
      const header = (name) => headers.find(([key]) => key.toLowerCase() === name)?.[1];
      const chunked = /chunked/i.test(header('transfer-encoding') ?? '');
      const length = chunked ? 0 : Number(header('content-length') ?? 0);
      if (!Number.isSafeInteger(length) || length < 0) {
        reply(400, 'The Content-Length is not a length.', true);
        return null;
      }
      if (length > MAX_BODY) {
        reply(413, `A request body is at most ${MAX_BODY / 1048576} MiB here.`, true);
        return null;
      }
      const connection = (header('connection') ?? header('proxy-connection') ?? '').toLowerCase();
      head = {
        method, target, headers, header, chunked, length,
        close: connection.includes('close') || (version === 'HTTP/1.0' && !connection.includes('keep-alive')),
        framed: version !== 'HTTP/1.0',
      };
      // A client that waits to be told to send its body is told.
      if ((chunked || length) && /100-continue/i.test(header('expect') ?? '')) write(bytes('HTTP/1.1 100 Continue\r\n\r\n'));
    }
    let body = null;
    if (head.chunked) {
      // Decoded once it can be whole: when what came ends as a chunked body ends (looked
      // at without joining what came, which would copy it all at every segment).
      if (size < 5 || tail(4) !== '\r\n\r\n') {
        if (size > MAX_BODY + MAX_HEAD) reply(413, `A request body is at most ${MAX_BODY / 1048576} MiB here.`, true);
        return null;
      }
      const decoded = unchunk(flat());
      if (!decoded) return null;
      if (decoded.body.length > MAX_BODY) {
        reply(413, `A request body is at most ${MAX_BODY / 1048576} MiB here.`, true);
        return null;
      }
      take(decoded.used);
      body = decoded.body;
    } else if (head.length) {
      if (size < head.length) return null;
      body = take(head.length).slice();
    }
    const request = { ...head, body };
    head = null;
    return request;
  }

  async function answer({ method, target, headers, header, body, close, framed }) {
    let url;
    try {
      if (/^https?:\/\//i.test(target)) url = new URL(target);
      else if (origin) url = new URL(target, origin);
      else if (scheme && header('host')) url = new URL(target, `${scheme}://${header('host')}`);
      else throw new Error('no site');
    } catch {
      reply(400, `Spindle's web gateway takes requests for http:// and https:// addresses, not ${JSON.stringify(target)}.`, close);
      return;
    }
    if (url.protocol !== 'http:' && url.protocol !== 'https:') {
      reply(400, `Spindle's web gateway reaches http:// and https:// addresses, not ${url.protocol}`, close);
      return;
    }
    if (local(url.hostname)) {
      reply(403, `The browser Linux does not reach ${url.host}: addresses on this computer or its local network are closed to it.`, close);
      return;
    }
    const sent = new Headers();
    for (const [name, value] of headers) {
      if (HOP.has(name.toLowerCase())) continue;
      try {
        sent.append(name, value);
      } catch {
        // Not a header value Chrome sends; the request goes without it.
      }
    }
    fetching = new AbortController();
    const reading = method === 'GET' || method === 'HEAD';
    let response;
    try {
      // A public URL may redirect into the LAN: refuse before following, not after.
      response = await fetch(url, {
        method, headers: sent, body: reading ? null : body, credentials: 'omit', cache: 'no-store',
        redirect: 'manual', referrerPolicy: 'no-referrer', signal: fetching.signal,
      });
    } catch (error) {
      if (!closed) {
        const store = storeRefusal(url);
        if (store) reply(403, `${store}.`, close);
        else {
          reply(502, `Chrome could not fetch ${url.href}: ${error.message}. If Spindle may not reach websites, `
            + "turn the thread's network on again in Spindle (Chrome asks for access to sites).", close);
        }
      }
      return;
    }
    if (response.type === 'opaqueredirect') {
      reply(502, `${url.href} answered with a redirect whose target Chrome hides. The gateway does not follow it: `
        + 'ask for the final public address directly.', close);
      return;
    }
    // Non-browser fetch implementations expose a manual redirect's target.
    if (response.status >= 300 && response.status < 400 && response.headers.has('location')) {
      response.body?.cancel().catch(() => {});
      const landed = new URL(response.headers.get('location'), url);
      if (local(landed.hostname)) {
        reply(403, `${url.href} redirected to ${landed.host}: addresses on this computer or its local network are closed to the browser Linux.`, close);
        return;
      }
      if (!['http:', 'https:'].includes(landed.protocol)) {
        reply(403, 'The gateway redirects only to public http(s) addresses.', close);
        return;
      }
      write(bytes(`HTTP/1.1 ${response.status} ${REASONS[response.status] ?? 'Redirect'}\r\nlocation: ${landed.href}\r\ncontent-length: 0\r\n\r\n`));
      return;
    }
    const empty = method === 'HEAD' || [204, 205, 304].includes(response.status) || response.status < 200;
    // A length Chrome's decoding does not change is the guest's too.
    const length = response.headers.has('content-encoding') ? null : response.headers.get('content-length');
    const chunked = !empty && !length && framed;
    const closing = close || (!empty && !length && !framed);
    const lines = [`HTTP/1.1 ${response.status} ${response.statusText || REASONS[response.status] || ''}`];
    for (const [name, value] of response.headers) if (!RECEIVED.has(name)) lines.push(`${name}: ${value}`);
    if (length && (!empty || method === 'HEAD')) lines.push(`content-length: ${length}`);
    // 205 is not one of HTTP's bodiless statuses: its empty body needs its length said.
    else if (response.status === 205) lines.push('content-length: 0');
    if (chunked) lines.push('transfer-encoding: chunked');
    if (closing) lines.push('connection: close');
    write(bytes(`${lines.join('\r\n')}\r\n\r\n`));
    if (empty || !response.body) {
      response.body?.cancel().catch(() => {});
      return;
    }
    const reader = response.body.getReader();
    try {
      for (;;) {
        const { value, done: last } = await reader.read();
        if (last) break;
        if (chunked) write(bytes(`${value.length.toString(16)}\r\n`));
        write(value);
        if (chunked) write(bytes('\r\n'));
        // The guest takes it at its pace: Chrome's reading waits while much is unsent.
        while (!closed && conn.send_buffer.length > HIGH_WATER) await pause(5);
        if (closed) {
          reader.cancel().catch(() => {});
          return;
        }
      }
    } catch {
      // Cut off midway: the guest sees the connection end before the body does.
      done();
      return;
    }
    if (chunked) write(bytes('0\r\n\r\n'));
    if (closing) done();
  }

  return { stop: (hard) => (hard ? drop() : done()) };
}

function headEnd(data) {
  const most = Math.min(data.length, MAX_HEAD + 4);
  for (let at = 0; at + 3 < most; at++) {
    if (data[at] === 13 && data[at + 1] === 10 && data[at + 2] === 13 && data[at + 3] === 10) return at;
  }
  return -1;
}

/** A chunked body ({body, used} once whole; null until then). */
function unchunk(data) {
  const parts = [];
  let total = 0;
  let at = 0;
  const line = () => {
    for (let i = at; i + 1 < data.length; i++) {
      if (data[i] === 13 && data[i + 1] === 10) {
        const found = text(data.subarray(at, i));
        at = i + 2;
        return found;
      }
    }
    return null;
  };
  for (;;) {
    const sizeLine = line();
    if (sizeLine === null) return null;
    const length = parseInt(sizeLine.split(';')[0], 16);
    if (!Number.isSafeInteger(length) || length < 0) return null;
    if (length === 0) {
      // Trailers, if any, then an empty line.
      for (let trailer = line(); trailer !== ''; trailer = line()) if (trailer === null) return null;
      const body = new Uint8Array(total);
      let put = 0;
      for (const part of parts) {
        body.set(part, put);
        put += part.length;
      }
      return { body, used: at };
    }
    if (data.length < at + length + 2) return null;
    parts.push(data.subarray(at, at + length));
    total += length;
    at += length + 2;
  }
}
