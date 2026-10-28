// Types of embedded.mjs (the embedded package's client) and of the events every
// Spindle client emits (docs/API.md, events).

export declare const API_VERSION: 1;

export declare class SpindleError extends Error {
  /** The stable error code: busy, head_moved, not_found, stopped, locked, ... */
  kind: string;
  status?: number;
  data: Record<string, unknown>;
}

/** An event's stable form; `type` names it (docs/API.md lists every field). */
export type SpindleEvent =
  | { type: 'prompt'; text: string }
  | { type: 'compacted'; before: number; after: number }
  | { type: 'model_step'; provider: string; model: string; step: number }
  | { type: 'model_delta'; text: string }
  | { type: 'model_text_withdrawn' }
  | { type: 'model_text'; text: string }
  | { type: 'usage'; input: number; output: number }
  | { type: 'cost'; usd: number; turn: number; total: number; billing: string }
  | { type: 'response_reused' }
  | { type: 'tool_call'; id: string; name: string; input: unknown }
  | { type: 'tool_skipped' }
  | { type: 'tool_refused'; reason: string }
  | { type: 'tool_reused' }
  | { type: 'tool_interrupted' }
  | { type: 'unisolated'; reason: string }
  | { type: 'display'; display: unknown }
  | { type: 'tool_result'; content: string; failed: boolean }
  | { type: 'task_reported'; task: string; text: string }
  | { type: 'task'; task: string; event: SpindleEvent }
  | { type: 'app_tool_details'; call: string; details: unknown }
  | { type: 'nested'; label: string }
  | { type: 'output'; stream: 'stdout' | 'stderr'; data: string }
  | { type: 'output_end' }
  | { type: 'stopped'; reason: string; steps: number; revision: number; files: number; complete: boolean }
  | { type: 'rewound'; target: number; revision: number; kind: 'settled' | 'before_model'; abandoned: number }
  | { type: 'state'; revision: number; changed: string[] }
  | { type: 'state_refused'; reason: string }
  | { type: 'app_calls_rewound'; calls: string[] }
  | { type: 'retrying'; target: number; head: number }
  | { type: 'retrying_first'; path: string }
  | { type: 'nothing_to_continue' }
  | { type: 'extended'; steps: number }
  | { type: 'steered'; text: string }
  | { type: 'reply_cut' }
  | { type: 'reply_not_cut' }
  | { type: 'settings_applied'; text: string }
  | { type: 'provider_retry'; status: number; seconds: number; attempt: number; compacting: boolean }
  | {
      type: 'app_tool'; call: string; tool_use_id: string; name: string; input: unknown;
      again: boolean; rerun: 'safe' | 'no'; timeout_s: number;
    }
  | { type: 'app_tool_done'; call: string; outcome: 'answered' | 'failed' | 'unknown' | 'no_answer' | 'stopped' };

/** A file given with a prompt or a new session. */
export type FileParam = { path: string; executable?: boolean } & ({ text: string } | { base64: string });

/** What an app tool's `run` returns: text, an answer object, or any JSON value. */
export type ToolAnswer =
  | string
  | {
      content?: string; error?: string; failed?: boolean; unknown?: boolean;
      files?: { write?: Record<string, string>; write_base64?: Record<string, string>; executable?: string[]; delete?: string[] };
      state?: Record<string, unknown>; details?: unknown;
    }
  | unknown;

export interface ToolDefinition {
  name: string;
  description: string;
  inputSchema?: Record<string, unknown>;
  rerun?: 'safe' | 'no' | boolean;
  timeoutS?: number;
  maxOutput?: number;
  run(input: any, call: SpindleEvent & { type: 'app_tool' }): ToolAnswer | Promise<ToolAnswer>;
  again?(input: any, call: SpindleEvent & { type: 'app_tool' }): ToolAnswer | Promise<ToolAnswer>;
}

/** The facts of a model request the app authorizes (no secret is ever in them). */
export interface RequestFacts {
  provider: string;
  model: string;
  endpoint: string;
  contract: string;
  /** The request's headers that do not depend on the key (`anthropic-version`, ...). */
  headers: Record<string, string>;
  /** The provider's authentication headers as templates, `{key}` where the key goes
   * (derived from the catalog; returning the key fills them in). */
  auth: Record<string, string>;
}

/** A file of the workspace a command runs over; `read(object)` gives its bytes. */
export interface RunFile { path: string; size: number; executable: boolean; object: string }

