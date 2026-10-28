# Spindle workspace

The Rust workspace for Spindle, plus its JavaScript and Python clients and the Chrome
extension.

| Member | Contents |
| --- | --- |
| `spindle-reference` (`src/`, `tests/`) | In-memory reference model of the store's authority rules |
| `spindle-store` ([`store/`](store)) | Store, decision driver, native session, API, CLI and browser host |
| `spindle-embed` ([`embed/`](embed)) | WebAssembly package with Node and browser storage |
| [`clients/`](clients) | Node.js and Python clients (no dependencies) |
| [`chrome/`](chrome) | Spindle for Chrome and its browser Linuxes |

The engine crates are in [`../dext-engine`](../dext-engine) and are referenced by path;
keep the two directories side by side.

```sh
cargo build --release --locked -p spindle-store --features cli --bin spindle
cargo test --locked --workspace --all-features
```

Documentation lives in [`../docs`](../docs/README.md): start with
[getting started](../docs/getting-started.md) and [architecture](../docs/architecture.md).
