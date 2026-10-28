// The real-browser test's page (embed/tests/browser_real.mjs drives it, one phase per
// browser launch): a real provider through the relay, every step reported to the relay
// with its request count, the driver deciding pass or fail.
const params = new URLSearchParams(location.search);
const phase = params.get('phase');
const dir = params.get('dir');
const provider = params.get('provider');
const model = params.get('model') || undefined;
const out = document.getElementById('out');

const report = (step, data = {}) => {
  out.textContent += `${step}: ${JSON.stringify(data).slice(0, 300)}\n`;
  return fetch('/report', { method: 'POST', body: JSON.stringify({ phase, step, ...data }) });
};
const stats = async () => (await fetch('/stats')).json();
const base64 = (bytes) => {
  let text = '';
  for (let at = 0; at < bytes.length; at += 0x8000) text += String.fromCharCode(...bytes.subarray(at, at + 0x8000));
  return btoa(text);
};

/** A worker running the client; `call` invokes a client method, `events` keeps them all. */
function start() {
  const worker = new Worker('./worker.mjs', { type: 'module' });
  const waiting = new Map();
  const events = [];
  const watchers = [];
  let next = 0;
  const ready = new Promise((resolve, reject) => {
    worker.onerror = (event) => reject(new Error(`worker failed: ${event.message}`));
    worker.onmessage = ({ data }) => {
      if (data.ready) return resolve();
      if (data.event) {
        events.push(data.event);
        for (const watch of [...watchers]) watch(data.event);
        return;
      }
      waiting.get(data.id)?.(data);
      waiting.delete(data.id);
    };
  });
  const call = (method, ...args) => new Promise((resolve) => {
    const id = ++next;
    waiting.set(id, resolve);
    worker.postMessage({ id, method, args });
  });
  const when = (type) => new Promise((resolve) => {
    const watch = (event) => {
      if (event.type !== type) return;
      watchers.splice(watchers.indexOf(watch), 1);
      resolve(event);
    };
    watchers.push(watch);
  });
  return { ready, call, events, when };
}
/** Waits for an event of `type`, or for `call` to end first (then the event never came). */
const before_ = (w, type, call) => Promise.race([w.when(type).then(() => true), call.then(() => false)]);
const reply = (events, from) => events.slice(from).filter((e) => e.type === 'model_delta').map((e) => e.text).join('');

async function first(w) {
  const created = await w.call('create', dir, provider, model ? { model } : {});
  await report('created', { ok: created.ok, error: created.message });
  // A real task with several file tools.
  let from = w.events.length;
  const tools = await w.call('prompt', dir, 'Use your file tools. Create notes.md containing the line "# Notes". Then add a second line "one" to it. Then read notes.md and reply with only its number of lines.');
  const notes = await w.call('read', dir, 'notes.md');
  await report('tools', { ok: tools.ok, complete: tools.result?.stopped?.complete, error: tools.message,
    calls: w.events.slice(from).filter((e) => e.type === 'tool_call').map((e) => e.name),
    notes: notes.result, reply: reply(w.events, from) });
  // Stop while the reply streams. The cut request's outcome is unknown (the provider
  // may have finished and billed it), so resume sends nothing, and retry sends it once.
  let before = await stats();
  const story = w.call('prompt', dir, 'Write a story of about 800 words about a lighthouse keeper. Use no tools.');
  const streamed = await before_(w, 'model_delta', story);
  const stopped = streamed ? await w.call('stop', dir) : { ok: false, message: 'it never streamed' };
  const cut = await story;
  const afterStop = await stats();
  const resumed = await w.call('resume', dir);
  const afterResume = await stats();
  const retried = await w.call('retry', dir);
  await report('stop', { streamed, stopOk: stopped.ok, cutKind: cut.ok ? 'completed' : cut.kind,
    cutRequests: afterStop.requests - before.requests, resumeOk: resumed.ok, resumeError: resumed.message,
    resumeRequests: afterResume.requests - afterStop.requests, retryComplete: retried.result?.stopped?.complete,
    retryError: retried.message, retryRequests: (await stats()).requests - afterResume.requests });
  // A message steered in while it works.
  from = w.events.length;
  const steps = w.call('prompt', dir, 'Use your file tools, one call at a time: create a.txt containing 1, then create b.txt containing 2, then reply with only DONE.');
  const working = await before_(w, 'tool_call', steps);
  const steered = working ? await w.call('steer', dir, 'Also create c.txt containing 3 before you reply.') : { ok: false };
  const done = await steps;
  const files = await w.call('files', dir);
  await report('steer', { steerOk: steered.ok, queued: steered.result?.queued, ok: done.ok,
    complete: done.result?.stopped?.complete, unsent: done.result?.unsent,
    steeredEvent: w.events.slice(from).some((e) => e.type === 'steered'),
    files: files.result?.files.map((f) => f.path) });
  // A request in flight when the browser dies: the driver kills it on this report.
  before = await stats();
  const essay = w.call('prompt', dir, 'Write an essay of about 1500 words about ocean tides. Use no tools.');
  const flowing = await before_(w, 'model_delta', essay);
  const early = flowing ? null : await essay;
  await report('crash-ready', { streaming: flowing, error: early?.message,
    requests: (await stats()).requests - before.requests });
}

async function second(w) {
  const status = await w.call('status', dir);
  await report('reopened', { ok: status.ok, head: status.result?.head, state: status.result?.state,
    complete: status.result?.complete });
  const other = start();
  await other.ready;
  const refused = await other.call('status', dir);
  await report('second worker', { ok: refused.ok, kind: refused.kind });
  let before = await stats();
  const resumed = await w.call('resume', dir);
  let after = await stats();
  await report('resume unknown', { ok: resumed.ok, kind: resumed.kind, error: resumed.message,
    requests: after.requests - before.requests });
  before = after;
  const retried = await w.call('retry', dir);
  after = await stats();
  await report('retry', { ok: retried.ok, complete: retried.result?.stopped?.complete, error: retried.message,
    requests: after.requests - before.requests });
  const moved = await w.call('exportSession', dir);
  const sealed = await w.call('status', dir);
  await report('export', { ok: moved.ok, error: moved.message, bundle: moved.ok ? base64(moved.result) : null,
    sealedOk: sealed.ok, sealedError: sealed.message });
}

async function third(w) {
  const back = new Uint8Array(await (await fetch('/back.spindle')).arrayBuffer());
  const moved = `${dir}-back`;
  const imported = await w.call('importSession', moved, back);
  const notes = await w.call('read', moved, 'notes.md');
  const from = w.events.length;
  const before = await stats();
  const turn = await w.call('prompt', moved, 'Read notes.md and reply with only its number of lines.');
  const after = await stats();
  const copy = await w.call('exportSession', moved, { keep: true });
  await report('back', { importOk: imported.ok, importError: imported.message, head: imported.result?.head,
    notes: notes.result, ok: turn.ok, complete: turn.result?.stopped?.complete, error: turn.message,
    reply: reply(w.events, from), requests: after.requests - before.requests,
    bundle: copy.ok ? base64(copy.result) : null });
}

try {
  const w = start();
  await w.ready;
  await ({ 1: first, 2: second, 3: third })[phase](w);
  await report('done');
} catch (error) {
  await report('error', { message: String(error.message ?? error) });
}
