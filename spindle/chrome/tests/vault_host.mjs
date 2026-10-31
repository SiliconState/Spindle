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
export async function writeTogether(changes) {
  for (const [name, key, value] of changes) value === undefined ? store(name).delete(key) : store(name).set(key, value);
}`;
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

// Forgotten and reset: gone.
await vault.forgetHost([['kv', 'host', undefined]]);
assert.equal(await vault.hostToken(), undefined);
assert.equal(stores.get('kv').has('host'), false);
await vault.keepHost(TOKEN);
await vault.reset();
assert.equal(await vault.hostToken(), undefined, 'a reset takes the host token with the keys');
assert.equal((await vault.state()).host, false);

console.log('vault-host passed: the host token sealed (never text), listed apart, locked and unlocked with the keys, re-sealed by a new passphrase, bound to its name, gone with a reset');
