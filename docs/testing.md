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
| `connection.rs` | Caller-supplied connections, lock contention, and the fence shared with the embedded package |
| `turn.rs`, `dext_recovery.rs` | Turn scheduling and recovery, including Unix kill-at-a-point cases |
| `driver.rs` | The portable driver against a scripted host (builds with `--no-default-features --features driver`) |
| `traces.rs` | Characterization traces of whole turns (`fixtures/traces/*.json`): plain, tools, files, app tools, refusals, drops, cuts, reopen cases, steering, compaction |
| `model_wait.rs` | Saved model responses: reused after a reopen, tool intents published once, a kill between receipt and adoption |
| `native_transcript.rs`, `image.rs`, `worker_lifecycle.rs` | Native transcripts and the pinned image worker (Linux); expected fault-injection panics are printed by the lifecycle cases |
| `usage.rs`, `usage_turns.rs` | Spend accounting and budgets: compaction summaries, nested `model()` calls, retries, drops, send-now cuts, rewinds and restarts |
| `pack_session.rs` | Session packs across whole turns |
| `migration.rs` | Upgrading a store written by an older schema (`fixtures/schema14.sqlite.deflate`) |
| `power_loss.rs` | Power loss and disk-full through a crash-simulating SQLite VFS (`support/crash_vfs.rs`; Linux) |
| `bundle.rs` | Store images and session bundles, including damaged and hostile input |
| `api.rs` | `spindle api` end to end over JSON-RPC (Unix), plus the Node and Python client smoke tests when those interpreters are installed. The golden frames in `fixtures/protocol/` are checked by a unit test in `src/api/stdio.rs` |
| `embedded.rs` | The embedded host against the native session on the same scripted bytes |
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
| `dext-native` | `strict.rs`, `strict_slots.rs`, `host_runner.rs`, `cgroup.rs` | The strict runner, per-user slots, host runner, cgroup cleanup |
| `dext-provider` | `portable.rs` | Provider shaping without native features |

## JavaScript suites

| Command (from `spindle/`) | Exercises |
| --- | --- |
| `node --test store/tests/ui.mjs store/tests/ui_app.mjs` | The web UI against a simulated DOM and API |
| `SPINDLE_BROWSER=/path/to/chromium node --test store/tests/ui_browser.mjs` | The web UI in a real headless browser |
| `SPINDLE_BIN=target/release/spindle node clients/js/smoke_test.mjs` | The Node client against a real `spindle api` |
| `SPINDLE_BIN=target/release/spindle python3 clients/python/smoke_test.py` | The Python client |
| `embed/tests/package.sh WASM_BINDGEN` | Builds the wasm package and runs its Node tests and, with `SPINDLE_BROWSER`, its OPFS browser tests |
| `store/tests/wasi_probe.sh WASI_LIBC_ROOT` | The store compiled for WASI (against an extracted wasi-libc package), run under Node |

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
| `wasm` | Provider and wire crates on `wasm32-unknown-unknown`, the browser-target store, the WASI probe, the embedded package in Node and headless Chrome, the browser Linux build and its Node tests, and Spindle for Browser against a scripted provider |
| `windows` | Native store, CLI and engine tests on Windows |

Two more workflows publish. [`pages.yml`](../.github/workflows/pages.yml) deploys `site/` to
GitHub Pages when it changes. [`browser-release.yml`](../.github/workflows/browser-release.yml),
on a `browser-v*` tag, builds Spindle for Browser, runs `extension_real.mjs --scripted` against
the zip it publishes, and attaches the zip to the tag's release (see
[chrome.md](chrome.md#install-a-release)).

## Dependency policy

`cargo deny --workspace --all-features check licenses` checks the licenses of every
crate in the workspace and all of their dependencies, with every feature on. It allows only
the licenses of the nearest `deny.toml`: `spindle/deny.toml` in `spindle/`, and the root
`deny.toml` in `dext-engine/`, which has none of its own. Both allow the same permissive set:
MIT, Apache-2.0, BSD-3-Clause, BSL-1.0, ISC, Zlib, Unicode-3.0, Unlicense and
CDLA-Permissive-2.0. `cargo audit --deny warnings` checks both lockfiles against the RustSec
advisory database. Lockfiles are committed and every build uses `--locked`.
