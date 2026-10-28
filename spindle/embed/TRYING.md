# Trying Spindle inside an app

Spindle can run inside a JavaScript app (Node, Electron, or a browser worker) with no
separate process: the embedded package. Sessions keep Spindle's rules there: every
model request is sent at most once, a request whose outcome a crash left unknown is
never sent again by itself, and the conversation, files and app state move together.
A session can also move between an app and the `spindle` command, in either direction.

This is experimental software. Use test directories, not real work.

## 1. Build the kit

You need Rust with the `wasm32-unknown-unknown` target, Node 20 or newer, and the
wasm-bindgen CLI at exactly the version the package pins (0.2.129):

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129 --locked
cd spindle
embed/kit.sh                 # or: embed/kit.sh /path/to/wasm-bindgen OUT_DIR
```

The kit (`embed/kit/` by default) holds the package for Node (`node/`) and browsers
(`web/`), the JavaScript clients (`clients/`), an example (`examples/try_node.mjs`) and
this guide.

## 2. Run a session in Node

Pick any provider from your catalog that signs in with an API key (Dext's
`~/.dext/providers.json` is used when present, otherwise Dext's built-in providers).
A ChatGPT sign-in works when the app passes its access token as the key (the package
reads the account from the token); Claude subscription sign-ins are not supported (an
Anthropic key goes in `x-api-key`). The key goes in `SPINDLE_KEY`; it stays in the app's
process and is never stored.

```sh
cd embed/kit
SPINDLE_KEY=... node examples/try_node.mjs --provider deepseek --dir ./s1 \
  "Create a file hello.txt containing: hi from spindle. Then reply with just: DONE"
```

You should see the tool call and the reply stream in, then the session's state and its
files. Run it again with another prompt and the same `--dir` to continue the session.

## 3. Move a session

- **Into a bundle and onward:** add `--export s1.spindle` to a run. The session moves
  into that file and `./s1` is sealed (opening it says where it went). Continue it in
  the `spindle` command built from this repository:

  ```sh
  spindle --dir ./s1-native import-session s1.spindle
  spindle --dir ./s1-native say "Append a second line to hello.txt"
  ```

  Or bring a bundle back into the app: `node examples/try_node.mjs --import FILE --dir
  ./new "PROMPT"`. `spindle --dir DIR export-session FILE` makes a bundle from a native
  session (add `--keep` to leave the original usable too, but then continue only one of
  the two).
- **In place (Node only):** a Node session directory is a native one. Close it in the
  app and run `spindle --dir ./s1 say "..."`, or the other way round. Each refuses the
  directory while the other has it open.

A session started by the `spindle` command usually has settings the app cannot use
(all built-in tools); the app says which and how to change them with `settings` (for
example `builtin_tools: files`).

## 4. In your own app

```js
import { fileURLToPath } from 'node:url';
import { nodeClient } from './clients/embedded-node.mjs';

const spindle = nodeClient({
  glue: fileURLToPath(new URL('./node/spindle_embed.js', import.meta.url)),
  // Only the key: the package says where it goes for each provider, from the catalog.
  credentials: async () => ({ key: KEY }),
});
spindle.on('event', (event) => { /* model_delta, tool_call, tool_result, stopped, ... */ });
await spindle.create('/path/to/session', 'deepseek');
const turn = await spindle.prompt('/path/to/session', 'Hello');
await spindle.close();
```

The method names are the same as the `spindle api` client's (`StdioClient`):
`prompt`, `resume`, `retry`, `stop`, `steer`, `history`, `files`, `read`, `settings`,
`state`, `taskStart`, plus `exportSession` and `importSession`. App tools: register
them with `tool({ name, description, inputSchema, run })`. Types are in
`clients/embedded.d.mts`. The full reference is `docs/API.md` and `docs/embedded.md` in
the repository.

`bash` needs a runner: create the session with `runner: 'app'` and give the client a
`runner` (your own, which Spindle reports as unisolated), or `runner: 'remote'` with
`remoteRunner({ url, token })` pointing at a `spindle serve` host that has a sandbox.
Without one, the model gets file tools only.

## 5. In a browser

For Spindle itself in Chrome, with no relay and no host, use Spindle for Chrome
(`chrome/README.md`): an extension may call providers directly. What follows is for
trying the package inside a web page of your own.

A browser needs two things Node does not: the key must never be in the page, and the
provider would have to allow the page's origin (CORS). The kit's relay solves both: it
serves the page and forwards its model requests, putting the key in, so the browser only
talks to the relay. Nothing about any provider is built into it: it forwards only to the
base URLs of the providers in your catalog (`~/.dext/providers.json`, or the built-ins),
and only requests from its own page.

1. Start the relay (any free port; it prints the page's address):

   ```sh
   cd embed/kit
   SPINDLE_KEY=... node examples/relay.mjs        # --port N to choose one
   ```

2. Open the address it prints after `open`, `http://localhost:PORT/examples/browser/`, in
   Chrome or Edge (the bare `http://localhost:PORT/` takes you there too). From Windows
   this works while the relay runs in WSL.
3. Type a session name such as `/sessions/one`, pick the provider (the list is your
   catalog), write a prompt and press Send. Try Stop while it writes, Status, and Export,
   which downloads a bundle and seals the session in the browser. Then import that file
   into a new session name, or into the `spindle` command (`spindle --dir DIR
   import-session FILE`).
4. Reload the page or restart the browser: the session is still there (OPFS). A second
   tab on the same session is refused while the first has it.

In your own app, run the client in a Dedicated Worker over http(s) with `storage:
'opfs'`, `pool: () => openOpfsPool()` and the `web/` package (call its `init()` first),
and send its requests through your own relay (see `examples/browser/worker.mjs`).

**The automated real-browser test** runs all of this against a real provider in a real
browser, with every step checked and every model request counted at the relay:

```sh
cd spindle
SPINDLE_KEY=... SPINDLE_REAL_BROWSER="/path/to/chrome" SPINDLE_BIN=/path/to/spindle \
  node embed/tests/browser_real.mjs embed/kit --provider ID
```

It runs a real multi-tool task, a stop mid-reply (`resume` must then send nothing and
`retry` exactly one request), a steering message, a browser killed mid-request and
restarted (the same), a second tab refused, and a move browser to `spindle` to browser
with a real turn in each, then checks the key is in no bundle. It costs a few cents.

## Things to try

- Long turns with several tools; `steer` with a message while it works.
- `stop` while it writes a reply. The provider may already have finished (and billed)
  that reply, so its outcome is unknown: `resume` says so and sends nothing, and `retry`
  sends it again, once. That refusal is Spindle's rule (a request is sent at most once),
  not a fault. A `stop` during a tool call ends the call instead, and `resume` goes on.
- Kill the app in the middle of a reply, then `resume` (it must not send the lost request
  again: it says the outcome is unknown) and `retry` (sends it once). A killed app
  leaves its lock file, on purpose: nothing can tell a dead app from a live one, so the
  session stays refused until you delete `store/authority.sqlite.lock` in its directory
  (only once that app is really gone).
- Moving sessions back and forth between the app and the `spindle` command.

Sessions hold your prompts and files: share a session directory or bundle only if that
is fine.

## Limits

- Not available inside an app: background tasks, `max_usd` budgets, `usage`/`doctor`,
  session packs, `run_code`, background jobs.
- Untested: Electron, Safari, Firefox, macOS.
- The package is about 5 MB of wasm (1.8 MB compressed).
- This `spindle` upgrades older sessions it opens (schema 14-18 to 19): do not point it
  at sessions an older installed Spindle still uses.
