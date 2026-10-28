// What the panel and the engine share: the channel they talk over and the extension's
// IndexedDB (provider keys, the provider catalog, the thread index, and the file each
// moved thread went into). Both belong to this extension's origin: no web page can
// reach them.
export const CHANNEL = 'spindle-engine';

const STORES = ['kv', 'keys', 'threads', 'bundles'];
let opening = null;

function database() {
  opening ??= new Promise((resolve, reject) => {
    const request = indexedDB.open('spindle', 2);
    request.onupgradeneeded = () => {
      for (const name of STORES) {
        if (!request.result.objectStoreNames.contains(name)) request.result.createObjectStore(name);
      }
    };
    request.onsuccess = () => {
      // A newer version of the extension asking to upgrade: let it.
      request.result.onversionchange = () => {
        request.result.close();
        opening = null;
      };
      resolve(request.result);
    };
    request.onerror = () => reject(request.error);
  }).catch((error) => {
    opening = null;
    throw error;
  });
  return opening;
}

async function run(store, mode, work) {
  const db = await database();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(store, mode);
    const request = work(tx.objectStore(store));
    tx.oncomplete = () => resolve(request.result);
    tx.onerror = tx.onabort = () => reject(tx.error);
  });
}

export const read = (store, key) => run(store, 'readonly', (s) => s.get(key));
export const write = (store, key, value) => run(store, 'readwrite', (s) => s.put(value, key));
export const remove = (store, key) => run(store, 'readwrite', (s) => s.delete(key));
export const keysOf = (store) => run(store, 'readonly', (s) => s.getAllKeys());
export const valuesOf = (store) => run(store, 'readonly', (s) => s.getAll());
/** A store's records as [key, value] pairs, read in one transaction. */
export async function entriesOf(store) {
  const db = await database();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(store, 'readonly');
    const names = tx.objectStore(store).getAllKeys();
    const values = tx.objectStore(store).getAll();
    tx.oncomplete = () => resolve(names.result.map((name, index) => [name, values.result[index]]));
    tx.onerror = tx.onabort = () => reject(tx.error);
  });
}

/** Puts and deletes records of several stores in one transaction: all of them, or none.
 * `changes` are `[store, key, value]`; an undefined value deletes the record. */
export async function writeTogether(changes) {
  const db = await database();
  const stores = [...new Set(changes.map(([store]) => store))];
  return new Promise((resolve, reject) => {
    const tx = db.transaction(stores, 'readwrite');
    tx.oncomplete = () => resolve();
    tx.onerror = tx.onabort = () => reject(tx.error);
    try {
      for (const [store, key, value] of changes) {
        if (value === undefined) tx.objectStore(store).delete(key);
        else tx.objectStore(store).put(value, key);
      }
    } catch (error) {
      // None of them, then: a change refused here would otherwise leave the others.
      tx.abort();
      reject(error);
    }
  });
}

/** Changes the record at `key` in one transaction (`change` gets it, returns the new
 * one): two changes never undo each other. Resolves with the new record, or undefined
 * when there was none. */
export async function update(store, key, change) {
  const db = await database();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(store, 'readwrite');
    const records = tx.objectStore(store);
    let next;
    const got = records.get(key);
    got.onsuccess = () => {
      if (got.result === undefined) return;
      try {
        next = change(got.result);
        records.put(next, key);
      } catch (error) {
        tx.abort();
        reject(error);
      }
    };
    tx.oncomplete = () => resolve(next);
    tx.onerror = tx.onabort = () => reject(tx.error);
  });
}

/** The host permission pattern for a catalog provider (its base URL's origin). */
export function accessFor(profile) {
  const { origin } = new URL(profile.base_url);
  return `${origin}/*`;
}

/** Opens the engine's document (it runs every thread), unless it is open. */
export async function startEngine() {
  const open = await chrome.runtime.getContexts({ contextTypes: ['OFFSCREEN_DOCUMENT'] });
  if (open.length) return;
  try {
    await chrome.offscreen.createDocument({
      url: 'engine.html',
      reasons: ['WORKERS'],
      justification: 'Runs Spindle threads so they keep going while the panel is closed',
    });
  } catch (error) {
    // Another panel created it first.
    if (!/single offscreen|already/i.test(error.message)) throw error;
  }
}
