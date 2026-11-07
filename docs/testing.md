# Testing and verification

Spindle's tests are the behavioral reference for this documentation. This page lists the
suites, what each one exercises, how to run them, and what the CI workflow checks. No
test needs a provider account: model behavior comes from scripted loopback providers
unless a test says it uses a real provider.

## Quick gates

From `spindle/`:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
```

From `dext-engine/`:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
```

Plain `cargo test` at the `spindle/` root runs only the small reference-model package;
use `--workspace --all-features` to include `spindle-store` and `spindle-embed`.

## Rust suites

### Reference model (`spindle/tests/`)

| File | Exercises |
| --- | --- |
| `authority.rs` | The in-memory authority rules: admission, publication, receipts, epochs |
| `lifecycle.rs` | Whole-lifecycle sequences against the reference model |
| `replay.rs` | Reconstructing every cut of a retained history without a live authority; refusing damaged or incompatible histories |
| `publication_oracle.rs` | An independent oracle checking the model's publication and monotonic history under faults |

### Store and hosts (`spindle/store/tests/`)

| File | Exercises |
| --- | --- |
| `admission.rs`, `transition.rs` | Admission consumption, owner lock and schema checks, and atomic revision publication |
| `workspace.rs`, `artifact.rs` | Workspace trees, manifests, objects and garbage collection |
| `ownership.rs` | Owned child scopes: attachment, cancellation, suspension, settlement and takeover |
| `connection.rs` | Caller-supplied connections, schema-19 in-place/image migration, lock contention, the shared owner fence, and native metadata writes refused for embedded-owned or moved stores |
| `turn.rs`, `dext_recovery.rs` | Turn scheduling and recovery, including Unix kill-at-a-point cases |
| `driver.rs` | The portable driver against a scripted host (builds with `--no-default-features --features driver`) |
| `traces.rs` | Characterization traces of whole turns (`fixtures/traces/*.json`): plain, tools, files, app tools, refusals, drops, cuts, reopen cases, steering, compaction |
| `model_wait.rs` | Saved model responses: reused after a reopen, tool intents published once, a kill between receipt and adoption |
| `native_transcript.rs`, `image.rs`, `worker_lifecycle.rs` | Native transcripts and the pinned image worker (Linux); expected fault-injection panics are printed by the lifecycle cases |
| `usage.rs`, `usage_turns.rs` | Spend accounting and budgets: compaction summaries, nested `model()` calls, retries, drops, send-now cuts, rewinds and restarts |
| `pack_session.rs` | Session packs across whole turns |
| `migration.rs` | Older-schema upgrades, packed schema-20 objects with stable digests, migration backups and refusal of damaged records |
| `power_loss.rs` | Power loss and disk-full through a crash-simulating SQLite VFS (`support/crash_vfs.rs`; Linux) |
| `bundle.rs` | Store images and session bundles, including damaged and hostile input. Every case is read both in memory and unpacked from a file into a staging directory, with the same answer word for word (or the same files, byte for byte); a bundle's manifest is checked before any file is read; a store's settings too large for a sidecar are refused before they are kept; a previous-schema store is checked where it lies and left byte for byte as it came |
| `import.rs` | Native imports (`agent::import_session`): a bundle refused at any step (its checksum, a file's digest after others were staged, a store, a sidecar) leaves nothing staged or in place; each import stages in a new, private directory, and one a stopped import left is swept while a live import's is kept; a relative path beginning `file:` is checked as a path. On Linux, child-process probes of peak memory: an import of a store over 64 MiB grows the peak by less than half the store (a read in memory grows past it), a store of 64 tasks' 1 MiB answers is checked one task at a time, and stores with settings far over 4 MiB are refused before their settings are kept |
| `api.rs` | `spindle api` end to end over JSON-RPC (Unix), plus the Node and Python client smoke tests when those interpreters are installed. The golden frames in `fixtures/protocol/` are checked by a unit test in `src/api/stdio.rs` |
| `embedded.rs` | Native/embedded parity, shared usage and budgets (nested calls, summaries and task trees), queued tasks, stopped reports, resume and settings fit |
| `bench.rs` | Ignored local benchmark of the embedded host built natively: small sessions, long history reads/reopens/exports, compaction, timings and peak resident memory |
| `host_tools.rs`, `self_edit.rs` | Host tools and self-edit threads end to end through `spindle serve` |

`power_loss.rs` runs a sample of crash points by default; `SPINDLE_POWER_LOSS=all` sweeps
every point and takes much longer.

Some cases need Linux namespaces and `bubblewrap`, delegated cgroup v2, or interpreters
(Python, Node). Where a requirement is missing, the test reports a skip; a skip is not
evidence that the path ran. Most `#[ignore]` entries are child-process entry points that
parent crash tests invoke, not disabled coverage.

### Engine (`dext-engine/*/tests/`)

| Crate | Files | Exercises |
| --- | --- | --- |
| `dext-core` | `turn.rs`, `response.rs`, `model_wait.rs`, `image.rs` | Turn, response and model-wait state; image framing |
| `dext-transcript` | `checkpoint.rs`, `continuation.rs`, `runtime_checkpoint.rs`, `turn.rs` | Checkpoint codecs and checked continuation |
| `dext-native` | `strict.rs`, `strict_reuse.rs`, `strict_slots.rs`, `host_runner.rs`, `cgroup.rs` | Shared-mapping writes retained on Linux, workspace reuse by place without stale links/modes/leftovers, runner slots, host runner, cgroup cleanup |
| `dext-provider` | `portable.rs` | Provider shaping without native features |

The engine host-runner scratch sweep is user-wide: running independent workspaces'
crash suites concurrently can leave abandoned candidates that another suite sweeps.
A nonzero cleanup count alone is not evidence that its live run leaked. Keep this
failure's output and rerun the affected suites separately when diagnosing it.

## JavaScript suites

| Command (from `spindle/`) | Exercises |
| --- | --- |
| `node --test store/tests/ui.mjs store/tests/ui_app.mjs` | The web UI against a simulated DOM and API |
| `SPINDLE_BROWSER=/path/to/chromium node --test store/tests/ui_browser.mjs` | The web UI in a real headless browser |
| `SPINDLE_BIN=target/release/spindle node clients/js/smoke_test.mjs` | The Node client against a real `spindle api` |
| `SPINDLE_BIN=target/release/spindle python3 clients/python/smoke_test.py` | The Python client |
| `embed/tests/package.sh WASM_BINDGEN` | Builds the wasm package and runs Node and optional OPFS browser tests. Includes `node_compact.mjs`: shrink session/task stores, preserve history/files/head, remove interrupted copies and refuse busy sessions (needs `node:sqlite`, Node 22.5+) |
| `SPINDLE_KEY=... SPINDLE_REAL_BROWSER=/path/to/chrome SPINDLE_BIN=target/release/spindle node embed/tests/browser_real.mjs KIT_DIR --provider ID [--model M] [--catalog FILE]` | The package in a real browser against a real provider (`KIT_DIR` from `embed/kit.sh`): stop and resume, steering, a killed browser's unknown request, `retry`, and a bundle out to a native host and back. Real, billed requests |
| `store/tests/wasi_probe.sh WASI_LIBC_ROOT` | The store compiled for WASI (against an extracted wasi-libc package), run under Node |

The embedded [`browser_opfs.mjs`](../spindle/embed/tests/browser_opfs.mjs) checks
worker/session exclusion, reopen after Chrome restarts, and OPFS compaction with
history and files intact when sibling Node glue and `node:sqlite` can create the
fragmented fixture. [`node_opfs_replace.mjs`](../spindle/embed/tests/node_opfs_replace.mjs)
also checks short header I/O, same-path rename, and interrupted replacements: a rename
whose record cannot be written changes nothing and the pool goes on; one that fails
after its record stops the pool (every later call fails) until the next open finishes
it; a file whose two names fill more than a header renames without a record.
[`node_opfs_torn.mjs`](../spindle/embed/tests/node_opfs_torn.mjs) replays each rename
(a directory set aside, a file replacing another, a file renamed to a new name with bytes
or empty), a file made and one deleted, cut at every byte and every set of 512-byte
sectors of each header write, and with each write its file had not flushed left out,
over slots this version and an older one wrote. Every reopened pool must hold each file
whole under one of its two names, a directory never split, no name made of two, and
every header must read the same with the older version's reader; a header damaged
otherwise is kept apart until reclaimed. [`node_session.mjs`](../spindle/embed/tests/node_session.mjs)
runs the package's sessions under Node end to end (app tools and runners, stops, a killed
owner, budgets, background tasks), including a stopped turn ending its own app call (its
handler's `call.signal` aborts with an `AbortError` whose `outcome` is `stopped`, and no
answer is sent for it) while a background task's call goes on and is answered.

