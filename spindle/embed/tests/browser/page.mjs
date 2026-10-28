// Drives two workers through one OPFS session: A creates it and runs a turn; B is
// refused while A holds the session's Web Lock; once A closes, B opens it and reads
// what A wrote. On a second launch (`?phase=reopen`, the same browser profile) a new
// worker reopens the session and runs another turn, then moves it by bundle to another
// OPFS place and goes on there. Results go to `window.result`.
const phase = new URLSearchParams(location.search).get('phase') || 'first';
function worker() {
  const w = new Worker('./worker.mjs', { type: 'module' });
  let next = 0;
  const waiting = new Map();
  const ready = new Promise((resolve, reject) => {
    w.onmessage = ({ data }) => {
      if (data.ready) return resolve();
      waiting.get(data.id)?.(data);
      waiting.delete(data.id);
    };
    w.onerror = (event) => {
      event.preventDefault?.();
      reject(new Error(`worker failed: ${event.message || 'error'} (${event.filename}:${event.lineno})`));
    };
  });
  return {
    ready,
    ask(action, args = {}) {
      const id = ++next;
      return new Promise((resolve) => {
        waiting.set(id, resolve);
        w.postMessage({ id, action, ...args });
      });
    },
  };
}

const steps = [];
const dir = '/sessions/a';
setTimeout(() => { window.result ??= { phase, steps: [...steps, ['page.timeout', { ok: false }]] }; }, 60_000);
try {
  if (phase === 'first') {
    const a = worker();
    const b = worker();
    await Promise.all([a.ready, b.ready]);
    steps.push(['a.create', await a.ask('create', { dir })]);
    steps.push(['b.status.while.a.holds', await b.ask('status', { dir })]);
    steps.push(['a.close', await a.ask('close')]);
    steps.push(['b.status.after.a', await b.ask('status', { dir })]);
    steps.push(['b.read', await b.ask('read', { dir, path: 'a.txt' })]);
    steps.push(['b.reopen', await b.ask('reopen', { dir })]);
    steps.push(['b.close', await b.ask('close')]);
    steps.push(['b.pool', await b.ask('pool', { phase })]);
  } else {
    const c = worker();
    await c.ready;
    steps.push(['c.status', await c.ask('status', { dir })]);
    steps.push(['c.read', await c.ask('read', { dir, path: 'a.txt' })]);
    steps.push(['c.prompt', await c.ask('prompt', { dir })]);
    // The session moves by bundle: its old place is sealed, and it goes on elsewhere.
    const exported = await c.ask('export', { dir });
    steps.push(['c.export', exported]);
    steps.push(['c.status.moved', await c.ask('status', { dir })]);
    steps.push(['c.import', await c.ask('import', { dir: '/sessions/b', base64: exported.base64 })]);
    steps.push(['c.read.b', await c.ask('read', { dir: '/sessions/b', path: 'a.txt' })]);
    steps.push(['c.prompt.b', await c.ask('prompt', { dir: '/sessions/b' })]);
    steps.push(['c.close', await c.ask('close')]);
    steps.push(['c.pool', await c.ask('pool', { phase })]);
  }
} catch (error) {
  steps.push(['page.error', { ok: false, message: String(error.message ?? error) }]);
}
window.result = { phase, steps };
