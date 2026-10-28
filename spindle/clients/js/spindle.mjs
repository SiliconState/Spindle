// Spindle API client for Node.js 18+, no dependencies.
//
// Two transports of the same v1 API (docs/API.md):
//   StdioClient  runs `spindle api` and speaks JSON-RPC 2.0 over NDJSON on its
//                stdin/stdout. Sessions are named by directory.
//   HttpClient   calls the /v1 routes of `spindle serve` with a bearer token
//                (`spindle --dir HOST token new NAME`). Sessions are named by id.
// Errors from either transport reject with a SpindleError whose `kind` is the stable
// error code (busy, head_moved, not_found, ...).
import { spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { EventEmitter } from 'node:events';
import { resolve } from 'node:path';
import { createInterface } from 'node:readline';

export const API_VERSION = 1;
export { SpindleError, Tools, answerOf, refusedAnswer } from './common.mjs';
import { SpindleError, Tools, refusedAnswer } from './common.mjs';

/** Longest request line `spindle api` reads. */
const MAX_LINE = 64 * 1024 * 1024;

/** Drives `spindle api`. Calls may overlap; each returns a Promise of its result
 * (prompt, continue, retry and rewind resolve when the turn ends). Every event is
 * emitted as 'event' with (event, session, requestId); a listener that throws does
 * not stop replies (its error is rethrown asynchronously). Once the process has ended
 * (or could not start), pending and new calls reject with kind 'closed'.
 *
 * With `supervise: true` a process that ends on its own (killed, crashed) is started
 * again ('restarted'): calls pending then still reject with 'closed', and for each
 * session this client ran (created, opened, prompted, continued, retried, rewound or
 * started a task in, and has not closed), `recover` is read ('recovered' with (dir,
 * report)) and a session whose `wake.work` is 'ready' and has no actionable unknown
 * recovery items is continued (known work only, even with resend consent;
 * the app is asked again for an unanswered call); a failure is 'recover_error' with
 * (error, dir). 'blocked' sessions wait for the app or a person. More than five ends
 * within a minute stop the supervision ('supervise_ended'). An app tool answer that
 * cannot be delivered is 'app_tool_error' with (error, event, session). A listener of
 * these that throws has its error rethrown asynchronously, like 'event' ones. */
export class StdioClient extends EventEmitter {
  constructor({ binary = process.env.SPINDLE_BIN || 'spindle', env = process.env, stderr = 'inherit', supervise = false } = {}) {
    super();
    Object.assign(this, { binary, env, stderr, supervise });
    this.pending = new Map();
    this.next = 0;
    this.closed = null;
    this.tools = new Tools();
    // Sessions this client ran, by resolved path, with the spelling it used.
    this.sessions = new Map();
    this.ending = false;
    this.ends = [];
    this.#start();
  }

  #start() {
    const child = spawn(this.binary, ['api'], { env: this.env, stdio: ['pipe', 'pipe', this.stderr] });
    this.process = child;
    // An ended process's late notices never close the one that replaced it.
    const close = (message) => { if (child === this.process) this.#close(message); };
    this.exited = new Promise((resolve) => {
      child.on('close', (code) => {
        close('spindle api ended');
        resolve(code);
        if (this.supervise && !this.ending && child === this.process) this.#restart();
      });
      // A binary that cannot start (ENOENT, EACCES) ends the client, never the program.
      child.on('error', (error) => {
        close(`spindle api failed: ${error.message}`);
        resolve(null);
      });
    });
    // Writes after the process ended fail here (EPIPE); the calls are rejected below.
    child.stdin.on('error', () => close('spindle api ended'));
    const lines = createInterface({ input: child.stdout });
    lines.on('line', (line) => {
      let frame;
      try { frame = JSON.parse(line); } catch { return; }
      this.#frame(frame);
    });
    lines.on('close', () => close('spindle api ended'));
  }

  #close(message) {
    this.closed ??= message;
    for (const { reject } of this.pending.values()) reject(new SpindleError('closed', this.closed));
    this.pending.clear();
  }

  /** Emits without letting a throwing listener cut the caller's work short. */
  #notify(name, ...args) {
    try {
      this.emit(name, ...args);
    } catch (error) {
      queueMicrotask(() => { throw error; });
    }
  }

  /** Remembers a session this client runs, for supervision. */
  #ran(dir) {
    if (typeof dir === 'string') this.sessions.set(resolve(dir), dir);
  }

  async #restart() {
    const now = Date.now();
    this.ends = this.ends.filter((at) => now - at < 60_000).concat(now);
    if (this.ends.length > 5) {
      this.#notify('supervise_ended');
      return;
    }
    this.closed = null;
    this.#start();
    this.#notify('restarted');
    for (const dir of [...this.sessions.values()]) {
      try {
        const report = await this.call('recover', { dir });
        this.#notify('recovered', dir, report);
        const unknown = report.items?.some((item) => item.next !== 'none' && (item.outcome === 'unknown' || item.unknown === true));
        if (report.wake?.work === 'ready' && !unknown) {
          this.resume(dir).catch((error) => this.#notify('recover_error', error, dir));
        }
      } catch (error) {
        this.#notify('recover_error', error, dir);
      }
    }
  }

  #frame(frame) {
    if (frame.method === 'event') {
      const { session, request, event } = frame.params || {};
      if (event?.type === 'app_tool' && this.tools.byName.has(event.name)) this.#answer(session, event);
      try {
        this.emit('event', event, session, request);
      } catch (error) {
        queueMicrotask(() => { throw error; });
      }
      return;
    }
    const waiting = this.pending.get(frame.id);
    if (!waiting) return;
    this.pending.delete(frame.id);
    if (frame.error) {
      const data = frame.error.data || {};
      waiting.reject(new SpindleError(data.code || 'internal', frame.error.message, frame.error.code, data));
    } else {
      waiting.resolve(frame.result);
    }
  }

  async #answer(session, event) {
    const process = this.process;
    const answer = await this.tools.answer(event);
    try {
      if (process !== this.process) throw new SpindleError('closed', 'app answer belongs to an ended process');
      try {
        await this.call('app_tool_result', { dir: session, call: event.call, ...answer });
      } catch (error) {
        // Refused as malformed, or not sendable at all (not JSON, too long).
        if (!['bad_request', 'unsendable'].includes(error?.kind)) throw error;
        await this.call('app_tool_result', { dir: session, call: event.call, ...refusedAnswer(error) });
      }
    } catch (error) {
      // Stale (the run stopped first) or the process ended: the call is asked again.
      this.#notify('app_tool_error', error, event, session);
    }
  }

  /** Registers an app tool (see Tools); sessions this client creates offer it. */
  tool(definition) { this.tools.add(definition); return this; }

  /** Sends one request; resolves with its result. One that cannot be sent (not JSON,
   * or a line over 64 MiB) rejects with kind 'unsendable' and is never written. */
  call(method, params = {}) {
    const id = ++this.next;
    const refuse = (kind, message) => {
      const refused = Promise.reject(new SpindleError(kind, message));
      refused.requestId = id;
      return refused;
    };
    if (this.closed) return refuse('closed', this.closed);
    let line;
    try {
      line = JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n';
    } catch (error) {
      return refuse('unsendable', `the request is not JSON: ${error.message}`);
    }
    if (Buffer.byteLength(line) > MAX_LINE) return refuse('unsendable', 'the request is over 64 MiB');
    const reply = new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
    reply.requestId = id;
    this.process.stdin.write(line);
    return reply;
  }

  // The methods of the API, by name (params as in docs/API.md).
  info() { return this.call('info'); }
  providers() { return this.call('providers'); }
  /** Tools registered with `tool()` are offered unless `settings.tools` says otherwise. */
  create(dir, provider, settings = {}) {
    this.#ran(dir);
    const tools = this.tools.byName.size ? { tools: this.tools.definitions() } : {};
    return this.call('create', { dir, provider, ...tools, ...settings });
  }
  open(dir) { this.#ran(dir); return this.call('open', { dir }); }
  closeSession(dir) {
    if (typeof dir === 'string') this.sessions.delete(resolve(dir));
    return this.call('close', { dir });
  }
  /** `state`: a JSON merge patch to the app state, committed with the prompt. */
  prompt(dir, text, { files, state, expectedHead } = {}) {
    this.#ran(dir);
    return this.call('prompt', { dir, text, ...(files && { files }), ...(state && { state }), ...(expectedHead != null && { expected_head: expectedHead }) });
  }
  resume(dir, { expectedHead, until } = {}) {
    this.#ran(dir);
    return this.call('continue', { dir, ...(until != null && { until }), ...(expectedHead != null && { expected_head: expectedHead }) });
  }
  retry(dir, { expectedHead } = {}) {
    this.#ran(dir);
    return this.call('retry', { dir, ...(expectedHead != null && { expected_head: expectedHead }) });
  }
  rewind(dir, revision, { expectedHead } = {}) {
    this.#ran(dir);
    return this.call('rewind', { dir, revision, ...(expectedHead != null && { expected_head: expectedHead }) });
  }
  stop(dir) { return this.call('stop', { dir }); }
  steer(dir, text) { return this.call('steer', { dir, text }); }
  status(dir) { return this.call('status', { dir }); }
  history(dir, revision) { return this.call('history', { dir, revision }); }
  async targets(dir) { return (await this.call('targets', { dir })).targets; }
  files(dir, revision) { return this.call('files', { dir, revision }); }
  /** The file's bytes as a Buffer. */
  async read(dir, path, revision) {
    const reply = await this.call('read', { dir, path, revision });
    return Buffer.from(reply.content, reply.encoding === 'utf8' ? 'utf8' : 'base64');
  }
  export(dir, target) { return this.call('export', { dir, target }); }
  /** Writes the session to a bundle file (`target`, new); it moves there unless `keep`. */
  exportSession(dir, target, { keep } = {}) { return this.call('export_session', { dir, target, ...(keep && { keep }) }); }
  /** Continues a bundled session in `dir` (new or empty). */
  importSession(dir, source) { this.#ran(dir); return this.call('import_session', { dir, source }); }
  settings(dir, changes = {}) { return this.call('settings', { dir, ...changes }); }
  usage(dir) { return this.call('usage', { dir }); }
  doctor(dir) { return this.call('doctor', { dir }); }
  /** The app state as of a revision (the head by default): { head, revision, state }. */
  state(dir, revision) { return this.call('state', { dir, revision }); }
  /** Applies a JSON merge patch to the app state: a step of its own between turns
   * ({ revision }), or at the running turn's next step boundary ({ queued }). */
  setState(dir, patch, { expectedHead } = {}) {
    return this.call('state_set', { dir, patch, ...(expectedHead != null && { expected_head: expectedHead }) });
  }

  /** What a session's owner left unfinished and what is safe to resume (`wake`). */
  recover(dir) { return this.call('recover', { dir }); }

  /** Runs the task `task` (the same id finds the same task) from the session's files to
   * the end of its turn: { task (its directory, a session of its own), answer, failed }. */
  taskStart(dir, task, prompt, { model, effort, files, settings, report = 'wait', expectedHead } = {}) {
    this.#ran(dir);
    return this.call('task_start', { dir, task, prompt, report, ...(model && { model }), ...(effort && { effort }), ...(files && { files }), ...(settings && { settings }), ...(expectedHead != null && { expected_head: expectedHead }) });
  }
  taskWait(dir, task, timeoutS = 600) { return this.call('task_wait', { dir, task, timeout_s: timeoutS }); }
  taskStop(dir, task) { return this.call('task_stop', { dir, task }); }
  /** The session's tasks with their head state: { tasks: [{ task, dir, status }] }. */
  tasks(dir) { return this.call('tasks', { dir }); }

  /** Ends input (running turns are asked to stop); resolves with the exit code. */
  close() {
    this.ending = true;
    this.process.stdin.end();
    return this.exited;
  }
}

/** Calls `spindle serve`'s /v1 routes with a bearer token. */
export class HttpClient {
  constructor(baseUrl, token) {
    this.base = baseUrl.replace(/\/+$/, '');
    this.token = token;
    this.tools = new Tools();
  }

  /** Registers an app tool (see Tools); offer it with `tools: client.tools.definitions()`
   * in a `new` or `settings` command, and answer calls with `answerAppTools`. */
  tool(definition) { this.tools.add(definition); return this; }
  appToolResult(id, call, answer) {
    return this.request('POST', `/v1/sessions/${id}/app_tool_result`, { call, ...answer });
  }
  /** Answers the session's registered app tools' calls until `signal` aborts (or the
   * session is deleted): one waiting when the stream (re)connects, from its snapshot,
   * then each `app_tool` event as it comes. The host ends a stream every five minutes;
   * this reconnects where it left off. */
  async answerAppTools(id, { signal } = {}) {
    // Calls being answered, or answered and not yet done: one announcement is answered
    // once (a snapshot and an event can name the same call).
    const answered = new Set();
    const handle = async (event) => {
      if (!event || event.type !== 'app_tool' || answered.has(event.call)) return;
      if (!this.tools.byName.has(event.name)) return;
      answered.add(event.call);
      try {
        try {
          await this.appToolResult(id, event.call, await this.tools.answer(event));
        } catch (error) {
          if (!(error instanceof SpindleError) || error.status !== 400) throw error;
          await this.appToolResult(id, event.call, refusedAnswer(error));
        }
      } catch (error) {
        // 409: no longer waiting (stopped, or answered elsewhere): it is asked again,
        // as a new announcement.
        answered.delete(event.call);
        if (!(error instanceof SpindleError) || error.status !== 409) throw error;
      }
    };
    let lastEventId;
    try {
      while (!signal?.aborted) {
        for await (const { id: at, kind, data } of this.events(id, { lastEventId, signal })) {
          if (at) lastEventId = at;
          if (kind === 'deleted') return;
          if (kind === 'snapshot') await Promise.all((data.app_tools ?? [data.app_tool]).map(handle));
          if (kind === 'event' && data.type === 'app_tool_done') answered.delete(data.call);
          else if (kind === 'event') await handle(data);
        }
      }
    } catch (error) {
      if (!signal?.aborted) throw error;
    }
  }

  async request(method, path, body, { raw = false } = {}) {
    const headers = { Authorization: `Bearer ${this.token}` };
    const binary = body instanceof Uint8Array;
    if (body !== undefined) headers['Content-Type'] = binary ? 'application/octet-stream' : 'application/json';
    const response = await fetch(this.base + path, {
      method,
      headers,
      body: body === undefined || binary ? body : JSON.stringify(body),
    });
    if (!response.ok) throw await refusal(response);
    return raw ? Buffer.from(await response.arrayBuffer()) : response.json();
  }

  info() { return this.request('GET', '/v1'); }
  async sessions() { return (await this.request('GET', '/v1/sessions')).sessions; }
  session(id) { return this.request('GET', `/v1/sessions/${id}`); }
  /** Admits a command and returns its receipt; reuse requestId to retry safely. */
  command(kind, fields = {}, requestId = randomUUID().replaceAll('-', '')) {
    return this.request('POST', '/v1/commands', { request_id: requestId, kind, ...fields });
  }
  receipt(requestId) { return this.request('GET', `/v1/receipts/${requestId}`); }
  messages(id, revision) { return this.request('GET', `/v1/sessions/${id}/messages${query({ revision })}`); }
  files(id, revision) { return this.request('GET', `/v1/sessions/${id}/files${query({ revision })}`); }
  read(id, path, revision) { return this.request('GET', `/v1/sessions/${id}/file${query({ path, revision })}`, undefined, { raw: true }); }
  async targets(id) { return (await this.request('GET', `/v1/sessions/${id}/targets`)).targets; }
  steer(id, text) { return this.request('POST', `/v1/sessions/${id}/steer`, { text }); }
  /** The app state as of a revision (the head by default): { head, revision, state }. */
  state(id, revision) { return this.request('GET', `/v1/sessions/${id}/state${query({ revision })}`); }
  /** What the session's owner left unfinished and what is safe to resume (`wake`). */
  recovery(id) { return this.request('GET', `/v1/sessions/${id}/recovery`); }
  /** The session's tasks with their head state: { tasks: [{ task, dir, status }] }. */
  tasks(id) { return this.request('GET', `/v1/sessions/${id}/tasks`); }
  taskStart(id, task, prompt, options = {}, requestId) { return this.command('task_start', { session: id, task, prompt, ...options }, requestId); }
  taskWait(id, task, timeoutS = 600) { return this.request('POST', `/v1/sessions/${id}/tasks/${encodeURIComponent(task)}/wait`, { timeout_s: timeoutS }); }
  taskStop(id, task) { return this.request('POST', `/v1/sessions/${id}/tasks/${encodeURIComponent(task)}/stop`, {}); }
  /** A task's messages form. */
  taskMessages(id, task, revision) { return this.request('GET', `/v1/sessions/${id}/tasks/${task}/messages${query({ revision })}`); }
  /** A state change for the running turn (between turns send a `state` command). */
  queueState(id, patch) { return this.request('POST', `/v1/sessions/${id}/state`, { patch }); }
  /** Uploads bytes for a later `say`'s attachments; resolves with { digest, size }. */
  upload(id, data) { return this.request('POST', `/v1/sessions/${id}/upload`, data); }

  /** Async iterator of { id, kind, data } from the session's event stream: kind is
   * 'snapshot', 'event' or 'deleted'. Pass the last id to resume after it. */
  async *events(id, { lastEventId, signal } = {}) {
    const headers = { Authorization: `Bearer ${this.token}` };
    if (lastEventId) headers['Last-Event-ID'] = lastEventId;
    const response = await fetch(`${this.base}/v1/sessions/${id}/events`, { headers, signal });
    if (!response.ok) throw await refusal(response);
    const decoder = new TextDecoder();
    let buffer = '';
    for await (const chunk of response.body) {
      buffer += decoder.decode(chunk, { stream: true });
      let end;
      while ((end = buffer.indexOf('\n\n')) >= 0) {
        const block = buffer.slice(0, end);
        buffer = buffer.slice(end + 2);
        const frame = {};
        for (const line of block.split('\n')) {
          if (!line || line.startsWith(':')) continue;
          const colon = line.indexOf(':');
          frame[line.slice(0, colon)] = line.slice(colon + 1).replace(/^ /, '');
        }
        if (frame.data !== undefined) yield { id: frame.id, kind: frame.event || 'message', data: JSON.parse(frame.data) };
      }
    }
  }
}

// A SpindleError from a refused request: the v1 envelope's code, or http_STATUS.
async function refusal(response) {
  const text = await response.text();
  let envelope;
  try { envelope = JSON.parse(text); } catch { /* not JSON */ }
  if (!envelope || typeof envelope !== 'object' || Array.isArray(envelope)) envelope = { error: text };
  const { error, code, ...data } = envelope;
  return new SpindleError(code || `http_${response.status}`, error ?? text, response.status, data);
}

// Percent-encoded (a space is %20), so no server has to read `+` as a space.
function query(params) {
  const entries = Object.entries(params).filter(([, value]) => value !== undefined && value !== null);
  return entries.length ? `?${entries.map(([key, value]) => `${encodeURIComponent(key)}=${encodeURIComponent(value)}`).join('&')}` : '';
}
