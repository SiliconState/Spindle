// Loads the wasm-bindgen Node glue for spindle-embed and checks the runtime the
// package stands on: SQLite and getrandom inside wasm32-unknown-unknown, an authority
// opened behind a JavaScript guard that is released once, after SQLite closes.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const glue = process.argv[2];
if (!glue) throw new Error('usage: node node_probe.mjs SPINDLE_EMBED_GLUE.js');
const spindle = createRequire(import.meta.url)(glue);

const released = [];
const report = JSON.parse(
  spindle.probe(() => released.push('first'), () => released.push('second')),
);
assert.deepEqual([...released].sort(), ['first', 'second'], 'each guard released once');
const [one, two] = report.stores;
assert.notEqual(one.identity, two.identity, 'each store draws its own identity');
for (const store of report.stores) {
  assert.equal(store.revision, 1);
  assert.equal(store.unknown_until_receipt, true);
  assert.equal(store.conflicting_receipt_refused, true);
}
assert.equal(report.process_id, 0, 'Wasm reports a diagnostic process id of 0');

let refused = 0;
spindle.refusedOpen(() => refused++);
assert.equal(refused, 1, 'a refused open still releases its guard');

console.log(
  'embed-probe passed: two memory authorities in wasm32 (admit/reuse, commit, unknown, ' +
    'receipt/conflict), distinct getrandom identities, guards released after close and on refusal',
);
