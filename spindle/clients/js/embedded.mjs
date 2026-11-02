// Spindle inside a JS/TS app: the embedded package (spindle-embed, wasm) behind the
// same API as StdioClient (docs/API.md), with no separate process. Runs in Node 20+,
// Electron and browser workers; it imports nothing from Node.
//
// The wasm module holds the sessions and their stores; this client does what they
// need from outside: model requests (`fetch`, with the app's credentials added to
// the request's non-secret headers; Spindle never sees a key), app tools, the
// command runner, and timers. Every model request is sent at most once; one whose
// outcome a crash or stop left unknown is never sent again by `resume`.
import { Emitter, SpindleError, Tools, refusedAnswer } from './common.mjs';
import { lockSession } from './opfs.mjs';

export { SpindleError } from './common.mjs';
export const API_VERSION = 1;

const DEADLINES = { firstByte: 180_000, idle: 180_000, total: 600_000 };
/** Longest refusal body read (as the native transport keeps it). */
const MAX_REFUSAL = 4096;
/** How long a stopped command's runner has to answer (with what it printed) before the
 * client answers for it, as stopped without its output. */
const STOP_GRACE_MS = 15_000;
/** How long a remote runner waits for a stopped command's own answer from its host. */
const REMOTE_STOP_MS = 10_000;
/** Anthropic's Messages API refuses a request a browser sends (it carries an Origin)
 * unless the request says it means to call the API from there; this client may run in
 * one, and the header changes nothing elsewhere. */
const ANTHROPIC_MESSAGES = 'anthropic-messages';
const BROWSER_ACCESS = { 'anthropic-dangerous-direct-browser-access': 'true' };

