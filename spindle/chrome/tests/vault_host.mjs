// The host's token in the vault (a Node test of vault.mjs, its IndexedDB records held in
// memory): kept sealed, never as text; listed apart from providers' credentials;
// unreadable while a passphrase vault is locked and readable again once unlocked;
// re-sealed with every other record when the passphrase changes; gone with a reset.
import assert from 'node:assert/strict';
import { register } from 'node:module';

// vault.mjs reads and writes through shared.mjs: here, maps in memory.
const stores = new Map();
globalThis.__vaultStores = stores;
const fake = `
const stores = globalThis.__vaultStores;
const store = (name) => { if (!stores.has(name)) stores.set(name, new Map()); return stores.get(name); };
export async function read(name, key) { return structuredClone(store(name).get(key)); }
export async function entriesOf(name) { return [...store(name)].map(([key, value]) => [key, structuredClone(value)]); }
export async function writeTogether(changes, durability = null) {
  for (const [name, key, value] of changes) {
    value === undefined ? store(name).delete(key) : store(name).set(key, value);
    globalThis.__vaultWrites.push({ name, key, durability });
  }
}`;
/** Each record written: its store, key and the durability it was written with. */
const writes = [];
globalThis.__vaultWrites = writes;
register(`data:text/javascript,${encodeURIComponent(`
export async function resolve(specifier, context, next) {
  if (specifier === './shared.mjs' && context.parentURL?.endsWith('/vault.mjs')) {
    return { url: 'data:text/javascript,' + encodeURIComponent(${JSON.stringify(fake)}), shortCircuit: true };
  }
  return next(specifier, context);
}`)}`);
const vault = await import('../vault.mjs');

const TOKEN = 'spindle-host-token-abc123';
const text = (value) => JSON.stringify(value, (key, item) => (ArrayBuffer.isView(item) || item instanceof ArrayBuffer ? [...new Uint8Array(item.buffer ?? item)] : item));
const plainAnywhere = () => [...stores.values()].some((records) => [...records.values()].some((record) => text(record).includes(TOKEN)));

await vault.keep('openai', { kind: 'key', key: 'sk-provider' });
await vault.keepHost(TOKEN, [['kv', 'host', { url: 'https://host.example' }]]);
assert.equal(await vault.hostToken(), TOKEN);
assert.deepEqual(stores.get('kv').get('host'), { url: 'https://host.example' }, 'the URL kept beside, readable');
assert.equal(plainAnywhere(), false, 'the token is never kept as text');
let state = await vault.state();
assert.deepEqual(state.kinds, { openai: 'key' }, 'not listed among providers');
assert.equal(state.host, true);

// A passphrase: the host's token re-sealed with the rest, and locked with them.
await vault.protect('correct horse battery', undefined);
assert.equal(await vault.hostToken(), TOKEN);
await vault.lock();
await assert.rejects(vault.hostToken(), (error) => error.kind === 'locked');
assert.equal(await vault.usableNow(), false);
await assert.rejects(vault.keepHost('other'), (error) => error.kind === 'locked', 'nothing kept while locked');
await vault.unlock('correct horse battery');
assert.equal(await vault.hostToken(), TOKEN);
assert.equal(plainAnywhere(), false);

// A record cannot be moved to another name: the host's opens only as the host's.
stores.get('keys').set('openai', stores.get('keys').get(':host'));
await assert.rejects(vault.secret('openai'), (error) => error.kind === 'damaged');
await vault.forget('openai');

// A sign-in kept, and its refresh token rotated: on disk before either is used (a power
// cut must not bring back a spent refresh token).
const durabilityOf = (key) => writes.filter((write) => write.name === 'keys' && write.key === key).at(-1)?.durability;
await vault.keep('chatgpt', { kind: 'signin', access: 'a1', refresh: 'r1' });
assert.equal(durabilityOf('chatgpt'), 'strict', 'a kept secret is written durably');
writes.length = 0;
const renewed = await vault.secret('chatgpt', async (held) => ({ ...held, access: 'a2', refresh: 'r2' }));
assert.equal(renewed.refresh, 'r2');
assert.equal(durabilityOf('chatgpt'), 'strict', 'a rotated refresh token is written durably');
assert.equal((await vault.secret('chatgpt')).refresh, 'r2');
await vault.forget('chatgpt');

