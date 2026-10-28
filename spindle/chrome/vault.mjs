// Provider secrets at rest: API keys and browser sign-ins. Each record in the 'keys'
// store is sealed with AES-GCM (256-bit) under the vault key, its provider bound in as
// additional data, so no secret is plain text on disk and no record can be moved to
// another provider. The vault key is one of two kinds:
//
//   device      a non-extractable key in this extension's IndexedDB. Nothing to type;
//               secrets are never written as text. Whoever copies the whole Chrome
//               profile copies this key with them.
//   passphrase  the vault key wrapped (AES-KW) by a key derived from the user's
//               passphrase (PBKDF2-SHA-256, 600,000 rounds). A copied profile holds
//               nothing usable without the passphrase. Unlocked once per engine start,
//               then held only in the engine's memory.
//
// Only the engine worker uses this module: it alone reads, writes and refreshes
// secrets. Changes run one at a time, and a change of vault key re-seals every record
// in the same transaction that replaces the key, so a crash leaves the old vault or the
// new one, never a mix. Opening a record costs one AES-GCM decryption (microseconds).
import { entriesOf, read, writeTogether } from './shared.mjs';

const ROUNDS = 600_000;
const MIN_PASSPHRASE = 8;
const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** The vault key while it is usable (device: once read; passphrase: once unlocked). */
let current = null;
/** Vault changes and secret reads, one after another. */
let queue = Promise.resolve();

function exclusive(work) {
  const run = queue.then(work);
  queue = run.catch(() => {});
  return run;
}

export class VaultError extends Error {
  constructor(kind, message) {
    super(message);
    this.kind = kind;
  }
}

const LOCKED = () => new VaultError('locked', "Spindle's keys are locked: unlock them in Settings, Providers");

const newKey = (extractable) => crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, extractable, ['encrypt', 'decrypt']);

/** The vault record, made (as a device vault) the first time one is needed. */
async function vault() {
  const kept = await read('kv', 'vault');
  if (kept) return kept;
  const made = { version: 1, mode: 'device', key: await newKey(false) };
  await writeTogether([['kv', 'vault', made]]);
  return made;
}

async function usable() {
  if (current) return current;
  const kept = await vault();
  if (kept.mode === 'device') return (current = kept.key);
  throw LOCKED();
}

async function seal(key, provider, secret) {
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const data = await crypto.subtle.encrypt(
    { name: 'AES-GCM', iv, additionalData: encoder.encode(provider) },
    key,
    encoder.encode(JSON.stringify(secret)),
  );
  return { version: 1, kind: secret.kind, iv, data };
}

/** A record's secret. A plain string is a key saved before records were sealed. */
async function open(key, provider, record) {
  if (typeof record === 'string') return { kind: 'key', key: record };
  const plain = await crypto.subtle.decrypt({ name: 'AES-GCM', iv: record.iv, additionalData: encoder.encode(provider) }, key, record.data)
    .catch(() => { throw new VaultError('damaged', `the saved credential for ${provider} cannot be read; save it again`); });
  return JSON.parse(decoder.decode(plain));
}

async function wrapping(passphrase, salt, rounds) {
  // Normalized, so the same passphrase typed through another keyboard or input method
  // gives the same key.
  const material = await crypto.subtle.importKey('raw', encoder.encode(passphrase.normalize('NFC')), 'PBKDF2', false, ['deriveKey']);
  return crypto.subtle.deriveKey({ name: 'PBKDF2', hash: 'SHA-256', salt, iterations: rounds }, material, { name: 'AES-KW', length: 256 }, false, ['wrapKey', 'unwrapKey']);
}

async function unwrapped(kept, passphrase, extractable) {
  const kek = await wrapping(passphrase, kept.salt, kept.rounds);
  return crypto.subtle.unwrapKey('raw', kept.wrapped, kek, 'AES-KW', { name: 'AES-GCM', length: 256 }, extractable, ['encrypt', 'decrypt'])
    .catch(() => { throw new VaultError('denied', 'that is not the passphrase'); });
}

function checked(passphrase) {
  if (typeof passphrase !== 'string' || [...passphrase].length < MIN_PASSPHRASE) {
    throw new VaultError('invalid', `a passphrase needs at least ${MIN_PASSPHRASE} characters`);
  }
  return passphrase;
}

/** Every record re-sealed from key `from` to key `to`, as changes for writeTogether. */
async function resealed(from, to) {
  const changes = [];
  for (const [provider, record] of await entriesOf('keys')) {
    changes.push(['keys', provider, await seal(to, provider, await open(from, provider, record))]);
  }
  return changes;
}

/** The vault for the panel: its mode, whether it is locked, each saved credential's kind
 * by provider, and (while unlocked) the account each sign-in names and the sign-ins the
 * provider ended. Read outside the queue (one transaction, nothing written), so a token
 * refresh in progress never holds it up. */
export async function state() {
  const kept = await read('kv', 'vault');
  if (!kept) return { mode: 'device', locked: false, kinds: {}, accounts: {}, ended: {} };
  const key = current ?? (kept.mode === 'device' ? kept.key : null);
  const kinds = {};
  const accounts = {};
  const ended = {};
  for (const [provider, record] of await entriesOf('keys')) {
    kinds[provider] = typeof record === 'string' ? 'key' : record?.kind ?? 'key';
    if (key && kinds[provider] === 'signin') {
      const held = await open(key, provider, record).catch(() => ({}));
      accounts[provider] = typeof held.account === 'string' ? held.account : null;
      if (held.ended) ended[provider] = true;
    }
  }
  return { mode: kept.mode, locked: !key, kinds, accounts, ended };
}

