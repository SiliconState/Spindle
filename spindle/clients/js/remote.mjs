// A Spindle host's /v1 over fetch only (docs/API.md, section 4): what a browser app
// such as Spindle for Browser drives a thread on a host with. No Node APIs: it runs in
// a page, a worker or an extension, and in Node 20+ (tests). Every refusal rejects with
// a SpindleError whose `kind` is the host's stable code (busy, moved, not_found, ...)
// and whose `status` is the HTTP status (0: the host could not be reached).
import { SpindleError } from './common.mjs';

export { SpindleError } from './common.mjs';

/** How long a plain request may take before it counts as unanswered. */
const REQUEST_MS = 30_000;
/** A stream that says nothing (not even its heartbeat, every 10 s) for this long is
 * taken as cut, and reconnected. */
const SILENT_MS = 35_000;
/** Waits between reconnects of an event stream, then the last one over and over. */
const BACKOFF_MS = [500, 1000, 2000, 5000, 10_000, 15_000];

/** A fresh request ID (`A-Z a-z 0-9 - _`, 32 hex digits). */
export function requestId() {
  return [...crypto.getRandomValues(new Uint8Array(16))].map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

/** Percent-encoded query (a space is %20, so no host has to read `+` as one). */
function query(params) {
  const entries = Object.entries(params).filter(([, value]) => value !== undefined && value !== null);
  return entries.length ? `?${entries.map(([key, value]) => `${encodeURIComponent(key)}=${encodeURIComponent(value)}`).join('&')}` : '';
}

/** A SpindleError from a refused response: the v1 envelope's code, or http_STATUS. */
async function refusal(response) {
  const text = await response.text().catch(() => '');
  let envelope = null;
  try {
    envelope = JSON.parse(text);
  } catch {}
  if (!envelope || typeof envelope !== 'object' || Array.isArray(envelope)) envelope = { error: text || `HTTP ${response.status}` };
  const { error, code, ...data } = envelope;
  return new SpindleError(code || `http_${response.status}`, error ?? `HTTP ${response.status}`, response.status, data);
}

/** A request that never reached the host, or whose answer never came back. */
function unreachable(error, base) {
  if (error instanceof SpindleError) return error;
  const why = error?.name === 'TimeoutError' ? 'it did not answer in time' : String(error?.message ?? error);
  return new SpindleError('unreachable', `${base} could not be reached (${why})`, 0);
}

/** Whether a refusal may be sent again as it was: nothing reached the host, or the host
 * cannot say whether it was admitted (`outcome_unknown`). */
export function worthRepeating(error) {
  return error?.status === 0 || error?.kind === 'outcome_unknown' || (error?.status >= 502 && error?.status <= 504);
}

/** One `signal` from several (any of them aborts it). */
function anySignal(signals) {
  const live = signals.filter(Boolean);
  if (live.length <= 1) return live[0];
  if (AbortSignal.any) return AbortSignal.any(live);
  const controller = new AbortController();
  for (const signal of live) {
    if (signal.aborted) controller.abort(signal.reason);
    else signal.addEventListener('abort', () => controller.abort(signal.reason), { once: true });
  }
  return controller.signal;
}

/** Waits `ms`, or less if `signal` aborts. */
function pause(ms, signal) {
  if (signal?.aborted) return Promise.resolve();
  return new Promise((done) => {
    const finish = () => {
      clearTimeout(timer);
      signal?.removeEventListener('abort', finish);
      done();
    };
    const timer = setTimeout(finish, ms);
    signal?.addEventListener('abort', finish, { once: true });
  });
}

/**
 * Server-Sent Events, parsed as the HTML standard reads them: lines end with CRLF, LF
 * or CR; `data` lines join with LF; a blank line dispatches; comments (`:`) and frames
 * without data are dropped (a heartbeat is one). `push(text)` returns the frames it
 * completed: `{id, kind, data}`, `data` parsed as JSON (or the text, when it is not).
 */
export class EventParser {
  #buffer = '';
  #data = null;
  #kind = '';
  #id = undefined;
  #started = false;
  /** The last chunk ended with a CR: an LF that starts the next one is its pair. */
  #afterCr = false;

  push(text) {
    let rest = this.#buffer + text;
    if (this.#afterCr && rest.startsWith('\n')) rest = rest.slice(1);
    this.#afterCr = false;
    if (!this.#started && rest) {
      this.#started = true;
      if (rest.charCodeAt(0) === 0xfeff) rest = rest.slice(1);
    }
    const frames = [];
    for (;;) {
      const at = rest.search(/\r\n|\r|\n/);
      if (at < 0) break;
      const line = rest.slice(0, at);
      const pair = rest.startsWith('\r\n', at);
      rest = rest.slice(at + (pair ? 2 : 1));
      if (!pair && !rest && text.endsWith('\r')) this.#afterCr = true;
      const frame = this.#line(line);
      if (frame) frames.push(frame);
    }
    this.#buffer = rest;
    return frames;
  }

  #line(line) {
    if (line === '') {
      const data = this.#data;
      const frame = data === null ? null : { id: this.#id, kind: this.#kind || 'message', data: parsed(data) };
      this.#data = null;
      this.#kind = '';
      return frame;
    }
    if (line.startsWith(':')) return null;
    const colon = line.indexOf(':');
    const field = colon < 0 ? line : line.slice(0, colon);
    let value = colon < 0 ? '' : line.slice(colon + 1);
    if (value.startsWith(' ')) value = value.slice(1);
    if (field === 'data') this.#data = this.#data === null ? value : `${this.#data}\n${value}`;
    else if (field === 'event') this.#kind = value;
    else if (field === 'id' && !value.includes('\0')) this.#id = value;
    return null;
  }
}

function parsed(text) {
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}

/** A Spindle host's /v1, with a bearer token (an API, runner or browser token). */
export class RemoteClient {
  constructor({ url, token, fetch = globalThis.fetch.bind(globalThis) }) {
    this.base = String(url).replace(/\/+$/, '');
    this.token = token;
    this.fetch = fetch;
  }

  #headers(more = {}) {
    return { ...(this.token != null && { authorization: `Bearer ${this.token}` }), ...more };
  }

  /** One request: its JSON answer (or, `raw`, the Response, checked ok). */
  async request(method, path, { body, headers = {}, raw = false, signal, timeout = REQUEST_MS } = {}) {
    const json = body !== undefined && !(body instanceof Uint8Array) && !(body instanceof Blob);
    let response;
    try {
      response = await this.fetch(this.base + path, {
        method,
        headers: this.#headers({ ...(json && { 'content-type': 'application/json' }), ...headers }),
        body: json ? JSON.stringify(body) : body,
        credentials: 'omit',
        cache: 'no-store',
        redirect: 'error',
        signal: anySignal([signal, timeout ? AbortSignal.timeout(timeout) : null]),
      });
    } catch (error) {
      if (signal?.aborted) throw error;
      throw unreachable(error, this.base);
    }
    if (!response.ok) throw await refusal(response);
    if (raw) return response;
    try {
      return await response.json();
    } catch (error) {
      throw unreachable(error, this.base);
    }
  }

  // ---- Discovery, models and the thread list.

  /** `{api, api_version, features, version, limits, ...}`. */
  info(options) { return this.request('GET', '/v1', options); }
  /** `{providers: [{id, name, signed_in, subscription, default_model, models: [...]}]}`. */
  models(options) { return this.request('GET', '/v1/models', options); }
  providers(options) { return this.request('GET', '/v1/providers', options); }
  /** `[{id, name, status: {state, at}}]`, newest first (a browser token sees its own). */
  async sessions(options) { return (await this.request('GET', '/v1/sessions', options)).sessions; }

  // ---- A session.

  /** The session object (head state, `busy`, `settings`, `usage`, `app_tools`, ...). */
  session(id, options) { return this.request('GET', `/v1/sessions/${id}`, options); }
  /** `{head, revision, messages}`. */
  messages(id, revision, options) { return this.request('GET', `/v1/sessions/${id}/messages${query({ revision })}`, options); }
  files(id, revision, options) { return this.request('GET', `/v1/sessions/${id}/files${query({ revision })}`, options); }
  /** A file's bytes (a Uint8Array). */
  async read(id, path, revision, options) {
    const response = await this.request('GET', `/v1/sessions/${id}/file${query({ path, revision })}`, { ...options, raw: true });
    return new Uint8Array(await response.arrayBuffer());
  }
  async targets(id, options) { return (await this.request('GET', `/v1/sessions/${id}/targets`, options)).targets; }
  usage(id, options) { return this.request('GET', `/v1/sessions/${id}/usage`, options); }
  tasks(id, options) { return this.request('GET', `/v1/sessions/${id}/tasks`, options); }
  taskStop(id, task, options) { return this.request('POST', `/v1/sessions/${id}/tasks/${encodeURIComponent(task)}/stop`, { ...options, body: {} }); }
  /** A message for the running turn: `{queued}` (409 not_running between turns). */
  steer(id, text, { now = false, ...options } = {}) {
    return this.request('POST', `/v1/sessions/${id}/steer`, { ...options, body: { ...(text != null && { text }), ...(now && { now: true }) } });
  }
  /** Bytes for a later `say`'s attachments: `{digest, size}`. */
  upload(id, bytes, options) {
    return this.request('POST', `/v1/sessions/${id}/upload`, { ...options, body: bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes), headers: { 'content-type': 'application/octet-stream' }, timeout: 150_000 });
  }
  /** The app's answer to an `app_tool` call (`{accepted}`; 409 conflict: no longer waiting). */
  appToolResult(id, call, answer, options) {
    return this.request('POST', `/v1/sessions/${id}/app_tool_result`, { ...options, body: { call, ...answer } });
  }

  // ---- Commands: admitted once per request ID, so sending one again is always safe.

  /**
   * Admits a command and resolves with its receipt (`{request_id, session, kind, state,
   * error}`). A request that may not have reached the host, or whose admission the host
   * cannot vouch for, is sent again with the same request ID (the host admits it once:
   * the same body answers with the same receipt) up to `attempts` times.
   */
  async command(kind, fields = {}, { requestId: id = requestId(), attempts = 4, signal } = {}) {
    for (let attempt = 1; ; attempt++) {
      try {
        return await this.request('POST', '/v1/commands', { body: { request_id: id, kind, ...fields }, signal });
      } catch (error) {
        if (attempt >= attempts || !worthRepeating(error) || signal?.aborted) throw Object.assign(error, { requestId: id });
        await pause(BACKOFF_MS[Math.min(attempt - 1, BACKOFF_MS.length - 1)], signal);
      }
    }
  }
  receipt(id, options) { return this.request('GET', `/v1/receipts/${encodeURIComponent(id)}`, options); }
  /** Resolves with the receipt once its command has ended (`completed`, `failed`,
   * `interrupted`), asking every `every` ms. */
  async settled(id, { every = 300, timeout = 120_000, signal } = {}) {
    const until = Date.now() + timeout;
    for (;;) {
      let receipt;
      try {
        receipt = await this.receipt(id, { signal });
      } catch (error) {
        if (!worthRepeating(error) || Date.now() > until) throw error;
      }
      if (receipt && receipt.state !== 'running') return receipt;
      if (Date.now() > until) throw new SpindleError('timeout', 'the host has not finished the command yet', 0, { receipt });
      await pause(every, signal);
      if (signal?.aborted) throw signal.reason;
    }
  }

  // ---- Threads a client brings and takes back (4.8).

  /**
   * Brings a thread in: `bytes` (a session bundle) under `key` (the Idempotency-Key: the
   * same key with the same bytes answers with the same session, so after any failure
   * the same call is safe to make again). `name`, and `provider` with `model` (both or
   * neither) as the host takes them. Resolves with `{session, fit, replayed}`.
   */
  importThread(bytes, { key, name, provider, model, signal } = {}) {
    const blob = bytes instanceof Blob ? bytes : new Blob([bytes]);
    return this.request('POST', `/v1/threads${query({ name, provider, model })}`, {
      body: blob, signal,
      headers: { 'content-type': 'application/octet-stream', 'idempotency-key': key },
      // The host's 2 minutes and a second per MiB for the upload, and half a minute more.
      timeout: 150_000 + Math.ceil(blob.size / 1048576) * 1000,
    });
  }

  /**
   * A thread's bundle: `{bytes, head, identity}`. Without `keep` the thread moves back:
   * the host seals its copy and keeps the bundle, answering every later export with the
   * same bytes until the thread is deleted (a `delete` command). A body cut short is
   * refused here (`kind` 'cut'): ask again.
   */
  async exportThread(id, { keep = false, signal } = {}) {
    // Up to 10 minutes for the answer (the host writes, checks and seals the bundle
    // first), then two and a half minutes and a second per MiB it says the bundle has,
    // as an upload is given: a large thread on a slow line still comes back.
    const cut = new AbortController();
    const after = (ms) => setTimeout(() => cut.abort(new DOMException('the export timed out', 'TimeoutError')), ms);
    let timer = after(600_000);
    let bytes;
    let response;
    try {
      try {
        response = await this.request('POST', `/v1/sessions/${id}/export`, { body: keep ? { keep: true } : {}, raw: true, signal: anySignal([signal, cut.signal]), timeout: 0 });
      } catch (error) {
        if (cut.signal.aborted && !signal?.aborted) throw unreachable(error, this.base);
        throw error;
      }
      clearTimeout(timer);
      const size = Number(response.headers.get('content-length'));
      timer = after(150_000 + Math.ceil((Number.isFinite(size) && size > 0 ? size : 0) / 1048576) * 1000);
      try {
        bytes = new Uint8Array(await response.arrayBuffer());
      } catch (error) {
        throw unreachable(error, this.base);
      }
    } finally {
      clearTimeout(timer);
    }
    const said = response.headers.get('content-length');
    const length = said == null ? null : Number(said);
    if (length != null && length !== bytes.byteLength) {
      throw new SpindleError('cut', `the bundle came cut short (${bytes.byteLength} of ${length} bytes)`, 0);
    }
    return { bytes, head: Number(response.headers.get('x-spindle-head')), identity: response.headers.get('x-spindle-identity') };
  }

  /** One-click connect (4.9): the code a connect page gave and its PKCE verifier, for a
   * browser token: `{token, client}`. Sent without a credential. */
  connectToken(code, verifier, options) {
    const anonymous = new RemoteClient({ url: this.base, token: null, fetch: this.fetch });
    return anonymous.request('POST', '/v1/connect/token', { ...options, body: { code, verifier } });
  }

  // ---- The event stream (4.6).

  /**
   * Frames of a session's event stream as they come: `{id, kind, data}` with `kind`
   * 'snapshot', 'event' or 'deleted'. `lastEventId` resumes after that frame. Ends when
   * the host ends the stream; a stream silent past its heartbeat is cut (rejects).
   */
  async *events(id, { lastEventId, signal } = {}) {
    const cut = new AbortController();
    let silent = null;
    const quiet = () => {
      clearTimeout(silent);
      silent = setTimeout(() => cut.abort(new SpindleError('unreachable', `${this.base} went quiet`, 0)), SILENT_MS);
    };
    quiet();
    let response;
    try {
      response = await this.fetch(`${this.base}/v1/sessions/${id}/events`, {
        headers: this.#headers({ accept: 'text/event-stream', ...(lastEventId && { 'last-event-id': lastEventId }) }),
        credentials: 'omit', cache: 'no-store', redirect: 'error', signal: anySignal([signal, cut.signal]),
      });
    } catch (error) {
      clearTimeout(silent);
      if (signal?.aborted) throw error;
      throw unreachable(cut.signal.aborted ? cut.signal.reason : error, this.base);
    }
    if (!response.ok) {
      clearTimeout(silent);
      throw await refusal(response);
    }
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    const parser = new EventParser();
    try {
      for (;;) {
        let chunk;
        try {
          chunk = await reader.read();
        } catch (error) {
          if (signal?.aborted) throw error;
          throw unreachable(cut.signal.aborted ? cut.signal.reason : error, this.base);
        }
        if (chunk.done) break;
        quiet();
        for (const frame of parser.push(decoder.decode(chunk.value, { stream: true }))) yield frame;
      }
    } finally {
      clearTimeout(silent);
      reader.cancel().catch(() => {});
    }
  }

  /**
   * Follows a session's events until `signal` aborts, `onFrame` returns false, or the
   * session is deleted: every frame goes to `onFrame(frame)` (awaited, in order), and a
   * stream that ends or breaks is opened again from the last frame's id (the host ends
   * one every five minutes), after a pause that grows while the host does not answer.
   * `onState('open' | 'waiting', error)` says how the stream stands. A refusal that
   * another try cannot change (401, 403, 404) rejects. Resolves with the last frame's
   * id and why it ended ('deleted', 'done', 'aborted').
   */
  async follow(id, { lastEventId, signal, onFrame, onState = () => {} } = {}) {
    let last = lastEventId;
    let failures = 0;
    while (!signal?.aborted) {
      try {
        for await (const frame of this.events(id, { lastEventId: last, signal })) {
          failures = 0;
          onState('open');
          const goOn = await onFrame(frame);
          if (frame.id !== undefined) last = frame.id;
          if (frame.kind === 'deleted') return { last, ended: 'deleted' };
          if (goOn === false) return { last, ended: 'done' };
        }
        await pause(BACKOFF_MS[Math.min(failures, BACKOFF_MS.length - 1)], signal);
        failures += 1;
      } catch (error) {
        if (signal?.aborted) break;
        if ([401, 403, 404].includes(error?.status)) throw error;
        onState('waiting', error);
        await pause(BACKOFF_MS[Math.min(failures, BACKOFF_MS.length - 1)], signal);
        failures += 1;
      }
    }
    return { last, ended: 'aborted' };
  }
}
