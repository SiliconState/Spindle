// Moving a thread to the user's Spindle host and back, so that whenever the engine
// stops (a crash, Chrome closing, a reload), the thread is live in exactly one place,
// or in a recorded move that finishes or rolls back by itself once the engine starts
// again and the host answers. Never lost, never live twice.
//
// A move is a record in kv and, once there are any, its bytes in `bundles`, both under
// `host-move:ID`, written together with a strict commit (on disk before the next step):
//
// To the host (`way: 'to'`):
//   upload   the thread's copy (a bundle) is held here with its Idempotency-Key; the
//            thread here is still live. Sent until the host answers: 201/200 (it holds
//            the thread) goes on; a refusal no repeat can change rolls back (the thread
//            stays here); no answer waits and sends the same bytes under the same key.
//   seal     the host holds it as `session`: this copy is sealed (its store's fence).
//   drop     the thread's record says `home` (where it lives now): the sealed store and
//            the held bytes go; the fence stays.
// Back to this browser (`way: 'back'`):
//   export   the host is asked to move it back (it seals its copy and keeps the
//            bundle, sending the same bytes on every later export).
//   import   the bytes are held here: they are imported in place of the fence (anything
//            a cut import left goes first), with the browser's fit.
//   release  the thread is live here again: the host's sealed copy is deleted; the
//            thread takes work meanwhile (the host's copy is sealed, not live).

/** The record's key (kv and bundles). */
export const MOVE = (id) => `host-move:${id}`;

/** The largest thread bundle this browser takes (a file brought in, or a thread coming
 * back from the host): bringing one in holds it about three times over in the engine's
 * memory (4 GB at most), so a larger one would end the engine rather than fail. */
export const BUNDLE_MOST = 1 << 30;

/** Bytes as the panel says them (1.0 GB). */
const gigabytes = (bytes) => `${(bytes / 2 ** 30).toFixed(1)} GB`;

/** Steps after which the thread here takes no work until the move ends. */
const HOLDING = new Set(['upload', 'seal', 'drop', 'export', 'import']);

/** What the thread says while a move waits on a step (the panel shows it). */
export const STEP_TEXT = {
  upload: 'Moving to your host: sending it',
  seal: 'Moving to your host: your host has it; sealing the copy here',
  drop: 'On your host: tidying the copy here',
  export: 'Coming back from your host: asking your host for it',
  import: 'Coming back from your host: bringing it in',
  release: 'Back in this browser: removing the sealed copy your host kept',
};

/** Refusals of an upload that the same request can never turn into a success: nothing
 * was imported, so the move rolls back. The host looks its key up before it reads or
 * checks the bundle, so a bundle it cannot read or a model it cannot send to proves that
 * no attempt imported it, whichever attempt hears it. The others (too large, a bad key or
 * query, a conflict) come before that lookup: only the first attempt, which no earlier
 * send can have imported, may take them as final. Anything else (no answer, 5xx, `busy`:
 * the same key still importing, 401: connect again) waits and sends it again. */
function refusedForGood(error, firstAttempt) {
  if (error?.kind === 'bad_bundle' || error?.kind === 'no_credentials') return true;
  if (!firstAttempt) return false;
  if (error?.status === 413) return true;
  if (error?.status === 400) return true;
  if (error?.status === 409) return error.kind === 'conflict';
  return false;
}

/** A key for one move of a thread: its identity and head, and a random part (a later
 * move of the same head is a new import, whatever the host kept). */
export function moveKey(identity, head) {
  const random = [...crypto.getRandomValues(new Uint8Array(6))].map((byte) => byte.toString(16).padStart(2, '0')).join('');
  const who = String(identity ?? '').replace(/[^A-Za-z0-9_-]/g, '').slice(0, 64) || 'thread';
  return `${who}-${Number(head) || 0}-${random}`;
}

/** The host's refusal as a sentence (a SpindleError's message starts with its code). */
export const said = (error) => String(error?.message ?? error).replace(/^[a-z_]+: /, '');

/** A move that cannot go on yet: it waits for the host (nothing is lost meanwhile). */
export class Waiting extends Error {
  constructor(message) {
    super(message);
    this.kind = 'waiting';
  }
}