export interface RunRequest {
  /** The session the command is for (what a script's `code` requests name). */
  session: string;
  command: string;
  timeoutS: number;
  network: boolean;
  files: RunFile[];
  links: { path: string; target: string }[];
  directories: string[];
  read(object: string): Uint8Array;
  signal: AbortSignal;
}

export interface RunResult {
  output: string;
  success: boolean;
  writes?: ({ path: string; executable?: boolean } & ({ content: string | Uint8Array } | { base64: string }))[];
  deletes?: string[];
  links?: { path: string; target: string }[];
  directories?: string[];
  /** Why the run could not start or finish (its changes are then not kept). */
  failure?: string | null;
}

export interface Runner {
  kind?: 'remote';
  /** The sandbox the runner runs commands in (64 hex digits): required of a remote
   * runner; an app runner that has none runs commands unisolated. */
  identity?(): Promise<string>;
  run(request: RunRequest): Promise<RunResult>;
}

export declare function remoteRunner(options: { url: string; token: string; fetch?: typeof fetch }): Runner;

export interface EmbeddedOptions {
  /** The package's wasm-bindgen module (Node glue, or the web module after `init()`). */
  wasm: unknown;
  storage?: 'memory' | 'node' | 'opfs';
  storageOptions?: { journal?: 'wal' | 'delete'; note?: string };
  /** node:fs, for `storage: 'node'`. */
  fs?: unknown;
  platform?: string;
  /** Opens the OPFS pool, for `storage: 'opfs'` (see opfs.mjs `openOpfsPool`). */
  pool?: () => Promise<unknown>;
  /** Providers in Dext's catalog format (default: Dext's built-ins). */
  catalog?: Record<string, unknown>;
  /** The key for a fresh model request (a string or `{ key }`, filling `facts.auth`), or
   * headers to add as given. A key fills `{key}` in the `auth` templates, and any claim
   * slot they carry (a ChatGPT sign-in's `{account}`) is read from the key itself, a JWT;
   * headers returned as given must carry such values themselves. */
  credentials?(facts: RequestFacts): Promise<string | { key: string } | Record<string, string>> | string | { key: string } | Record<string, string>;
  fetch?: typeof fetch;
  /** Runs `bash`: the app's runner and a remote one, or one runner (a remote runner
   * serves `runner: 'remote'` sessions, any other `runner: 'app'` ones). */
  runner?: Runner | { app?: Runner | null; remote?: Runner | null };
  deadlines?: { firstByte?: number; idle?: number; total?: number };
  resolveDir?(dir: string): string;
}

/** Settings `create` and `settings` take (docs/API.md); the package honors file
 * tools (`builtin_tools: 'files' | 'none'`), `runner` and either compaction (`model`,
 * the default, or `digest`). */
export interface SessionSettings {
  model?: string;
  network?: boolean;
  effort?: 'default' | 'off' | 'low' | 'medium' | 'high' | 'xhigh' | 'max';
  compaction?: 'model' | 'digest';
  tools?: unknown[];
  builtin_tools?: 'files' | 'none';
  instructions?: string;
  resend_unknown?: boolean;
  tasks?: boolean | { max_depth?: number; max_running?: number; models?: string[] };
  runner?: 'none' | 'remote' | 'app';
  /** `auto` (the default here), `required` (refuses runners that do not isolate) or `off`. */
  isolation?: 'auto' | 'required' | 'off';
  /** What a session from a native host may hold, cleared here (no spend ledger, no host
   * paths, no toolchain in an app). */
  max_usd?: null;
  sources?: [];
  toolchain?: false;
  files?: FileParam[];
}

export interface TurnResult {
  session: string;
  head: number;
  stopped: (SpindleEvent & { type: 'stopped' }) | null;
  rewound: (SpindleEvent & { type: 'rewound' }) | null;
  unsent: string[];
}

