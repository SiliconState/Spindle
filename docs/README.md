# Spindle documentation

Everything here is written from the source in this repository; where a document and the
code disagree, the code and its tests are right. Source links point at the files that
implement what each page describes.

## Start here

| Document | What it covers |
| --- | --- |
| [Getting started](getting-started.md) | Build, sign in to a provider, run and resume a session, start the browser host |
| [Concepts](concepts.md) | Sessions, revisions, effects, unknown outcomes, rewind, tools, tasks, bundles |
| [Architecture](architecture.md) | System context, layers, crates, a turn end to end, hosts, deployment shapes |
| [Security](../SECURITY.md) | Trust model, credentials, command isolation, recovery semantics, the browser host |

## Reference

| Document | What it covers |
| --- | --- |
| [CLI](cli.md) | Every `spindle` command and flag |
| [API](API.md) | The versioned public API: JSON-RPC methods, HTTP `/v1` routes, events, data types, errors |
| [Clients](clients.md) | The Node.js and Python clients |
| [Testing](testing.md) | Test suites, how to run them, CI and dependency policy |

## Components

| Document | Component | Source |
| --- | --- | --- |
| [Store](store.md) | SQLite authority, revisions, objects and workspace trees, rewind, ownership, bundles, migrations | [`spindle/store/src`](../spindle/store/src), [`spindle/src`](../spindle/src) |
| [Driver and session](driver.md) | The decision driver, the native `Session`, model requests, recovery, steering, budgets, compaction | [`driver.rs`](../spindle/store/src/driver.rs), [`agent.rs`](../spindle/store/src/agent.rs) |
| [Tools and execution](tools.md) | Built-in tools, sandboxed commands, background jobs, images, app tools, tasks, code mode | [`agent/`](../spindle/store/src/agent), [`code_mode.rs`](../spindle/store/src/code_mode.rs) |
| [Browser host](host.md) | `spindle serve`: listener, pairing, remote access, previews, self-edit, web UI | [`serve/`](../spindle/store/src/serve), [`ui/`](../spindle/store/ui) |
| [Embedded package](embedded.md) | Spindle inside a JS app via WebAssembly; Node and OPFS storage; moving sessions | [`embed/`](../spindle/embed), [`embedded/`](../spindle/store/src/embedded) |
| [Chrome extension](chrome.md) | Spindle for Chrome: contexts, threads, keys and sign-in, page tools, build and tests | [`chrome/`](../spindle/chrome) |
| [Browser Linux](browser-linux.md) | The v86 and WebAssembly Linuxes that run commands in the browser | [`chrome/linux`](../spindle/chrome/linux), [`chrome/wasm-linux`](../spindle/chrome/wasm-linux) |
| [Engine crates](engine.md) | `dext-core`, `dext-transcript`, `dext-native`, `dext-provider`; provider catalog and wire formats | [`dext-engine/`](../dext-engine) |

## Reading paths

- **Using Spindle from a terminal or phone:** getting started, concepts, CLI, browser host.
- **Building an app on Spindle:** concepts, API, clients or embedded package, tools (app tools).
- **Understanding the durability model:** concepts, architecture, store, driver.
- **Working on Spindle itself:** architecture, store, driver, tools, engine, testing.