/**
 * The moves of this engine. `deps`:
 * - `read(store, key)`, `keysOf(store)`, `update(store, key, change, options)`
 *   (atomic read/modify/write with sibling changes and strict durability);
 * - `remote(url)`: the host's RemoteClient when `url` is the host connected with a
 *   browser token (else it throws, saying why);
 * - `local`: `copy(id)` (the thread's bundle, kept here, set up for the host),
 *   `facts(bytes)` ({head, identity}, the bundle checked whole), `sealed(id)`,
 *   `seal(id)`, `dropSealed(id)`, `clear(id)` (everything of the thread's place, fence
 *   too), `restore(id, bytes)` (a bundle imported in its place with the browser's fit:
 *   resolves with the thread's new fields and what changed, `said`);
 * - `changed(thread)`: a thread's record changed (panels are told);
 * - `cut(step)`: tests stop the engine there.
 */
export function hostMoves(deps) {
  const { read, keysOf, update, remote, local, changed = () => {}, cut = async () => {} } = deps;
  /** One continuation per thread at a time. */
  const going = new Map();

  async function thread(id) {
    const found = await read('threads', id);
    if (!found) throw Object.assign(new Error('there is no such thread'), { kind: 'not_found' });
    return found;
  }

  /** The thread's record and the move's together, on disk. `move` undefined ends the
   * move (its bytes go too); `bytes`: undefined keeps them as they are, null drops
   * them, a value holds it. */
  async function commit(id, threadChanges, move, bytes) {
    const changes = [['kv', MOVE(id), move]];
    if (bytes !== undefined || move === undefined) changes.push(['bundles', MOVE(id), bytes ?? undefined]);
    const next = await update('threads', id, (before) => ({ ...before, ...threadChanges }), { changes, durability: 'strict' });
    if (!next) throw Object.assign(new Error('there is no such thread'), { kind: 'not_found' });
    changed(next);
    return next;
  }

  /** What the thread shows while its move is at `step`; `why`, what it waits for. */
  const pending = (way, step, why = null) => ({ moving: { way, step, text: STEP_TEXT[step], why } });

  /**
   * Starts moving thread `id` to the host at `url` (its commands, model and page tools
   * there; `name`, `provider` and `model` as the host takes them). The thread must be
   * at rest (the caller holds it). Resolves once the move is recorded; `done` then
   * resolves (or rejects) once it has finished, rolled back or must wait for the host.
   */
  async function toHost(id, { url, name, provider, model }) {
    const bytes = await local.copy(id);
    const { head, identity } = local.facts(bytes);
    const move = { id, way: 'to', step: 'upload', url, key: moveKey(identity, head), name, provider: provider ?? null, model: model ?? null, head, identity, size: bytes.byteLength, at: Date.now() };
    await commit(id, pending('to', 'upload'), move, { bytes, size: bytes.byteLength });
    await cut('to:recorded');
    return { done: proceed(id) };
  }

  /** Starts bringing hosted thread `id` back to this browser. */
  async function back(id) {
    const found = await thread(id);
    if (!found.home) throw Object.assign(new Error('this thread is not on your host'), { kind: 'invalid' });
    const move = { id, way: 'back', step: 'export', url: found.home.url, session: found.home.session, at: Date.now() };
    await commit(id, pending('back', 'export'), move);
    await cut('back:recorded');
    return { done: proceed(id) };
  }

  /** Takes thread `id`'s move as far as it goes now (one at a time per thread). */
  function proceed(id) {
    const before = going.get(id) ?? Promise.resolve();
    const next = before.catch(() => {}).then(() => step(id));
    going.set(id, next);
    next.finally(() => { if (going.get(id) === next) going.delete(id); }).catch(() => {});
    return next;
  }

  async function step(id) {
    for (;;) {
      const move = await read('kv', MOVE(id));
      if (!move) return { done: true };
      let result;
      try {
        result = move.way === 'to' ? await stepTo(id, move) : await stepBack(id, move);
      } catch (error) {
        // Said on the thread; the step is taken again from its record next time.
        if (!(error instanceof Waiting) && error?.kind !== 'cut') {
          const current = await read('kv', MOVE(id));
          if (current) await commit(id, pending(current.way, current.step, said(error)), { ...current, error: said(error) }).catch(() => {});
        }
        throw error;
      }
      if (result) return result;
    }
  }

  /** One step of a move to the host; null to go on with the next. */
  async function stepTo(id, move) {
    if (move.step === 'upload') {
      const held = await read('bundles', MOVE(id));
      if (!held?.bytes) return wait(id, move, new Error('its recorded bundle is missing; the move stays held until it can be recovered'));
      // No host to send to yet (not connected): nothing is attempted, so a refusal of the
      // first real send can still roll the move back.
      let host;
      try {
        host = remote(move.url);
      } catch (error) {
        return wait(id, move, error);
      }
      const firstAttempt = !move.attempted;
      move = { ...move, attempted: true };
      await commit(id, pending('to', 'upload'), move);
      let reply;
      try {
        reply = await host.importThread(held.bytes, { key: move.key, name: move.name, provider: move.provider ?? undefined, model: move.model ?? undefined });
      } catch (error) {
        // A previous send may have committed even if this send is refused before lookup.
        if (refusedForGood(error, firstAttempt)) return rollTo(id, move, `your host refused it: ${said(error)}`);
        return wait(id, move, error);
      }
      await cut('to:uploaded');
      const session = reply.session;
      await commit(id, pending('to', 'seal'), { ...move, step: 'seal', session: session.id, fit: reply.fit ?? null, settings: session.settings ?? null, error: null });
      await cut('to:session');
      return null;
    }
    if (move.step === 'seal') {
      if (!(await local.sealed(id))) await local.seal(id);
      await cut('to:sealed');
      const settings = move.settings ?? {};
      await commit(id, {
        ...pending('to', 'drop'),
        home: { url: move.url, session: move.session },
        state: 'idle', note: null, runner: settings.runner ?? 'none', network: Boolean(settings.network), tasks: Boolean(settings.tasks),
        ...(settings.provider && { provider: settings.provider }), ...(settings.model && { model: settings.model }),
        ...(settings.effort && { effort: settings.effort }),
        updated: Date.now(),
      }, { ...move, step: 'drop' });
      await cut('to:home');
      return null;
    }
    // drop: the thread lives on the host; what is left of it here goes, but its fence.
    await local.dropSealed(id);
    await commit(id, { moving: null }, undefined);
    return { done: true, way: 'to', session: move.session };
  }

  /** A move to the host that cannot happen: the thread stays (or is again) live here. */
  async function rollTo(id, move, why) {
    if (await local.sealed(id)) {
      // Sealed after the host took it, and the host has since lost it: the copy held
      // here (taken before the seal, the same thread) comes back in its place.
      const held = await read('bundles', MOVE(id));
      if (!held?.bytes) return wait(id, move, new Error('its copy here is gone, and this copy is sealed'));
      await local.clear(id);
      await local.restore(id, held.bytes);
    }
    await commit(id, { moving: null, note: `It stayed in this browser: ${why}.` }, undefined);
    return { done: false, way: 'to', why };
  }

  /** One step of a move back here; null to go on. */
  async function stepBack(id, move) {
    if (move.step === 'export') {
      let host;
      try {
        host = remote(move.url);
      } catch (error) {
        return wait(id, move, error);
      }
      // Marked before it goes: after an export that may have reached the host, its own
      // export (still writing and sealing there) can be what keeps the thread busy.
      const firstAttempt = !move.attempted;
      if (firstAttempt) {
        // One too large for this browser to take stays on the host, as it was: the host
        // says its bundle's size before anything is sealed there (`bundle_bytes`; a host
        // that does not say, an older one, is asked for the bundle as before).
        const size = await host.session(move.session).then((session) => session?.bundle_bytes, () => null);
        if (size > BUNDLE_MOST) {
          const why = `it would come back as about ${gigabytes(size)}, more than this browser takes (${gigabytes(BUNDLE_MOST)})`;
          await commit(id, { moving: null, note: `It stays on your host: ${why}.` }, undefined);
          return { done: false, way: 'back', why };
        }
        move = { ...move, attempted: true };
        await commit(id, pending('back', 'export'), move);
      }
      let exported;
      try {
        exported = await host.exportThread(move.session);
        local.facts(exported.bytes);
      } catch (error) {
        // Busy at the first attempt: nothing moved (a turn or task runs there), so the
        // thread stays on the host. Busy later may be this move's own export: it waits,
        // and the host sends the sealed bundle once it is done.
        if (firstAttempt && error?.kind === 'busy' && error.status === 409) {
          await commit(id, { moving: null, note: `It stays on your host: ${said(error)}.` }, undefined);
          return { done: false, way: 'back', why: said(error) };
        }
        return wait(id, move, error);
      }
      await commit(id, pending('back', 'import'), { ...move, step: 'import', head: exported.head, identity: exported.identity, size: exported.bytes.byteLength, error: null }, { bytes: exported.bytes, size: exported.bytes.byteLength });
      await cut('back:held');
      return null;
    }
    if (move.step === 'import') {
      const held = await read('bundles', MOVE(id));
      // Lost here before it was in: the host sends the same bytes again.
      if (!held?.bytes) {
        await commit(id, pending('back', 'export'), { ...move, step: 'export' });
        return null;
      }
      await local.clear(id);
      const { fields, said: changes = [] } = await local.restore(id, held.bytes);
      await cut('back:imported');
      const note = changes.length ? `Back from your host. Changed to run here: ${changes.join('; ')}.` : 'Back from your host.';
      // The thread is in (live here from this commit): its held bytes go with it.
      await commit(id, { ...fields, ...pending('back', 'release'), home: null, state: 'idle', note, updated: Date.now() }, { ...move, step: 'release', said: changes }, null);
      return null;
    }
    // release: the host's sealed copy goes (already gone counts as gone).
    try {
      const host = remote(move.url);
      let receipt = await host.command('delete', { session: move.session });
      if (receipt.state === 'running') receipt = await host.settled(receipt.request_id);
      if (receipt.state !== 'completed') throw new Error(receipt.error ?? 'your host has not removed its sealed copy');
    } catch (error) {
      if (error?.status !== 404) return wait(id, move, error);
    }
    await cut('back:released');
    await commit(id, { moving: null }, undefined);
    return { done: true, way: 'back' };
  }

  /** The move waits for the host: said on the thread; the caller tries again later. */
  async function wait(id, move, error) {
    const why = error?.status === 401
      ? 'your host no longer takes this browser\'s token: connect it again in Settings'
      : said(error);
    await commit(id, pending(move.way, move.step, why), { ...move, error: why });
    throw new Waiting(why);
  }

  /** Every move recorded here: `{id, way, step, size, error}`. */
  async function list() {
    const moves = [];
    for (const key of await keysOf('kv')) {
      if (!String(key).startsWith('host-move:')) continue;
      const move = await read('kv', key);
      if (!move) continue;
      // Its recorded size, never the held bytes (up to 2 GiB) read just to measure them.
      moves.push({ id: move.id, way: move.way, step: move.step, url: move.url, size: move.size ?? 0, error: move.error ?? null });
    }
    return moves;
  }

  /** Whether thread `id` is in a move that holds it (no work until it ends). */
  async function holding(id) {
    const move = await read('kv', MOVE(id));
    return Boolean(move && HOLDING.has(move.step));
  }

  /** Every recorded move, taken as far as it goes now (after a start, or once the host
   * connects). Bundle cleanup belongs to the same transaction as ending a move; a
   * separate scan could mistake a newly recorded move's bytes for an orphan. */
  async function resume() {
    const runs = [];
    for (const key of await keysOf('kv')) {
      if (!String(key).startsWith('host-move:')) continue;
      const id = String(key).slice('host-move:'.length);
      // One under way already goes on by itself (another step queued behind it would
      // only send the same bytes again at once).
      if (going.has(id)) continue;
      runs.push(proceed(id).catch((error) => {
        if (!(error instanceof Waiting)) console.warn(`Spindle: the move of thread ${id} stopped:`, error);
      }));
    }
    await Promise.all(runs);
  }

  return { toHost, back, proceed, resume, list, holding };
}
