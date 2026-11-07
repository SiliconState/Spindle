# Spindle workspace

The Rust workspace for Spindle, plus its JavaScript and Python clients and Spindle for
Browser (Chrome, Edge and Brave).

| Member | Contents |
| --- | --- |
| `spindle-reference` (`src/`, `tests/`) | In-memory reference model of the store's authority rules |
| `spindle-store` ([`store/`](store)) | Store, decision driver, native session, API, CLI and web UI host |
| `spindle-embed` ([`embed/`](embed)) | WebAssembly package with Node and browser storage |
| [`clients/`](clients) | Node.js and Python API clients and the fetch-only `remote.mjs` (no dependencies), and the JavaScript side of `spindle-embed` (`embedded.mjs`, `opfs.mjs`) |
| [`chrome/`](chrome) | Spindle for Browser and its browser Linuxes |

The engine crates are in [`../dext-engine`](../dext-engine) and are referenced by path;
keep the two directories side by side.

```sh
cargo build --release --locked -p spindle-store --features cli --bin spindle
cargo test --locked --workspace --all-features
```

The store reads schemas 20 and 21: it starts at 20 and widens atomically to 21 for
more than eight intents in a step (up to 256). Schema 19 migrates in place; a widened
store is not readable by an older binary. Native and embedded hosts share the
[`spend` ledger](store/src/spend.rs), dollar budgets and queued task reports.
The embedded client also offers checked store compaction and bundle validation;
see [the package reference](../docs/embedded.md). The browser extension is version
0.1.7 ([manifest](chrome/manifest.json)); its UI, preview choices and storage controls
are documented in [Spindle for Browser](../docs/chrome.md).

Documentation lives in [`../docs`](../docs/README.md): start with
[getting started](../docs/getting-started.md) and [architecture](../docs/architecture.md).