export declare class EmbeddedClient {
  constructor(options: EmbeddedOptions);
  /** Resolves once a remote runner's identity is known. */
  ready: Promise<void>;
  on(name: 'event', listener: (event: SpindleEvent, session: string, requestId: number | null) => void): this;
  on(name: 'app_tool_error' | 'work_error' | 'runner_error', listener: (error: unknown, ...rest: unknown[]) => void): this;
  off(name: string, listener: (...args: any[]) => void): this;
  once(name: string, listener: (...args: any[]) => void): this;
  tool(definition: ToolDefinition): this;
  /** A request of a script the runner is running for `dir` (the code-mode bridge, as
   * native run_code scripts ask: model, tool, batch, search, describe, limits): its
   * reply. Model calls go out with `credentials` and `fetch`; tools to their handlers. */
  code(dir: string, request: { method: string; input?: unknown; id?: number }, options?: { signal?: AbortSignal }): Promise<{ ok: boolean; data?: unknown; error?: string; may_have_run?: boolean }>;
  /** Connects (null: disconnects) the runner for `kind` sessions, binding its sandbox. */
  connectRunner(kind: 'app' | 'remote', runner: Runner | null): Promise<void>;
  /** The sites scripts' `http()` may reach (a code policy's `http` grants, no headers). */
  setHttpGrants(grants: { name: string; url: string; risk: 'read' | 'write' | 'danger'; methods?: string[]; description?: string; timeout_seconds?: number; max_response_bytes?: number }[]): void;
  /** The provider catalog the sessions use, in Dext's format (no credentials). */
  catalog(): { active_provider?: string; providers: { id: string; base_url: string; request_contract?: string; default_model?: string; models?: string[] }[] } & Record<string, unknown>;
  /** The efforts `provider`'s `model` takes, `default` first; `label` says what a request on that route asks for. */
  efforts(provider: string, model: string): { effort: NonNullable<SessionSettings['effort']>; label: string; short: string }[];
  call(method: string, params?: Record<string, unknown>): Promise<any>;
  info(): Promise<{ api_version: number; transport: 'embedded'; features: string[]; version: string; schema: number }>;
  providers(): Promise<{ providers: { id: string; name: string; model: string; contract: string }[] }>;
  create(dir: string, provider: string, settings?: SessionSettings): Promise<any>;
  open(dir: string): Promise<any>;
  closeSession(dir: string): Promise<{ session: string; closed: boolean }>;
  prompt(dir: string, text: string, options?: { files?: FileParam[]; state?: Record<string, unknown>; expectedHead?: number }): Promise<TurnResult>;
  resume(dir: string, options?: { expectedHead?: number; until?: 'unknown' }): Promise<TurnResult>;
  retry(dir: string, options?: { expectedHead?: number }): Promise<TurnResult>;
  rewind(dir: string, revision: number, options?: { expectedHead?: number }): Promise<TurnResult>;
  stop(dir: string): Promise<{ session: string; stopping: true }>;
  steer(dir: string, text?: string, options?: { now?: boolean }): Promise<{ session: string; queued: number }>;
  status(dir: string): Promise<any>;
  /** `from`: only the messages from that index on (`total` counts them all). */
  history(dir: string, revision?: number, options?: { from?: number }): Promise<{ head: number; revision: number; messages: unknown[]; total: number; app_details: unknown[] }>;
  /** Rewind targets among the newest `limit` steps (48 unless said; at most 8192), newest first. */
  targets(dir: string, options?: { limit?: number }): Promise<{ revision: number; kind: 'settled' | 'before_model' }[]>;
  files(dir: string, revision?: number): Promise<{ head: number; revision: number; files: { path: string; size: number; digest: string; executable: boolean }[]; links: unknown[]; directories: string[] }>;
  read(dir: string, path: string, revision?: number): Promise<Uint8Array>;
  settings(dir: string, changes?: SessionSettings): Promise<any>;
  state(dir: string, revision?: number): Promise<{ head: number; revision: number; state: Record<string, unknown> }>;
  setState(dir: string, patch: Record<string, unknown>, options?: { expectedHead?: number }): Promise<{ session: string; revision?: number; queued?: number }>;
  recover(dir: string): Promise<{ head: number; phase: string; items: unknown[]; wake: { work: 'none' | 'ready' | 'blocked'; why: string | null } }>;
  taskStart(dir: string, task: string, prompt: string, options?: { model?: string; effort?: string; files?: 'keep' | 'discard'; settings?: Record<string, unknown>; report?: 'wait'; expectedHead?: number }): Promise<TurnResult & { task: string; answer: string; failed: boolean; state: 'finished' }>;
  /** The session as a session bundle; it moves (this copy sealed) unless `keep`.
   * `settings` change the bundle's copy only, for the host it goes to. */
  exportSession(dir: string, options?: { keep?: boolean; settings?: Record<string, unknown> }): Promise<Uint8Array>;
  /** Continues a bundled session as `dir` (nothing there yet). */
  importSession(dir: string, bytes: Uint8Array | ArrayBuffer): Promise<any>;
  close(): Promise<number>;
}
