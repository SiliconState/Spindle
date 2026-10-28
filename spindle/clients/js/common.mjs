// Pieces both Spindle clients share (spindle.mjs, embedded.mjs), with no Node
// imports so the embedded client runs in browsers too.

export class SpindleError extends Error {
  constructor(kind, message, status, data = {}) {
    super(`${kind}: ${message}`);
    this.kind = kind;
    this.status = status;
    this.data = data;
  }
}

/** Tools an app supplies (docs/API.md, app tools), shared by both transports.
 * `tool({ name, description, inputSchema, rerun, timeoutS, maxOutput, run, again })` registers
 * one: `run(input, call)` returns text, `{ content | error, failed, files, state }`, or
 * any other value (sent as its JSON); throwing answers with the error. A call cut off
 * by a crash or stop is asked again (every continue, until answered) with `call.again`
 * true: a tool declared `rerun: 'safe'` simply runs again; any other answers "unknown"
 * unless it gives `again(input, call)`, which can look up its own record of `call.call`
 * (stable across restarts) and answer from it. Only registered tools are answered
 * automatically: an app answers others itself (`app_tool_result`). */
export class Tools {
  constructor() { this.byName = new Map(); }
  add({ name, description, inputSchema = { type: 'object', properties: {} }, rerun = 'no', timeoutS, maxOutput, run, again }) {
    if (typeof run !== 'function') throw new TypeError(`tool ${name}: run must be a function`);
    // The server takes true/false as well; one spelling here keeps `answer` simple.
    if (typeof rerun === 'boolean') rerun = rerun ? 'safe' : 'no';
    this.byName.set(name, { name, description, inputSchema, rerun, timeoutS, maxOutput, run, again });
  }
  /** The definitions to offer, as `create`/`settings` take them (`tools`). */
  definitions() {
    return [...this.byName.values()].map(({ name, description, inputSchema, rerun, timeoutS, maxOutput }) => ({
      name, description, input_schema: inputSchema, rerun, ...(timeoutS != null && { timeout_s: timeoutS }), ...(maxOutput != null && { max_output: maxOutput }),
    }));
  }
  /** The answer to an `app_tool` event (never throws). */
  async answer(event) {
    const tool = this.byName.get(event.name);
    try {
      if (!tool) return { error: `this app has no handler for the tool ${event.name}` };
      if (event.again && tool.rerun !== 'safe') {
        return tool.again ? answerOf(await tool.again(event.input, event)) : { unknown: true };
      }
      return answerOf(await tool.run(event.input, event));
    } catch (error) {
      return { error: String(error?.message ?? error) };
    }
  }
}

const ANSWER_KEYS = ['content', 'error', 'failed', 'unknown', 'files', 'state', 'details'];

/** A tool's return value as an answer (the same shaping as the Python client). */
export function answerOf(value) {
  if (typeof value === 'string') return { content: value };
  if (value === undefined || value === null) return { content: '' };
  if (typeof value === 'object' && !Array.isArray(value) && ANSWER_KEYS.some((key) => key in value)) {
    const { content, error, failed, unknown, files, state, details } = value;
    // An answer needs text: one that only writes files or state says nothing more.
    const text = content === undefined && error === undefined && !unknown ? '' : content;
    return { content: text, error, failed, unknown, files, state, details };
  }
  return { content: JSON.stringify(value) };
}

/** What to send when Spindle refused an answer (or it could not be sent): the call
 * fails with the reason, instead of waiting out its timeout. */
export function refusedAnswer(error) {
  const why = String(error?.message ?? error).replace(/^bad_request: /, '');
  return { error: `the app's answer was refused: ${why}` };
}

/** A minimal event emitter (Node's EventEmitter is not in browsers): `on`, `off`,
 * `once`, `emit`. A listener that throws has its error rethrown asynchronously. */
export class Emitter {
  #listeners = new Map();
  on(name, listener) {
    if (!this.#listeners.has(name)) this.#listeners.set(name, []);
    this.#listeners.get(name).push(listener);
    return this;
  }
  off(name, listener) {
    const list = this.#listeners.get(name) || [];
    const at = list.indexOf(listener);
    if (at >= 0) list.splice(at, 1);
    return this;
  }
  once(name, listener) {
    const wrapped = (...args) => { this.off(name, wrapped); listener(...args); };
    return this.on(name, wrapped);
  }
  emit(name, ...args) {
    const list = [...(this.#listeners.get(name) || [])];
    for (const listener of list) {
      try {
        listener(...args);
      } catch (error) {
        queueMicrotask(() => { throw error; });
      }
    }
    return list.length > 0;
  }
}