/** Refreshes in progress, by provider: one at a time each, outside the queue. */
const renewing = new Map();

/** `provider`'s secret, or undefined. `renew` (optional) may replace it before it is
 * used (a sign-in refreshing its token). It runs outside the queue, one per provider at
 * a time (callers meanwhile get its result), and what it returns is sealed and kept
 * unless the record changed meanwhile (signed out, or signed in again). */
export async function secret(provider, renew) {
  const held = await exclusive(async () => {
    const record = await read('keys', provider);
    if (record === undefined) return undefined;
    const key = await usable();
    const opened = await open(key, provider, record);
    // A key saved before records were sealed is sealed now that it is read.
    if (typeof record === 'string') await writeTogether([['keys', provider, await seal(key, provider, opened)]]);
    return opened;
  });
  if (held === undefined || !renew) return held;
  if (renewing.has(provider)) return renewing.get(provider);
  const run = (async () => {
    const next = await renew(held);
    if (!next) return held;
    await exclusive(async () => {
      const record = await read('keys', provider);
      const key = await usable();
      const now = record === undefined ? null : await open(key, provider, record).catch(() => null);
      if (now?.kind === held.kind && now.refresh === held.refresh) {
        await writeTogether([['keys', provider, await seal(key, provider, next)]]);
      }
    });
    return next;
  })();
  renewing.set(provider, run);
  run.finally(() => renewing.delete(provider)).catch(() => {});
  return run;
}

/** Keeps `secret` ({kind: 'key', key} or {kind: 'signin', ...}) for `provider`. */
export function keep(provider, secret) {
  return exclusive(async () => writeTogether([['keys', provider, await seal(await usable(), provider, secret)]]));
}

export function forget(provider) {
  return exclusive(() => writeTogether([['keys', provider, undefined]]));
}

/** Seals keys saved before records were sealed (an update from such a build). */
export function sealPlain() {
  return exclusive(async () => {
    const kept = await vault();
    if (kept.mode !== 'device') return;
    const changes = [];
    for (const [provider, record] of await entriesOf('keys')) {
      if (typeof record === 'string') changes.push(['keys', provider, await seal(kept.key, provider, { kind: 'key', key: record })]);
    }
    if (changes.length) await writeTogether(changes);
  });
}

export function unlock(passphrase) {
  return exclusive(async () => {
    const kept = await vault();
    if (kept.mode !== 'passphrase') return;
    current = await unwrapped(kept, String(passphrase ?? ''), false);
  });
}

/** Whether credentials can be read and kept now (not a locked passphrase vault). */
export function usableNow() {
  return exclusive(async () => {
    try {
      await usable();
      return true;
    } catch {
      return false;
    }
  });
}

export function lock() {
  return exclusive(async () => {
    if ((await vault()).mode === 'passphrase') current = null;
  });
}

/** Locks the vault with `passphrase`, or changes it (`previous` is the one it has).
 * Either way every record is re-sealed under a fresh key, so neither the device key nor
 * an earlier passphrase's wrapped key opens what is kept from now on. */
export function protect(passphrase, previous) {
  return exclusive(async () => {
    checked(passphrase);
    const kept = await vault();
    const from = kept.mode === 'passphrase' ? await unwrapped(kept, String(previous ?? ''), false) : kept.key;
    const salt = crypto.getRandomValues(new Uint8Array(16));
    const kek = await wrapping(passphrase, salt, ROUNDS);
    // Wrapping needs the new key's bytes once; the copy kept in memory cannot give them.
    const fresh = await newKey(true);
    const wrapped = await crypto.subtle.wrapKey('raw', fresh, kek, 'AES-KW');
    // In use from here: the same key, not extractable.
    const key = await crypto.subtle.unwrapKey('raw', wrapped, kek, 'AES-KW', { name: 'AES-GCM', length: 256 }, false, ['encrypt', 'decrypt']);
    await writeTogether([...(await resealed(from, key)), ['kv', 'vault', { version: 1, mode: 'passphrase', salt, rounds: ROUNDS, wrapped }]]);
    current = key;
  });
}

/** Back to a device vault: the passphrase is checked, and every record re-sealed under a
 * new device key. */
export function unprotect(passphrase) {
  return exclusive(async () => {
    const kept = await vault();
    if (kept.mode !== 'passphrase') return;
    const from = await unwrapped(kept, String(passphrase ?? ''), false);
    const key = await newKey(false);
    await writeTogether([...(await resealed(from, key)), ['kv', 'vault', { version: 1, mode: 'device', key }]]);
    current = key;
  });
}

/** A forgotten passphrase: every saved key and sign-in is deleted, and a new device vault
 * takes the old one's place, in one transaction. Threads are not touched. */
export function reset() {
  return exclusive(async () => {
    const key = await newKey(false);
    const gone = (await entriesOf('keys')).map(([provider]) => ['keys', provider, undefined]);
    await writeTogether([...gone, ['kv', 'vault', { version: 1, mode: 'device', key }]]);
    current = key;
  });
}