const deferred = () => {
  let resolve;
  const promise = new Promise((done) => { resolve = done; });
  return { promise, resolve };
};
for (const changed of ['forgotten', 'replaced', 'unchanged']) {
  await vault.keep('chatgpt', { kind: 'signin', access: 'before', refresh: 'before-refresh' });
  const entered = deferred();
  const finish = deferred();
  const renewing = vault.secret('chatgpt', async (held) => {
    entered.resolve();
    await finish.promise;
    return changed === 'unchanged' ? null : { ...held, access: 'after', refresh: 'after-refresh' };
  });
  await entered.promise;
  if (changed === 'replaced') await vault.keep('chatgpt', { kind: 'key', key: 'replacement' });
  else await vault.forget('chatgpt');
  finish.resolve();
  const result = await renewing;
  assert.deepEqual(result, changed === 'replaced' ? { kind: 'key', key: 'replacement' } : undefined,
    'a pending refresh cannot return a forgotten or replaced credential, even with no update');
  assert.deepEqual(await vault.secret('chatgpt'), result);
}
for (const changed of ['identical', 'renew-failed', 'no-update', 'reset-identical']) {
  const original = { kind: 'signin', access: 'before', refresh: 'before-refresh' };
  await vault.keep('chatgpt', original);
  const entered = deferred();
  const finish = deferred();
  const pending = vault.secret('chatgpt', async (held) => {
    entered.resolve();
    await finish.promise;
    if (changed === 'renew-failed') throw new Error('old sign-in could not refresh');
    return changed === 'no-update' ? null : { ...held, access: 'after', refresh: 'after-refresh' };
  });
  await entered.promise;
  if (changed === 'reset-identical') await vault.reset();
  else await vault.forget('chatgpt');
  await vault.keep('chatgpt', original);
  finish.resolve();
  assert.deepEqual(await pending, original, 'a new save is not the old refresh, even with identical values');
  assert.deepEqual(await vault.secret('chatgpt'), original);
}
for (const changed of ['same-refresh', 'locked', 'reset']) {
  await vault.keep('chatgpt', { kind: 'signin', access: 'before', refresh: 'before-refresh' });
  const entered = deferred();
  const finish = deferred();
  const renewing = vault.secret('chatgpt', async (held) => {
    entered.resolve();
    await finish.promise;
    return { ...held, access: 'after', refresh: 'after-refresh' };
  });
  await entered.promise;
  if (changed === 'same-refresh') await vault.keep('chatgpt', { kind: 'signin', access: 'replacement', refresh: 'before-refresh' });
  else if (changed === 'locked') {
    await vault.protect('correct horse battery', 'correct horse battery');
    await vault.lock();
  } else await vault.reset();
  finish.resolve();
  if (changed === 'locked') {
    await assert.rejects(renewing, (error) => error.kind === 'locked');
    await vault.unlock('correct horse battery');
    await vault.unprotect('correct horse battery');
  } else assert.deepEqual(await renewing, changed === 'same-refresh'
    ? { kind: 'signin', access: 'replacement', refresh: 'before-refresh' } : undefined);
}
await vault.forget('chatgpt');

await vault.keep('chatgpt', { kind: 'signin', access: 'shared', refresh: 'shared-refresh' });
{
  const entered = deferred();
  const finish = deferred();
  let calls = 0;
  const renew = async (held) => {
    calls++;
    entered.resolve();
    await finish.promise;
    return { ...held, access: 'shared-after' };
  };
  const first = vault.secret('chatgpt', renew);
  const second = vault.secret('chatgpt', renew);
  await entered.promise;
  await vault.state();
  finish.resolve();
  assert.deepEqual(await first, await second, 'concurrent callers share the renewed record');
  assert.equal(calls, 1, 'only one refresh runs per provider');
  await assert.rejects(vault.secret('chatgpt', () => { throw new Error('refresh failed'); }), /refresh failed/);
  assert.equal((await vault.secret('chatgpt')).access, 'shared-after', 'a failed current refresh does not change storage');
}
await vault.forget('chatgpt');

// Forgotten and reset: gone.
await vault.forgetHost([['kv', 'host', undefined]]);
assert.equal(await vault.hostToken(), undefined);
assert.equal(stores.get('kv').has('host'), false);
await vault.keepHost(TOKEN);
await vault.reset();
assert.equal(await vault.hostToken(), undefined, 'a reset takes the host token with the keys');
assert.equal((await vault.state()).host, false);

console.log('vault-host passed: the host token sealed (never text), listed apart, locked and unlocked with the keys, re-sealed by a new passphrase, bound to its name, gone with a reset; kept and rotated sign-ins written durably');