function base64ToBytes(text) {
  const binary = atob(text);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

/** Bytes as a string of one character per byte (and back), so byte patterns can be
 * found with string search. */
function binaryOf(bytes) {
  let binary = '';
  for (let i = 0; i < bytes.length; i += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return binary;
}

function bytesOfBinary(binary) {
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

function bytesToBase64(bytes) {
  return btoa(binaryOf(bytes));
}

/** A header value the app adds for a grant: visible ASCII, spaces and tabs (so the bytes
 * sent are the text's own, and an echo of them can be found), at most 8 KiB. */
const ADDED_VALUE = /^[\t\x20-\x7e]{1,8192}$/;
/** The shortest secret (a value, or an auth scheme's credentials in it) the client
 * looks for in what comes back, as native code mode does. */
const MIN_SECRET = 4;
const REDACTED = '[redacted]';

/** What of each added header value is secret: the value, and an auth scheme's
 * credentials (`Bearer TOKEN`: TOKEN too), so an echo of either is found. */
function secretsOf(values) {
  const out = [];
  for (const value of values) {
    out.push(value);
    const scheme = /^[!#$%&'*+.^_`|~0-9A-Za-z-]+[ \t]+(\S[\s\S]*)$/.exec(value);
    if (scheme) out.push(scheme[1]);
  }
  return out;
}

/** The forms a secret is commonly echoed in, as native code mode finds them
 * (store/src/code_http.rs, `needles`): as is, JSON-escaped (also with `\/`),
 * percent-encoded (either hex case) and base64 (standard and URL-safe, at each of the
 * three alignments it can have inside a longer encoding). Longest first. The secrets
 * are ASCII, so each character is one byte. */
function needlesOf(secrets) {
  const out = new Set();
  for (const secret of secrets) {
    if (secret.length < MIN_SECRET) continue;
    const escaped = JSON.stringify(secret).slice(1, -1);
    const percent = (upper) => [...secret].map((ch) => {
      if (/[A-Za-z0-9._~-]/.test(ch)) return ch;
      const hex = ch.charCodeAt(0).toString(16).padStart(2, '0');
      return `%${upper ? hex.toUpperCase() : hex}`;
    }).join('');
    const forms = [secret, escaped, escaped.replaceAll('/', '\\/'), percent(true), percent(false)];
    for (let shift = 0; shift < 3; shift++) {
      const encoded = btoa('\0'.repeat(shift) + secret);
      // Characters that depend only on the secret's own bytes.
      const start = shift === 0 ? 0 : 4;
      const end = Math.floor((shift + secret.length) / 3) * 4;
      if (end >= start + 8) {
        const core = encoded.slice(start, end);
        forms.push(core, core.replaceAll('+', '-').replaceAll('/', '_'));
      }
    }
    for (const form of forms) if (form.length >= MIN_SECRET) out.add(form);
  }
  return [...out].sort((a, b) => b.length - a.length);
}

/** `text` (one character per byte) with every echo of a secret replaced. */
function scrubbed(text, needles) {
  for (const needle of needles) if (text.length >= needle.length) text = text.split(needle).join(REDACTED);
  return text;
}

/** How much of the end of `text` begins `needle` (its longest such prefix): a body cut
 * short can end partway into a secret. */
function partialEnd(text, needle) {
  const joined = `${needle}\uffff${text.slice(-(needle.length - 1))}`;
  const longest = new Int32Array(joined.length);
  for (let i = 1; i < joined.length; i++) {
    let k = longest[i - 1];
    while (k > 0 && joined[i] !== joined[k]) k = longest[k - 1];
    if (joined[i] === joined[k]) k += 1;
    longest[i] = k;
  }
  return longest[joined.length - 1];
}

/** A runner's file write as the package takes it: `{path, base64, executable}`. */
function writeOf(file) {
  const bytes = typeof file.content === 'string' ? new TextEncoder().encode(file.content) : file.content ?? new Uint8Array();
  return { path: file.path, base64: bytesToBase64(bytes), executable: Boolean(file.executable) };
}

/** What a host's refusal says: the HTTP API's error envelope is `{error, code}`. */
const refusal = (body) => (typeof body?.error === 'string' ? body.error : null);

/** A command runner whose commands a Spindle host runs (`spindle serve`, `/v1/run`):
 * each over a copy of the call's workspace, in that host's sandbox, its changed
 * files returned so a rewind still covers them. `token` is any token that reaches the
 * runner routes: a runner token, or the browser token one-click connect gives. */
export function remoteRunner({ url, token, fetch = globalThis.fetch }) {
  const base = url.replace(/\/+$/, '');
  const headers = { authorization: `Bearer ${token}`, 'content-type': 'application/json' };
  return {
    kind: 'remote',
    async identity() {
      // A host that does not answer is told apart from a slow one soon.
      const response = await fetch(`${base}/v1/runner`, { headers, signal: AbortSignal.timeout(10_000) });
      const body = await response.json().catch(() => ({}));
      if (!response.ok) throw new SpindleError(body.code || 'runner', refusal(body) || `runner answered ${response.status}`, response.status);
      return body.helper;
    },
    async run({ command, timeoutS, network, files, links, directories, read, signal }) {
      if (signal?.aborted) return { output: '', success: false, failure: 'the command was stopped before it ran' };
      const upload = files.map((file) => ({ path: file.path, base64: bytesToBase64(read(file.object)), executable: file.executable }));
      // The run's name: a Stop tells the host to stop the command, whose own answer then
      // says what it printed until then (a host that predates /v1/run/ID/stop says 404,
      // and the command runs out its time there). That answer is waited for
      // REMOTE_STOP_MS at most. Whether it stopped is the host's to tell: GET /v1/run/ID.
      const run = crypto.randomUUID();
      const ended = new AbortController();
      let waited = null;
      const stop = () => {
        fetch(`${base}/v1/run/${run}/stop`, { method: 'POST', headers, body: '{}', signal: AbortSignal.timeout(10_000) }).catch(() => {});
        waited = setTimeout(() => ended.abort(), REMOTE_STOP_MS);
      };
      signal?.addEventListener('abort', stop, { once: true });
      try {
        const response = await fetch(`${base}/v1/run`, {
          method: 'POST', headers, signal: ended.signal,
          body: JSON.stringify({ command, timeout_s: timeoutS, network, files: upload, links, directories, run }),
        });
        const body = await response.json().catch(() => ({}));
        if (!response.ok) return { output: '', success: false, failure: refusal(body) || `the runner answered HTTP ${response.status}` };
        return body;
      } finally {
        clearTimeout(waited);
        signal?.removeEventListener('abort', stop);
      }
    },
  };
}

/** The claim at `path` in a key that is a token (a JWT's payload), as a string: what a
 * provider's header carries from it (a ChatGPT sign-in's account). */
function claimOf(key, path, provider) {
  let value;
  try {
    const part = key.split('.')[1].replaceAll('-', '+').replaceAll('_', '/');
    const bytes = Uint8Array.from(atob(part.padEnd(Math.ceil(part.length / 4) * 4, '=')), (c) => c.charCodeAt(0));
    value = path.reduce((at, step) => at?.[step], JSON.parse(new TextDecoder().decode(bytes)));
  } catch {
    value = undefined;
  }
  if (typeof value !== 'string' || !value) {
    throw new Error(`the sign-in for ${provider} does not name its ${path.at(-1)}: sign in again`);
  }
  return value;
}

/** Drives the embedded package. Calls may overlap; each returns a Promise of its
 * result (prompt, resume, retry, rewind and taskStart resolve when the turn ends).
 * Events are emitted as 'event' with (event, session, requestId), as StdioClient
 * emits them.
 *
 * Options:
 *   wasm         the package's wasm-bindgen module (Node: its `--target nodejs` glue;
 *                browsers: the `--target web` module after `init()`).
 *   storage      'memory' (default), 'node' (directories: give `fs` (node:fs) and
 *                `platform`; `dir`s are resolved with `resolveDir`; embedded-node.mjs
 *                does both) or 'opfs' (a browser Dedicated Worker: give `pool`, an
 *                async function opening it, such as `() => openOpfsPool()` from
 *                opfs.mjs; `dir`s are absolute OPFS paths such as /sessions/a).
 *                An OPFS session's exclusive Web Lock is taken before its files are
 *                touched (a call is refused with kind 'locked' while another worker
 *                or tab holds it) and given back once its stores close.
 *   storageOptions  node: { journal: 'wal'|'delete', note } (`note` names this owner
 *                in lock files; default `spindle-embed`).
 *   catalog      providers in Dext's catalog format (default: Dext's built-ins).
 *   credentials  async ({ provider, model, endpoint, contract, headers, auth }) =>
 *                the key (a string or `{ key }`: the client fills the provider's
 *                authentication headers, `auth`, which the package derives from its
 *                catalog), or headers to add as given. Asked before every fresh model
 *                request, before it is reserved; throwing refuses it (nothing is sent).
 *   grantHeaders async ({ grant, url, method, names }) => { [name]: value } for each of
 *                `names`: the values of the headers an HTTP grant adds (`setHttpGrants`),
 *                asked before each request a script makes under it. Each value is
 *                visible ASCII; it goes only in that request, and every echo of it (or
 *                of an auth scheme's credentials in it) is taken out of what comes back
 *                before the package or the script sees it (one whose secret is under 4
 *                characters, which could not be found, is refused). Throwing refuses
 *                the request (nothing is sent).
 *   fetch        the fetch to send model requests, and scripts' requests to their
 *                grants, with (default globalThis.fetch).
 *   runner       runs `bash`: `{ app, remote }`, each optional, or one runner (a
 *                `remoteRunner(...)` serves `runner: 'remote'` sessions, any other
 *                `runner: 'app'` ones). A runner is { run: async ({ command, timeoutS,
 *                network, files, links, directories, read, signal }) => ({ output,
 *                success, writes, deletes, links, directories, failure }) };
 *                `read(object)` gives a file's bytes. `signal` ends when the user stops
 *                the command: stop it and still answer, with what it printed until then
 *                (that answer is recorded as stopped). An app runner that sandboxes its
 *                commands says so with `identity()` (64 hex digits, as a remote
 *                runner's): they then count as isolated. `connectRunner` changes them.
 *   deadlines    { firstByte, idle, total } in ms for model requests. */
/** Why an app tool handler's `call.signal` ended: an AbortError (as `fetch` and
 * `throwIfAborted` expect) with the call's `outcome` (`stopped`, `no_answer`, or `closed`
 * when the client closed). */
const settled = (outcome) => Object.assign(new DOMException(`the call was settled without this answer (${outcome})`, 'AbortError'), { outcome });

export class EmbeddedClient extends Emitter {
  #wasm;
  #embedded;
  #pending = new Map();
  #next = 0;
  #works = new Map();
  /** App tool calls being answered: `${session}\0${call}` (as their `app_tool` event
   * names them) -> the AbortController whose signal their handler holds (`#answer`),
   * ended when the package records the call without that answer (`#settleApp`). */
  #appCalls = new Map();
  #auth = new Map();
  #closed = null;
  #opfs = null;

  constructor({ wasm, storage = 'memory', storageOptions = {}, fs, platform, pool, catalog, credentials, grantHeaders, fetch = globalThis.fetch, runner, deadlines = {}, resolveDir = (dir) => dir } = {}) {
    super();
    if (!wasm?.Embedded) throw new TypeError('EmbeddedClient needs the spindle-embed wasm module');
    if (storage === 'node') {
      if (!fs) throw new TypeError("storage 'node' needs `fs` (node:fs)");
      wasm.installNodeFs(fs, platform ?? 'linux');
    }
    if (storage === 'opfs') {
      if (typeof pool !== 'function') throw new TypeError("storage 'opfs' needs `pool`, an async function opening it");
      this.#opfs = { open: pool, fs: null, owned: new Set(), owning: new Map(), releasing: new Map() };
    }
    this.#wasm = wasm;
    this.#embedded = new wasm.Embedded(storage, JSON.stringify(storageOptions), catalog ? JSON.stringify(catalog) : undefined);
    Object.assign(this, { credentials, grantHeaders, fetchImpl: fetch, resolveDir });
    this.deadlines = { ...DEADLINES, ...deadlines };
    this.tools = new Tools();
    this.runners = {};
    const given = runner?.run ? { [runner.kind === 'remote' ? 'remote' : 'app']: runner } : runner ?? {};
    this.ready = Promise.all(['app', 'remote'].map((kind) => this.connectRunner(kind, given[kind] ?? null)))
      .then(() => {}, (error) => { this.emit('runner_error', error); });
  }

  /** Connects (or, with null, disconnects) the runner for `kind` ('app' or 'remote')
   * sessions, binding the sandbox identity it declares. A remote runner must declare
   * one; an app runner without one runs commands unisolated. */
  async connectRunner(kind, runner) {
    if (kind !== 'app' && kind !== 'remote') throw new TypeError(`no runner kind ${kind}`);
    const helper = runner && (kind === 'remote' || runner.identity) ? await runner.identity() : null;
    this.runners[kind] = runner;
    if (kind === 'remote') this.#embedded.setRunner(helper ?? null);
    else this.#embedded.setAppRunner(helper ?? null);
  }

  /** The sites scripts' `http()` may reach: grants as a native code policy's `http`
   * list ({name, url, risk, methods?, timeout_seconds?, max_response_bytes?,
   * headers?}). The package checks each request against them; this client sends it with
   * `fetch`, following no redirect. A grant's `headers` name those the app adds, each
   * `{"app": true}` (`{"Authorization": {"app": true}}`): their values come from
   * `grantHeaders` as each request goes, never from the package, and a script may not
   * set them. `[]` grants none, as by default. */
  setHttpGrants(grants) {
    try {
      this.#embedded.setHttpGrants(JSON.stringify(grants ?? []));
    } catch (error) {
      throw new SpindleError(error.code || 'failed', error.message);
    }
  }

  /** The provider catalog the sessions use, in Dext's format (no credentials). */
  catalog() { return JSON.parse(this.#embedded.catalog()); }

  /** The efforts `provider`'s `model` takes, `default` first: [{effort, label, short}],
   * `label` saying what a request on that route then asks for. */
  efforts(provider, model) { return JSON.parse(this.#embedded.efforts(provider, model)); }

  /** Registers an app tool (as StdioClient's `tool`); sessions this client creates offer it.
   * Its `run(input, call)` learns whose call it is (`call.session`, the session's dir; a
   * task's call is named `tasks/KEY/...`) and when to stop: `call.signal` ends when the
   * package settles the call without its answer (the turn or task that asked was stopped,
   * or the tool's `timeoutS` passed, or this client closed; a background task's calls go
   * on when its parent's turn stops), its reason an AbortError whose `outcome` says which
   * (`stopped`, `no_answer`, `closed`); the answer is then not sent. */
  tool(definition) { this.tools.add(definition); return this; }

  /** OPFS: holds `dir`'s Web Lock (and the pool) before its files are touched. */
  async #own(dir) {
    const opfs = this.#opfs;
    await opfs.releasing.get(dir);
    if (this.#closed) throw new SpindleError('closed', this.#closed);
    if (opfs.owned.has(dir)) return;
    if (!opfs.owning.has(dir)) {
      const owning = (async () => {
        const release = await lockSession(dir);
        if (!release) throw new SpindleError('locked', `another worker or tab holds the session ${dir}`);
        try {
          if (!opfs.fs) {
            opfs.fs = await opfs.open();
            this.#wasm.installNodeFs(opfs.fs, 'opfs');
          }
        } catch (error) {
          await release();
          throw error;
        }
        try {
          if (this.#closed) throw new SpindleError('closed', this.#closed);
          this.#embedded.ownSession(dir, () => {
            opfs.owned.delete(dir);
            const released = release().finally(() => {
              if (opfs.releasing.get(dir) === released) opfs.releasing.delete(dir);
            });
            opfs.releasing.set(dir, released);
          });
        } catch (error) {
          await release();
          throw new SpindleError(error.code || 'invalid', error.message);
        }
        opfs.owned.add(dir);
      })();
      opfs.owning.set(dir, owning.finally(() => opfs.owning.delete(dir)));
    }
    return opfs.owning.get(dir);
  }

  /** Sends one request; resolves with its result. */
  call(method, params = {}) {
    const id = ++this.#next;
    if (this.#closed) return Promise.reject(new SpindleError('closed', this.#closed));
    if (this.#opfs && typeof params.dir === 'string' && method !== 'close') {
      // A running task's label (`dir#key`) lives in its session's place.
      return this.#own(params.dir.split('#')[0]).then(() => this.#send(id, method, params));
    }
    return this.#send(id, method, params);
  }

  #send(id, method, params) {
    if (this.#closed) return Promise.reject(new SpindleError('closed', this.#closed));
    let line;
    try {
      line = JSON.stringify({ jsonrpc: '2.0', id, method, params });
    } catch (error) {
      return Promise.reject(new SpindleError('unsendable', `the request is not JSON: ${error.message}`));
    }
    const reply = new Promise((resolve, reject) => this.#pending.set(id, { resolve, reject }));
    reply.requestId = id;
    this.#frames(this.#embedded.handle(line));
    return reply;
  }

  #frames(json) {
    for (const frame of typeof json === 'string' ? JSON.parse(json) : json) {
      if (frame.method === 'event') {
        const { session, request, event } = frame.params;
        if (event?.type === 'app_tool' && this.tools.byName.has(event.name)) this.#answer(session, event);
        if (event?.type === 'app_tool_done') this.#settleApp(session, event);
        this.emit('event', event, session, request);
      } else if (frame.method === 'work') {
        this.#work(frame.params);
      } else if (frame.method === 'work_cancel') {
        this.#cancel(frame.params.session, frame.params.id);
      } else if (frame.method === 'work_stop') {
        this.#stopRun(frame.params.session, frame.params.id);
      } else if (frame.id != null && this.#pending.has(frame.id)) {
        const waiting = this.#pending.get(frame.id);
        this.#pending.delete(frame.id);
        if (frame.error) {
          const data = frame.error.data || {};
          waiting.reject(new SpindleError(data.code || 'internal', frame.error.message, frame.error.code, data));
        } else {
          waiting.resolve(frame.result);
        }
      }
    }
  }

  #deliver(session, id, answer) {
    const key = `${session}\u0000${id}`;
    const work = this.#works.get(key);
    if (!work || work.cancelled) return;
    this.#works.delete(key);
    let frames;
    try {
      frames = this.#embedded.deliver(session, id, JSON.stringify(answer));
    } catch (error) {
      // Refused (no answer to that work): nothing changed. Timers call this, so the
      // error goes to listeners, never up into the event loop.
      this.emit('work_error', error, answer, session);
      return;
    }
    this.#frames(frames);
  }

  #cancel(session, id) {
    const key = `${session}\u0000${id}`;
    const work = this.#works.get(key);
    if (!work) return;
    work.cancelled = true;
    this.#works.delete(key);
    work.controller.abort();
    clearTimeout(work.timer);
  }

  /** The user stopped the command a work runs: its runner is told (its signal ends)
   * and its answer, with what the command printed until then, still goes to the
   * package, which records the command as stopped. A runner that has not answered
   * STOP_GRACE_MS after the stop is answered for, as stopped without its output. */
  #stopRun(session, id) {
    const work = this.#works.get(`${session}\u0000${id}`);
    if (!work || work.kind !== 'run' || work.stopping) return;
    work.stopping = true;
    work.controller.abort();
    work.timer = setTimeout(() => this.#deliver(session, id, {
      kind: 'ran', output: '', success: false,
      failure: `its runner had not answered ${STOP_GRACE_MS / 1000} s after the stop`,
    }), STOP_GRACE_MS);
  }

  #work({ session, id, work }) {
    const key = `${session}\u0000${id}`;
    const entry = { kind: work.kind, controller: new AbortController(), cancelled: false, timer: null, call: work.call };
    this.#works.set(key, entry);
    const run = async () => {
      switch (work.kind) {
        case 'authorize': return this.#authorize(session, id, work);
        case 'fetch': return this.#fetch(session, id, work, entry);
        case 'run': return this.#run(session, id, work, entry);
        case 'wait':
          entry.timer = setTimeout(() => this.#deliver(session, id, { kind: 'waited' }), work.ms);
          return undefined;
        case 'app':
          entry.timer = setTimeout(() => this.#deliver(session, id, { kind: 'app_timeout' }), work.timeout_s * 1000);
          return undefined;
        default:
          throw new Error(`unknown work ${work.kind}`);
      }
    };
    // Work runs after the frames that asked for it are handled.
    queueMicrotask(() => run().catch((error) => this.emit('work_error', error, work, session)));
  }

  async #authorize(session, id, work) {
    try {
      this.#auth.set(session, await this.#headers(work));
      this.#deliver(session, id, { kind: 'authorized' });
    } catch (error) {
      this.#deliver(session, id, { kind: 'unauthorized', error: String(error?.message ?? error) });
    }
  }

  /** A model request's headers from the app's credentials (throws when it has none). */
  async #headers({ provider, model, endpoint, contract, headers, auth = {}, key_slot: slot, claims = [] }) {
    if (!this.credentials) throw new Error('no credentials were given to this client');
    const given = (await this.credentials({ provider, model, endpoint, contract, headers: { ...headers }, auth: { ...auth } })) || {};
    // A key (or `{ key }`) fills the provider's own authentication headers, as the
    // package derived them from its catalog, with any claim of the key they carry (a
    // ChatGPT sign-in's account); anything else is headers to add as given.
    const key = typeof given === 'string' ? given : Object.keys(given).length === 1 && 'key' in given ? given.key : null;
    let added = given;
    if (key !== null) {
      const filled = claims.map(({ slot: place, path }) => [place, claimOf(key, path, provider)]);
      // Functions as replacements: a `$` in a key is text, not a replacement pattern.
      added = Object.fromEntries(Object.entries(auth).map(([name, template]) => {
        for (const [place, value] of filled) template = template.replaceAll(place, () => value);
        return [name, template.replaceAll(slot, () => key)];
      }));
    }
    return { ...headers, ...(contract === ANTHROPIC_MESSAGES && BROWSER_ACCESS), ...added };
  }

  /** One request of a script run by the app's runner (the code-mode bridge, as native
   * run_code scripts ask): its reply. The package checks and journals each call; this
   * runs them: a model request with the app's credentials and fetch, an app tool with
   * its handler. */
  async code(dir, request, { signal } = {}) {
    const step = await this.call('code_call', { dir: this.#dir(dir), request });
    if (step.reply) return step.reply;
    const { call, children, parallel = children.length } = step;
    // At most `parallel` at once, each answered as it ends; once the reply is in (the
    // last answer, or a journal failure), no more start.
    // Stopped (the command ended), none starts: those not run stay uncertain, as the
    // journal keeps any child reserved and never answered (never sent again by itself).
    let reply = null;
    let failure = null;
    const queue = [...children];
    const lane = async () => {
      for (let child = queue.shift(); child && !reply && !signal?.aborted; child = queue.shift()) {
        try {
          const answer = child.kind === 'model' ? await this.#codeModel(child, signal)
            : child.kind === 'http' ? await this.#codeHttp(child, signal) : await this.#codeTool(child, signal, dir);
          let answered;
          try {
            answered = await this.call('code_answer', { dir: this.#dir(dir), call, index: child.index, answer });
          } catch (error) {
            if (error.kind !== 'bad_request' || child.kind !== 'app') throw error;
            // A tool's answer the package would not take: kept as uncertain instead.
            answered = await this.call('code_answer', { dir: this.#dir(dir), call, index: child.index,
              answer: { unknown: true, content: `the app's answer was refused: ${error.message}` } });
          }
          if (answered.reply) reply = answered.reply;
        } catch (error) {
          failure ??= error;
        }
      }
    };
    await Promise.all(Array.from({ length: Math.max(1, Math.min(parallel, children.length)) }, lane));
    if (reply) return reply;
    if (signal?.aborted) throw new SpindleError('stopped', 'the command ended before its calls were answered');
    throw failure ?? new SpindleError('failed', 'the script call got no reply');
  }

  async #codeModel(child, signal) {
    if (signal?.aborted) return { error: 'stopped before it was sent', sent: false };
    let headers;
    try {
      headers = await this.#headers(child);
    } catch (error) {
      return { error: String(error?.message ?? error), sent: false };
    }
    // The client's total deadline for a model request, and the caller's stop.
    const deadline = AbortSignal.timeout(this.deadlines.total);
    try {
      const response = await this.fetchImpl(child.endpoint, {
        method: 'POST', signal: signal ? AbortSignal.any([signal, deadline]) : deadline, redirect: 'error',
        headers: { ...headers, 'content-type': 'application/json', accept: 'text/event-stream' },
        body: new TextEncoder().encode(JSON.stringify(child.body)),
      });
      return { status: response.status, body: await response.text() };
    } catch (error) {
      // Whether it left is not known: the store keeps it uncertain.
      return { error: `the request failed: ${error?.message ?? error}`, sent: true };
    }
  }

  /** The values of the headers `child`'s grant adds (`app_headers` names them), from
   * `grantHeaders`: `{ added, needles }` (the forms their secrets are found in), or
   * `{ refused }`, and then nothing is sent. */
  async #added(child) {
    const names = Array.isArray(child.app_headers) ? child.app_headers : [];
    if (!names.length) return { added: {}, needles: [] };
    const grant = child.grant ?? '?';
    if (typeof this.grantHeaders !== 'function') {
      return { refused: `grant ${grant} adds headers (${names.join(', ')}) that this app does not supply` };
    }
    let given;
    try {
      given = await this.grantHeaders({ grant, url: child.url, method: child.method, names: [...names] });
    } catch (error) {
      return { refused: String(error?.message ?? error) };
    }
    const byName = new Map(Object.entries(given ?? {}).map(([name, value]) => [name.toLowerCase(), value]));
    const added = {};
    for (const name of names) {
      const value = byName.get(name.toLowerCase());
      const text = typeof value === 'string' ? value.trim() : '';
      if (!ADDED_VALUE.test(text)) return { refused: `the app gave no value it can send for header ${name} of grant ${grant}` };
      // What it cannot find in what comes back, it does not send.
      if (secretsOf([text]).some((part) => part.length < MIN_SECRET)) {
        return { refused: `the value for header ${name} of grant ${grant} is too short to keep out of what comes back` };
      }
      added[name] = text;
    }
    return { added, needles: needlesOf(secretsOf(Object.values(added))) };
  }

  /** A request a grant allows, as the package froze it: no cookies, no redirect
   * followed (one is reported as the server's answer), the grant's timeout, at most
   * its response size, with the headers the grant adds; every echo of their values
   * is taken out of what comes back. */
  async #codeHttp(child, signal) {
    if (child.body != null && (child.method === 'GET' || child.method === 'HEAD')) {
      return { error: `a ${child.method} request carries no body here; nothing was sent`, sent: false };
    }
    if (signal?.aborted) return { error: 'stopped before it was sent', sent: false };
    const { added, needles, refused } = await this.#added(child);
    if (refused) return { error: `${refused}; nothing was sent`, sent: false };
    if (signal?.aborted) return { error: 'stopped before it was sent', sent: false };
    const hide = (text) => (needles.length ? scrubbed(String(text), needles) : String(text));
    // Checked before sending (as fetch would check them), so headers it would refuse are
    // refused with nothing sent; given to `fetch` as a plain object, as before.
    let headers;
    try {
      const checked = new Headers(child.headers ?? {});
      for (const [name, value] of Object.entries(added)) checked.set(name, value);
      headers = Object.fromEntries(checked);
    } catch (error) {
      return { error: `the request's headers cannot be sent (${hide(error?.message ?? error)}); nothing was sent`, sent: false };
    }
    const deadline = AbortSignal.timeout(child.timeout_seconds * 1000);
    let response;
    try {
      response = await this.fetchImpl(child.url, {
        method: child.method, headers, credentials: 'omit', redirect: 'manual', cache: 'no-store',
        body: child.body == null ? undefined : new TextEncoder().encode(child.body),
        signal: signal ? AbortSignal.any([signal, deadline]) : deadline,
      });
    } catch (error) {
      return { error: `the request failed: ${hide(error?.message ?? error)}`, sent: true };
    }
    if (response.type === 'opaqueredirect' || (response.status >= 300 && response.status < 400 && response.headers.has('location'))) {
      response.body?.cancel().catch(() => {});
      return { error: 'the server answered with a redirect, which is not followed here; ask for the address it leads to', answered: true };
    }
    const limit = child.max_response_bytes;
    // A secret is taken out whole before the body is cut, so it is read this far past.
    const look = needles[0]?.length ?? 0;
    const parts = [];
    let size = 0;
    let cut = null;
    const reader = response.body?.getReader();
    try {
      while (reader && size <= limit + look) {
        const { value, done } = await reader.read();
        if (done) break;
        parts.push(value);
        size += value.length;
      }
    } catch (error) {
      // The body was cut off (a timeout, a stop): what came is the answer.
      cut = `the response body was cut off: ${hide(error?.message ?? error)}`;
    }
    reader?.cancel().catch(() => {});
    // Read short of its end (the limit, or a cut): it may end partway into a secret.
    const short = size > limit + look || cut !== null;
    let body = new Uint8Array(size);
    let at = 0;
    for (const part of parts) {
      body.set(part, at);
      at += part.length;
    }
    if (needles.length) {
      let text = scrubbed(binaryOf(body), needles);
      if (short) text = text.slice(0, text.length - Math.max(0, ...needles.map((needle) => partialEnd(text, needle))));
      body = bytesOfBinary(text);
    }
    const truncated = short || body.length > limit;
    const headersBack = {};
    for (const [name, value] of response.headers) headersBack[hide(name)] = hide(value);
    return { status: response.status, headers: headersBack, body_base64: bytesToBase64(body.subarray(0, limit)), truncated, ...(cut && { body_error: cut }) };
  }

  /** An app tool for a script, until its answer or the command's end (then uncertain:
   * the handler may still be at work). */
  async #codeTool(child, signal, session) {
    // Ended already: the tool never starts (the child stays as it was reserved).
    if (signal?.aborted) return { unknown: true, content: 'the command ended before the tool started' };
    // The handler gets the command's signal too: once the command ends, its answer is no
    // longer awaited (said unknown below), so what it would still do must not happen.
    const answer = this.tools.answer({ type: 'app_tool', call: child.call, name: child.name, input: child.input, again: false, rerun: 'safe', session, signal });
    if (!signal) return answer;
    let stop;
    const stopped = new Promise((resolve) => {
      stop = () => resolve({ unknown: true, content: 'the command ended while the tool ran' });
      signal.addEventListener('abort', stop, { once: true });
    });
    try {
      return await Promise.race([answer, stopped]);
    } finally {
      signal.removeEventListener('abort', stop);
    }
  }

  async #fetch(session, id, work, entry) {
    const { signal } = entry.controller;
    let ended = false;
    let timedOut = false;
    const deadline = (ms) => setTimeout(() => { timedOut = true; entry.controller.abort(); }, ms);
    const total = deadline(this.deadlines.total);
    let idle = deadline(this.deadlines.firstByte);
    const dropped = (reason) => this.#deliver(session, id, { kind: 'dropped', reason });
    try {
      const headers = {
        ...(this.#auth.get(session) || {}),
        'content-type': 'application/json',
        accept: 'text/event-stream',
        ...(work.gzip ? { 'content-encoding': 'gzip' } : {}),
      };
      const response = await this.fetchImpl(work.endpoint, { method: 'POST', headers, body: base64ToBytes(work.body), signal, redirect: 'error' });
      clearTimeout(idle);
      const contentType = response.headers.get('content-type');
      if (!response.ok) {
        const reader = response.body?.getReader();
        const parts = [];
        let size = 0;
        const until = Date.now() + 2000;
        while (reader && size < MAX_REFUSAL && Date.now() < until) {
          const { value, done } = await Promise.race([reader.read(), new Promise((r) => setTimeout(() => r({ done: true }), until - Date.now()))]);
          if (done) break;
          parts.push(value);
          size += value.length;
        }
        reader?.cancel().catch(() => {});
        const body = new Uint8Array(Math.min(size, MAX_REFUSAL));
        let at = 0;
        for (const part of parts) {
          body.set(part.subarray(0, body.length - at), at);
          at += Math.min(part.length, body.length - at);
          if (at >= body.length) break;
        }
        ended = true;
        this.#deliver(session, id, { kind: 'response', status: response.status, content_type: contentType, retry_after: response.headers.get('retry-after'), body: bytesToBase64(body) });
        return;
      }
      // Only an event stream is read, as the native transport reads it: any other body
      // is refused unread, by its type.
      if (contentType != null && contentType.split(';')[0].trim().toLowerCase() !== 'text/event-stream') {
        ended = true;
        response.body?.cancel().catch(() => {});
        this.#deliver(session, id, { kind: 'response', status: response.status, content_type: contentType });
        return;
      }
      const reader = response.body.getReader();
      for (;;) {
        idle = deadline(this.deadlines.idle);
        const { value, done } = await reader.read();
        clearTimeout(idle);
        if (done) break;
        if (entry.cancelled) return;
        const { frames, streaming } = JSON.parse(this.#embedded.chunk(session, id, value));
        this.#frames(frames);
        if (!streaming) {
          // The host stopped waiting (a refusal streamed, a cut, a stop): stop reading.
          ended = true;
          this.#works.delete(`${session}\u0000${id}`);
          reader.cancel().catch(() => {});
          return;
        }
      }
      ended = true;
      this.#deliver(session, id, { kind: 'response', status: response.status, content_type: contentType });
    } catch (error) {
      if (ended || entry.cancelled) return;
      dropped(timedOut ? 'deadline' : signal.aborted ? 'stopped' : 'transport');
    } finally {
      clearTimeout(total);
      clearTimeout(idle);
    }
  }

  async #run(session, id, work, entry) {
    let result;
    try {
      const runner = this.runners[work.runner === 'remote' ? 'remote' : 'app'];
      if (!runner) throw new Error(work.runner === 'remote' ? 'no remote runner (a Spindle host) is connected' : 'this app has no command runner');
      const read = (object) => this.#embedded.runnerFile(session, id, object);
      result = await runner.run({
        session, command: work.command, timeoutS: work.timeout_s, network: work.network,
        files: work.files, links: work.links, directories: work.directories,
        read, signal: entry.controller.signal,
      });
    } catch (error) {
      if (entry.cancelled) return;
      result = { output: '', success: false, failure: `the runner failed: ${error?.message ?? error}` };
    }
    if (entry.cancelled) return;
    clearTimeout(entry.timer);
    this.#deliver(session, id, {
      kind: 'ran',
      output: String(result.output ?? ''),
      success: Boolean(result.success),
      writes: (result.writes || []).map((file) => (file.base64 != null ? file : writeOf(file))),
      deletes: result.deletes || [],
      links: result.links || [],
      directories: result.directories || [],
      failure: result.failure ?? null,
    });
  }

  async #answer(session, event) {
    // The handler learns whose call it is (`call.session`, the session's dir) and when
    // the package stops waiting for it (`call.signal`, `#settleApp`).
    const key = `${session}\u0000${event.call}`;
    const controller = new AbortController();
    this.#appCalls.set(key, controller);
    let answer;
    try {
      answer = await this.tools.answer({ ...event, session, signal: controller.signal });
    } finally {
      if (this.#appCalls.get(key) === controller) this.#appCalls.delete(key);
    }
    // No one waits for it now: nothing to send (the package would refuse it).
    if (controller.signal.aborted) return;
    try {
      try {
        await this.call('app_tool_result', { dir: session, call: event.call, ...answer });
      } catch (error) {
        if (!['bad_request', 'unsendable'].includes(error?.kind)) throw error;
        await this.call('app_tool_result', { dir: session, call: event.call, ...refusedAnswer(error) });
      }
    } catch (error) {
      this.emit('app_tool_error', error, event, session);
    }
  }

  /** An app call the package recorded (`app_tool_done`). One still being answered here
   * was settled without its answer (stopped with its turn or task, say): no one waits for
   * it, so its handler is told (`call.signal`, ended with the outcome) and sends nothing. */
  #settleApp(session, { call, outcome }) {
    // Its work: this session's or one of its tasks' (`dir#KEY`), never another session's
    // that happens to share a call id (two imported from one bundle).
    for (const [key, work] of this.#works) {
      if (work.kind === 'app' && work.call === call && (key.startsWith(`${session}\u0000`) || key.startsWith(`${session}#`))) {
        clearTimeout(work.timer);
        this.#works.delete(key);
      }
    }
    this.#appCalls.get(`${session}\u0000${call}`)?.abort(settled(outcome ?? 'settled'));
  }

  #dir(dir) { return typeof dir === 'string' ? this.resolveDir(dir) : dir; }

  // The methods of the API, by name (params as in docs/API.md), as StdioClient has them.
  info() { return this.call('info'); }
  providers() { return this.call('providers'); }
  /** Tools registered with `tool()` are offered unless `settings.tools` says otherwise. */
  create(dir, provider, settings = {}) {
    const tools = this.tools.byName.size ? { tools: this.tools.definitions() } : {};
    return this.call('create', { dir: this.#dir(dir), provider, ...tools, ...settings });
  }
  open(dir) { return this.call('open', { dir: this.#dir(dir) }); }
  async closeSession(dir) {
    dir = this.#dir(dir);
    const result = await this.call('close', { dir });
    await this.#opfs?.releasing.get(dir);
    return result;
  }
  /** Gives back the free pages of the session's stores (its tasks' too): `{before,
   * after, stores}`, sizes in bytes. An open session closes first (as `closeSession`);
   * one with a turn or a background task running is refused. */
  compactSession(dir) { return this.call('compact', { dir: this.#dir(dir) }); }
  prompt(dir, text, { files, state, expectedHead } = {}) {
    return this.call('prompt', { dir: this.#dir(dir), text, ...(files && { files }), ...(state && { state }), ...(expectedHead != null && { expected_head: expectedHead }) });
  }
  resume(dir, { expectedHead, until } = {}) {
    return this.call('continue', { dir: this.#dir(dir), ...(until != null && { until }), ...(expectedHead != null && { expected_head: expectedHead }) });
  }
  retry(dir, { expectedHead } = {}) {
    return this.call('retry', { dir: this.#dir(dir), ...(expectedHead != null && { expected_head: expectedHead }) });
  }
  rewind(dir, revision, { expectedHead } = {}) {
    return this.call('rewind', { dir: this.#dir(dir), revision, ...(expectedHead != null && { expected_head: expectedHead }) });
  }
  stop(dir) { return this.call('stop', { dir: this.#dir(dir) }); }
  steer(dir, text, { now } = {}) { return this.call('steer', { dir: this.#dir(dir), ...(text != null && { text }), ...(now && { now }) }); }
  status(dir) { return this.call('status', { dir: this.#dir(dir) }); }
  /** The transcript (at `revision`, or now); `from`: only the messages from that index on
   * (`total` counts them all). */
  history(dir, revision, { from } = {}) { return this.call('history', { dir: this.#dir(dir), revision, ...(from && { from }) }); }
  /** Rewind targets, newest first, among the newest `limit` steps (48 unless said; at most 8192). */
  async targets(dir, { limit } = {}) { return (await this.call('targets', { dir: this.#dir(dir), ...(limit && { limit }) })).targets; }
  files(dir, revision) { return this.call('files', { dir: this.#dir(dir), revision }); }
  /** The file's bytes (a Uint8Array). */
  async read(dir, path, revision) {
    const reply = await this.call('read', { dir: this.#dir(dir), path, revision });
    return reply.encoding === 'utf8' ? new TextEncoder().encode(reply.content) : base64ToBytes(reply.content);
  }
  settings(dir, changes = {}) { return this.call('settings', { dir: this.#dir(dir), ...changes }); }
  /** The session's spend: totals, `turns` and `models`, as a native host's `usage`. */
  usage(dir) { return this.call('usage', { dir: this.#dir(dir) }); }
  /** What this package can run: built-in tools, runners, budgets, task modes, ... */
  capabilities() { return this.call('capabilities'); }
  /** What the session's settings must change to run here: `{changes, notes, refused}`
   * (`changes` is one `settings` call; `runner`: where commands go when the session
   * names none of this app's runners, 'app' unless said). An import carries it too. */
  fit(dir, { runner } = {}) { return this.call('fit', { dir: this.#dir(dir), ...(runner && { runner }) }); }
  state(dir, revision) { return this.call('state', { dir: this.#dir(dir), revision }); }
  setState(dir, patch, { expectedHead } = {}) {
    return this.call('state_set', { dir: this.#dir(dir), patch, ...(expectedHead != null && { expected_head: expectedHead }) });
  }
  recover(dir) { return this.call('recover', { dir: this.#dir(dir) }); }
  /** The tasks the session started: `{tasks: [{task, state, queued, answer, pending, delivered}]}`. */
  tasks(dir) { return this.call('tasks', { dir: this.#dir(dir) }); }
  /** A task as it stands now (`task` its name or label; nothing waits here: an app
   * hears of its work and events as they come). */
  taskWait(dir, task) { return this.call('task_wait', { dir: this.#dir(dir), task }); }
  /** Stops a background task where it is (`{task, stopping}`; false: it was not running). */
  taskStop(dir, task) { return this.call('task_stop', { dir: this.#dir(dir), task }); }
  taskStart(dir, task, prompt, { model, effort, files, settings, report = 'wait', expectedHead } = {}) {
    return this.call('task_start', { dir: this.#dir(dir), task, prompt, report, ...(model && { model }), ...(effort && { effort }), ...(files && { files }), ...(settings && { settings }), ...(expectedHead != null && { expected_head: expectedHead }) });
  }

  /** The session as a session bundle (a Uint8Array) to save or send: its store and its
   * tasks', each as one image. Unless `keep`, the session moves: it closes here and this
   * copy is sealed, so it continues only where the bundle is imported (here, in
   * another app, or a native host's `import_session`). `settings` change the bundle's
   * copy only, for the host it goes to (a native host's own, such as `builtin_tools:
   * 'all'`, included); the session keeps its own. A copy's stores are checked as far as
   * the bundle records them (an import checks them whole), unless `whole`: a copy the
   * app may need to go back to, a backup, is checked record by record, as a move is. */
  async exportSession(dir, { keep = false, settings, whole = false } = {}) {
    dir = this.#dir(dir);
    if (this.#closed) throw new SpindleError('closed', this.#closed);
    if (this.#opfs) await this.#own(dir);
    try {
      return this.#embedded.exportSession(dir, Boolean(keep), settings ? JSON.stringify(settings) : undefined, Boolean(whole));
    } catch (error) {
      throw new SpindleError(error.code || 'failed', error.message);
    }
  }

  /** Checks a session bundle whole, as `importSession` would (framing, checksum,
   * manifest, every store), writing nothing: `{head, identity, files}`. */
  checkBundle(bytes) {
    try {
      return JSON.parse(this.#embedded.checkBundle(bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes)));
    } catch (error) {
      throw new SpindleError(error.code || 'invalid', error.message ?? String(error));
    }
  }
  /** Continues a bundled session (from `exportSession` or a native `export_session`) as
   * `dir` (nothing there yet); resolves with its status, plus `imported`. */
  async importSession(dir, bytes) {
    dir = this.#dir(dir);
    if (this.#closed) throw new SpindleError('closed', this.#closed);
    if (this.#opfs) await this.#own(dir);
    try {
      return JSON.parse(this.#embedded.importSession(dir, bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes)));
    } catch (error) {
      throw new SpindleError(error.code || 'failed', error.message);
    }
  }

  /** Stops every running turn's outside work and refuses new calls; resolves when done. */
  async close() {
    this.#closed = 'the client was closed';
    // No one will take their answers: the app's handlers are told.
    for (const controller of this.#appCalls.values()) controller.abort(settled('closed'));
    this.#appCalls.clear();
    for (const [key, work] of this.#works) {
      work.cancelled = true;
      work.controller.abort();
      clearTimeout(work.timer);
      this.#works.delete(key);
    }
    for (const { reject } of this.#pending.values()) reject(new SpindleError('closed', this.#closed));
    this.#pending.clear();
    // Closing the sessions closes their stores, which gives their Web Locks back.
    this.#embedded.free?.();
    if (this.#opfs) {
      await Promise.allSettled([...this.#opfs.owning.values(), ...this.#opfs.releasing.values()]);
      this.#opfs.fs?.close();
    }
    return 0;
  }
}
