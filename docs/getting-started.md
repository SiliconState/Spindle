# Getting started

This guide builds the `spindle` binary, signs in to a model provider, runs a first
session from the terminal, and starts the browser host. For the embedded JavaScript
package see [embedded.md](embedded.md); for the Chrome extension see [chrome.md](chrome.md).

## Requirements

| Need | Why |
| --- | --- |
| Rust 1.89 or newer (`spindle-store` declares `rust-version`), Cargo | Builds every crate |
| A C compiler | Builds the bundled SQLite |
| Linux with `bubblewrap` and unprivileged user namespaces | Sandboxed commands (`--isolation required`). macOS and Windows run commands with weaker host-runner limits; see [tools.md](tools.md). |
| Node.js 20 or newer (optional) | The JavaScript clients, the embedded package and UI tests |
| Python 3 (optional) | The Python client |

Keep the repository layout as it is: `spindle/` builds against the engine crates in the
sibling `dext-engine/` directory through Cargo path dependencies.

## Build

```sh
git clone https://github.com/SiliconState/Spindle.git
cd Spindle/spindle
cargo build --release --locked -p spindle-store --features cli --bin spindle
./target/release/spindle version
```

`version` prints the build's version and its store schema. The examples below assume
the binary is on your `PATH` as `spindle`.

## Sign in to a provider

Providers come from the Dext provider catalog; sign-ins are stored in Dext's state
directory (`~/.dext`, or `$DEXT_HOME`), never in a session.

```sh
spindle providers                 # every provider, its default model and sign-in state
spindle login deepseek            # paste the API key at the prompt
spindle login PROVIDER web        # browser sign-in, for providers that support it
spindle logout PROVIDER
```

Prefer the prompt (or piping the key on stdin) over passing a key as an argument, which
shell history may keep. A provider's environment variable (for example
`DEEPSEEK_API_KEY` or `OPENAI_API_KEY`; `spindle providers` names it when a credential is
missing) also works.

Custom or local OpenAI-compatible endpoints are added to `~/.dext/providers.json`; see
[engine.md](engine.md#provider-catalog).

## A first session

A session lives in a directory: `--dir DIR`, or `$SPINDLE_DIR`, or `./.spindle` by default.

```sh
spindle --dir ./calc new --provider deepseek --isolation required \
  "Create a small command-line calculator in Python with tests"

spindle --dir ./calc status       # where the session stands
spindle --dir ./calc log          # the conversation so far
spindle --dir ./calc files        # the versioned workspace
spindle --dir ./calc cat calc.py  # one file at the head revision
spindle --dir ./calc say "Add a power operator"
```

Useful options on `new` and `say`:

| Option | Effect |
| --- | --- |
| `--model M` | Model id (default: the provider's default) |
| `--isolation auto\|required\|off` | Sandbox policy for commands |
| `--network` / `--no-network` | Internet access for commands |
| `--toolchain` | The host Rust toolchain, read-only, with a build cache beside the session |
| `--source DIR` | With `--toolchain`, mount a host directory read-only at `/NAME` (for example a path dependency) |
| `--effort LEVEL` | Reasoning effort: `default`, `off`, `low`, `medium`, `high`, `xhigh`, `max` |
| `--compaction model\|digest` | How older history is shrunk when it outgrows the context window |
| `--max-usd USD\|none` | Dollar budget for the thread |
| `--attach FILE[=PATH]` | Put a file into the workspace with the prompt |

The full command reference is [cli.md](cli.md).

## When a run is interrupted

Kill the process mid-turn and nothing is lost: every step up to the interruption is
committed.

```sh
spindle --dir ./calc continue     # resume; never resends a request with an unknown outcome
spindle --dir ./calc retry        # explicitly re-run an interrupted model call, once
spindle --dir ./calc doctor       # check the session and safely repair what it can
```

If `continue` reports an **unknown outcome**, the provider may or may not have received
(and billed) the request; `retry` sends it again. See
[concepts.md](concepts.md#unknown-outcomes).

## Rewind and export

```sh
spindle --dir ./calc targets              # revisions you can go back to
spindle --dir ./calc rewind 4             # conversation, files and app state together
spindle --dir ./calc export ./out         # write the workspace to a directory
spindle --dir ./calc export-session calc.spindle      # move the session into a bundle
spindle --dir ./elsewhere import-session calc.spindle # continue it somewhere else
spindle --dir ./calc usage                # spend per turn and per model
```

## The browser host

`spindle serve` runs a mobile-first web UI and the HTTP `/v1` API on loopback. The
directory it is given is a *host directory* that holds many sessions.

```sh
spindle --dir ./host serve --port 8790
# Spindle listening on http://127.0.0.1:8790; pairing key in ./host/pairing.key
```

Open the address, paste the pairing key, and the browser receives a revocable device
token in an HTTP-only cookie. For programs, `spindle --dir ./host token new NAME` mints a
bearer token for `/v1`.

The listener binds loopback only. To reach it from another device, put it behind an
authenticated HTTPS reverse proxy and pass `--origin https://HOST`; with Cloudflare
Access, also pass `--access-team` and `--access-aud` so the host verifies the Access
token itself. Never expose the loopback port directly. See [host.md](host.md).

## Driving Spindle from a program

| Option | Use when |
| --- | --- |
| `spindle api` (JSON-RPC 2.0 over stdio) with the [Node or Python client](clients.md) | A local app that can spawn a process |
| HTTP `/v1` on `spindle serve` | A remote client, or several clients sharing a host |
| The [embedded package](embedded.md) | A JavaScript app that should own storage and I/O itself, including in a browser |
| The Rust `agent` module | A Rust program linking `spindle-store` with the `agent` feature |

All of them speak the same API, version 1: [API.md](API.md).

## Next steps

- [Concepts](concepts.md): revisions, effects, unknown outcomes, rewind.
- [Architecture](architecture.md): how the pieces fit together.
- [Security](../SECURITY.md): what isolation does and does not cover.