The browser extension and the browser Linuxes have their own suites; see
[chrome.md](chrome.md#tests) and [browser-linux.md](browser-linux.md). Tests that take a
real provider key send real, billed requests and say so in their usage line.

## Portable feature gates

The decision driver and the embedded host must build without native code. CI also
lints and tests the bare store and each of `dext-turn`, `dext-image-continuation`,
`driver`, `workspace-tools`, `shape`, `embedded` and `cli` independently, with
`--no-default-features`. All-feature tests alone cannot detect optional-dependency
leaks: the JSON-dependent task-answer amendment is compiled only with `driver`.

```sh
cargo clippy --locked -p spindle-store --no-default-features --features driver --lib \
  --target wasm32-unknown-unknown -- -D warnings
cargo clippy --locked -p spindle-store --no-default-features --features embedded --lib \
  --target wasm32-unknown-unknown -- -D warnings
cargo tree --locked -p spindle-store --no-default-features --features driver -e normal
```

The driver's normal dependency tree must not contain `dext-native`, `dext-provider`,
`reqwest` or `tokio`; the embedded host's wasm tree must not contain `reqwest`, `tokio`,
`image`, `ring` or `rustls`.

## Continuous integration

[`.github/workflows/spindle.yml`](../.github/workflows/spindle.yml) runs on every branch push and
pull request, except those that change only the website or the release workflow:

| Job | Runs |
| --- | --- |
| `linux` | fmt and Clippy, debug and release workspace tests, engine tests, Node/Python clients, UI tests, `cargo audit` and `cargo deny --workspace --all-features check licenses` in both workspaces (see [Dependency policy](#dependency-policy)) |
| `portable` | Clippy and tests of `spindle-store` for each portable feature set, plus the native-free dependency checks |
| `wasm` | Provider/wire wasm32 builds, store/WASI, Node/OPFS, browser Linux and both browser editions; scripted extension and sign-in/vault suites, focused Node checks, renewal and real-Chrome review |
| `windows` | Native store, CLI and engine tests on Windows |

Two more workflows publish. [`pages.yml`](../.github/workflows/pages.yml) deploys `site/` to
GitHub Pages when it changes. [`browser-release.yml`](../.github/workflows/browser-release.yml),
on a `browser-v*` tag, builds both source/GitHub and store editions, tests the exact
unpacked zips with `extension_real.mjs --scripted` and `signin_vault.mjs`, and tests
the source zip with `review_browser.mjs` and
`node --expose-gc chrome/tests/engine_renew.mjs UNPACKED/web`. It checks
`build.json`'s version, edition and source fingerprints, then attests and
attaches both zips and their SHA-256 files to the release (see
[chrome.md](chrome.md#install-a-release)).

The extension's focused suites are linked in [chrome.md](chrome.md#tests):
`panel_tree.mjs` checks closed-folder rendering, change counts, filter/redraw state
and navigation through large trees; `linux_warm.mjs` checks lazy v86 starts;
`vault_host.mjs` checks sealing and passphrase handling for the host token;
`engine_renew.mjs` checks old wasm memory is released and fresh sessions work.
`review_browser.mjs` covers contained/static previews and version choices,
template/reparse sanitizing, WebRTC and DNS leakage checks with a positive netlog
control, background task cards/reports, queued file messages, reset backup cleanup,
one copy per moved thread, move and index updates sharing one strict transaction, and
Storage > Free. Its document previews (since 0.1.7) use
[`office_fixtures.mjs`](../spindle/chrome/tests/office_fixtures.mjs): a Word, an Excel
and a PowerPoint file written as those programs write them (stored and deflated parts),
a PDF, an SVG without its namespace, one whose markup is not XML, a short WAV, and
documents whose numbers are out of every bound. The reader's output is checked (text,
styles, lists, merged cells, pictures, number formats; hidden text and very hidden sheets
left out; a sheet too large to parse saying so), the out-of-bound documents still drawn
within 15 seconds with their numbers bounded, and a damaged or renamed file refused with
the reason. Then each file opens in a tab of its own: the PDF's tab becomes Chrome's own
viewer of it, served by the service worker; each document shows its text in the preview
frame under a `script-src 'none'` policy; the SVGs draw (the loose one as a page) and
the WAV is ready to play. The workspace page shows the same previews and offers each in
a tab. `review_regressions.mjs` also checks what 0.1.7 added: the service worker's
PDF address (a thread's PDF answered as `application/pdf` with `nosniff`, read from the
engine over a channel closed after each read; a page, an SVG, a bad thread id, an empty,
`..` or encoded-slash path and a bad escape refused before anything is read; a site's
`/file/` address, another method and Spindle's other pages left untouched by its fetch
handler); an allowed action refused, nothing done, once its tab is on another site, page
or address, its part changed, or another dialog is open; a turn's Stop ending its own
page actions, not its background tasks'; a thread file over 1 GB refused before it is
read; the engine freeing the pool's unreadable slots (Settings, Storage, Unreadable),
saying how many bytes; a message that comes back
going only into its own thread's box, kept in storage until that thread opens; and Bring
back leaving a thread whose `bundle_bytes` is over 1 GB on the host, nothing exported or
sealed, while an older host that does not report it is asked for the bundle as before.
These tests use scripted services, not provider accounts.

## Local engine benchmarks

From `spindle/`, run
[`embed/tests/bench.sh`](../spindle/embed/tests/bench.sh):

```sh
embed/tests/bench.sh [--quick] /path/to/wasm-bindgen OUT_DIR [EARLIER_DIR]
```

It builds release wasm and runs [`bench.mjs`](../spindle/embed/tests/bench.mjs)
under Node, then [`store/tests/bench.rs`](../spindle/store/tests/bench.rs) natively
with `--ignored --nocapture`. A model that answers immediately isolates local
engine time and memory from network latency. Reports are `wasm.json` and
`native.json`; an earlier report directory prints side-by-side changes. Scenarios
cover small sessions, long histories and compaction; wasm also measures file trees
and concurrent sessions. They are local measurements, not a CI timing gate.

## Dependency policy

`cargo deny --workspace --all-features check licenses` checks the licenses of every
crate in the workspace and all of their dependencies, with every feature on. It allows only
the licenses of the nearest `deny.toml`: `spindle/deny.toml` in `spindle/`, and the root
`deny.toml` in `dext-engine/`, which has none of its own. Both allow the same permissive set:
MIT, Apache-2.0, BSD-3-Clause, BSL-1.0, ISC, Zlib, Unicode-3.0, Unlicense and
CDLA-Permissive-2.0. `cargo audit --deny warnings` checks both lockfiles against the RustSec
advisory database. Lockfiles are committed and every build uses `--locked`.
