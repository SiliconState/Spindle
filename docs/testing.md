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
| `bundle.rs` | Store images and session bundles, including damaged and hostile input |
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

## JavaScript suites

| Command (from `spindle/`) | Exercises |
| --- | --- |
| `node --test store/tests/ui.mjs store/tests/ui_app.mjs` | The web UI against a simulated DOM and API |
| `SPINDLE_BROWSER=/path/to/chromium node --test store/tests/ui_browser.mjs` | The web UI in a real headless browser |
| `SPINDLE_BIN=target/release/spindle node clients/js/smoke_test.mjs` | The Node client against a real `spindle api` |
| `SPINDLE_BIN=target/release/spindle python3 clients/python/smoke_test.py` | The Python client |
| `embed/tests/package.sh WASM_BINDGEN` | Builds the wasm package and runs Node and optional OPFS browser tests. Includes `node_compact.mjs`: shrink session/task stores, preserve history/files/head, remove interrupted copies and refuse busy sessions (needs `node:sqlite`, Node 22.5+) |
| `store/tests/wasi_probe.sh WASI_LIBC_ROOT` | The store compiled for WASI (against an extracted wasi-libc package), run under Node |

The embedded [`browser_opfs.mjs`](../spindle/embed/tests/browser_opfs.mjs) checks
worker/session exclusion, reopen after Chrome restarts, and OPFS compaction with
history and files intact when sibling Node glue and `node:sqlite` can create the
fragmented fixture. [`node_opfs_replace.mjs`](../spindle/embed/tests/node_opfs_replace.mjs)
also checks short header I/O, same-path rename, and interrupted replacements.

The browser extension and the browser Linuxes have their own suites; see
[chrome.md](chrome.md#tests) and [browser-linux.md](browser-linux.md). Tests that take a
real provider key send real, billed requests and say so in their usage line.

## Portable feature gates

The decision driver and the embedded host must build without native code:

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
| `wasm` | Provider and wire crates on wasm32, browser-target store, WASI, embedded Node/OPFS tests, browser Linux, scripted extension, plus `review_regressions`, `linux_route`, `panel_steps`, `linux_warm`, `vault_host`, `engine_renew` and real-Chrome `review_browser` |
| `windows` | Native store, CLI and engine tests on Windows |

Two more workflows publish. [`pages.yml`](../.github/workflows/pages.yml) deploys `site/` to
GitHub Pages when it changes. [`browser-release.yml`](../.github/workflows/browser-release.yml),
on a `browser-v*` tag, builds Spindle for Browser and tests the exact unpacked zip
with `extension_real.mjs --scripted`, `review_browser.mjs` and
`node --expose-gc chrome/tests/engine_renew.mjs UNPACKED/web`. It checks
`build.json`'s version and both 12-hex source fingerprints, then attests and
attaches the zip and SHA-256 to the release (see
[chrome.md](chrome.md#install-a-release)).

The extension's focused suites are linked in [chrome.md](chrome.md#tests):
`panel_tree.mjs` checks closed-folder rendering, change counts, filter/redraw state
and navigation through large trees; `linux_warm.mjs` checks lazy v86 starts;
`vault_host.mjs` checks sealing and passphrase handling for the host token;
`engine_renew.mjs` checks old wasm memory is released and fresh sessions work.
`review_browser.mjs` covers contained/static previews and version choices,
template/reparse sanitizing, WebRTC and DNS leakage checks with a positive netlog
control, background task cards/reports, queued file messages, reset backup cleanup,
one copy per moved thread and Storage > Free. These tests use scripted services,
not provider accounts.

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
