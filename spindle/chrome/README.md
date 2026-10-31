# Spindle for Browser

Spindle running inside your browser, tested on Chrome and Edge: a side-panel extension
(Manifest V3; Chrome's names below cover Edge's too) whose threads are sessions of
Spindle's embedded package (`embed/`), kept in the browser's private storage (OPFS).
No server and no relay: the extension calls your provider directly with your key.

- **Threads keep going while the panel is closed.** They run in an engine Chrome keeps
  open in the background (an offscreen document's worker), not in the panel.
- **Your key stays in this browser,** sealed (AES-GCM) in the extension's own storage, and
  goes only to its provider. Add a passphrase in Settings, Providers and a copy of your
  Chrome profile made from then on cannot read your keys; you unlock them once each time
  Spindle's engine starts. The extension may reach a provider's site once you save its key (Chrome asks).
  No provider is written into the extension: they come from the catalog, Spindle's
  built-ins or your own `~/.dext/providers.json`.
- **Sign in instead of a key** where the provider has a browser sign-in: **Sign in** for
  ChatGPT opens OpenAI's sign-in page in a tab, Spindle catches where it sends you back,
  and the engine keeps (and refreshes) the tokens, sealed like keys.
- **bash, in a Linux inside Chrome:** threads run commands in Alpine Linux (bash,
  coreutils, Python 3 with pip, Node with npm, git, curl, jq, ripgrep) emulated by v86 in
  the extension, sealed off from your computer and from Spindle's keys. Commands run as
  an unprivileged user, each in namespaces of its own: a fresh /tmp and home each time
  (only the workspace is kept), its own processes (whatever a command leaves running
  ends with it) and, for a thread without network, a network of its own with nothing
  in it. Several threads' commands run side by side (one per thread at a time, and one
  with network at a time), so a script waiting on its Spindle calls holds up no other
  thread. A thread's workspace stays in the Linux between its commands: only files
  changed since its last command go in, and only files a command changed come back.
  The Linux keeps the browser's clock. The first command ever boots it (about 20 seconds
  in our measurements, once); after that it resumes in under a second, and rests (giving
  its memory back) after ten idle minutes. Opening a thread that ran commands there in
  the last day starts it, so its next command rarely waits; a thread that only talks
  or reads pages leaves it, and its 512 MiB, unstarted until a command needs it.
  Choosing "This browser" for a thread's commands, or Start in Settings, starts it too.
- **Memory given back:** wasm memory only grows, so a long thread or a large export
  leaves the engine at its peak. Once every thread has rested half a minute, an engine
  that grew past 64 MiB starts a fresh copy of itself (sessions stay on disk; keys,
  the Linuxes and a connected host carry over) and the old copy's memory goes back to
  the browser.
- **A second, faster Linux for plain commands (when built in):** Spindle's wasm Linux is
  Linux itself compiled to WebAssembly, with busybox `sh` and Python 3.13, run at the
  machine's own speed with no emulator: it boots in about half a second and runs plain
  shell commands 2 to 9 times faster than the emulated Linux, Python 3 to 25 times. No
  switch: a command that is plain POSIX sh with programs it has runs there (each in
  namespaces of its own, on CPUs of its own, as the same unprivileged user), anything
  else runs in the emulated Linux, which also runs again a command the wasm Linux turns
  out to lack something for (unless it made a Spindle call). Commands of threads with
  network always run in the emulated Linux.
- **Code mode through bash:** scripts in that Linux can ask Spindle for calls, each
  journaled as native `run_code` journals them (a call cut off is never sent twice): from
  bash `spindle model PROMPT` (a model's answer), `spindle tool NAME JSON` (the page
  tools, an app's tools), `spindle search`, `spindle http URL`; in Python `import spindle`
  (`spindle.model(prompt)['text']`, `spindle.models([...])`, `spindle.tool(...)`,
  `spindle.http(url)`, `spindle.batch(...)`); in Node `require('spindle')`, or `import
  ... from 'spindle'` in `.mjs` and TypeScript (`node file.ts` runs it, types stripped).
  The model is told so, and does in one script what would take many tool calls. The
  `spindle` command is a small Rust program (`linux/command`), so a call from bash or
  Node costs the round trip, not a Python start in the emulator (most of a second). Models
  run on your key, as turns do; each command may make up to 128 calls (32 of them
  models). Calls from several processes wait their turn; `batch` and `models` run up to
  four together.
- **A thread's network** (off until you turn it on in the thread's model page, Network
  for commands): its commands reach websites through Chrome, with no server between.
  The Linux's network card ends inside the extension: a small proxy in the Linux
  (`http_proxy` and `https_proxy` are set for the command) opens HTTPS with a
  certificate from the Linux's own authority, made at its first start (its key never
  leaves it; commands trust it), and Chrome makes each real request with its own TLS
  and without your cookies. curl, git, pip, npm, Python and Node's own `fetch` work.
  Your computer and its local network are refused by address and name (localhost,
  names without a dot, `.local`, `.lan`, `.home.arpa`, private and link-local ranges; a
  public name that points into your network is not caught, since Chrome resolves
  names). Redirects are not followed: Chrome can hide their target, so the gateway
  refuses those rather than contacting an unchecked address. Ask for the final public
  address directly (or use your own relay). A command without network reaches nothing. Chrome asks once for access to sites, the same access page
  reading uses. The model button in the box shows a globe for such a thread.
- **Or a relay you run** (opt-in, Settings, Relay): a WISP server address
  (`wisps://...`, such as wisp-js or epoxy-server) gives threads with network the whole
  internet through it, any protocol (ssh, git over SSH, databases), with the Linux's own
  TLS; names are then looked up over DNS-over-HTTPS (cloudflare-dns.com), only while a
  command with network runs. Spindle runs no relay of its own.
- **Sites scripts may call:** whatever a thread's network, a script can call a site you
  list in Settings, Sites scripts may call (an address prefix such as `https://api.example.com/v1/`, to
  read, or read and change). Chrome asks once for each. Requests are journaled like
  every Spindle call, carry no cookies, follow no redirect, and anything outside a listed
  prefix (or a method it does not allow) is refused, naming what is allowed.
- **Or your Spindle host's sandbox:** connect your host in Settings, Your Spindle host (its
  address and a runner token from `spindle --dir HOST token new chrome --runner`, which
  runs commands and reaches nothing else there), and choose it for
  a thread in the model page (Commands run in: Host).
- **Page reading** (a switch in Settings, Pages; off until you turn it on, when Chrome
  asks for access to sites): threads can list your
  tabs and read the page you are looking at with `list_tabs` and `read_page`, see its
  parts (`page_elements`: headings, links, buttons, fields, tables, each part to act on
  with a ref such as `kqm12`, which names a part of that page only) and its HTML
  (`page_html`, scripts and passwords left out). With page actions on, `read_page` with
  `scroll` reads a feed that loads as it scrolls in one call (down that many screens,
  each new line once). `screenshot` pictures the tab showing once you have let Spindle picture
  pages (saved in the workspace, shown in the panel; a script can show it to a vision
  model with `spindle model --image FILE`).
- **Screenshots of your own:** Capture this tab (the camera under the + in the message
  box) pictures the tab you are on and attaches it to your message (it shows with the message; the thread finds
  it in its workspace). The first time, Chrome asks to let Spindle picture pages
  (`<all_urls>`, which Chrome requires for a picture of a tab; it covers every site, so
  Chrome then counts site access as given too, while the Pages switches stay as they
  are). Settings, Pages takes it back with site access. A message sent while the
  thread's turn runs steers that turn, but a running turn takes text only: a message
  with a screenshot or other files waits in the message box (a line above what you
  type) and goes as the next turn the moment this one ends (Take back puts it in the box
  again). It is kept there, saying
  why, when the thread cannot take it then: an unknown outcome needs you first, or the
  turn did not finish (you stopped it, say) and waits for Retry, Continue or Take back.
- **Page actions** (a second switch, off until you turn it on; Chrome's debugger, which
  Chrome does not offer as an optional permission, so the extension holds it and the
  switch decides): threads browse as
  you would, in your browser, signed in as you: `navigate` (an address, a new tab, back,
  forward, reload), `click`, `type`, `select`, `press` (Enter, Escape, Control+a...),
  `scroll` (the page, or the pane under the middle of the window), `wait_for` (text or
  a selector), `dialog` (answer an alert, confirm or prompt), `close_tab`, `screenshot` of
  any tab (through the debugger) and `network` (record a tab's requests, list them, read
  a response). Clicks, keys and typing go through Chrome's debugger, so pages see them
  as real input; Chrome shows that Spindle is debugging a tab while it acts on it.
- **Files in and out:** attach files to a message (the + beside the box, or drop them on
  the panel) and they land in the thread's workspace. What a thread makes shows in the
  conversation as a card under the step that wrote it: a web page shows in a preview
  (its styles and pictures come from the workspace; it is a sandboxed page that cannot
  reach Spindle or your keys, and reaches nothing outside the workspace: no fetch,
  picture or script from the web, no form sent). Its scripts run only once you press Run
  its scripts on it, kept for that version of it: the page, its own scripts and the
  small data files it or they name, which are the files it may then fetch from the
  workspace (any of them changing asks again). Load from the web lets it run and load
  over HTTPS. Markdown is drawn, pictures show,
  text and code show as they are; each card has icon buttons (show here, open in the
  workspace, Download; their words in their tooltips). The workspace is listed once, in a
  card folded until you open it, just above the last turn's final answer: as a tree whose
  folders stay shut until you open one (a folder holding only a folder shares its row,
  `lib/python3.14/site-packages`; each says how many files it holds and how many changed),
  the files that turn made or changed (by file tools or commands) marked new or edited,
  scratch files (`.txt`, `.py`, `tests/`) out of sight until you show them, with
  Download all (.zip); it has a page of its own in the panel (Workspace in the thread's
  menu, or a card's workspace button): its files to filter and open, one at a time with
  its preview, the files before and after it a click (or an arrow key) away, its path to
  copy, Download, and for a page or picture Open in a tab of its own. With
  page tools on, a thread can open a file for you itself: `navigate` to
  `/workspace/NAME`. Downloads go through Chrome's downloads API (the `downloads`
  permission), which saves them even when the browser would hold back a download a page
  starts after waiting on something; a name Chrome refuses (`.env`, `con.txt`) is saved
  as `_env`, `_con.txt`.
- **A long run, readable:** a turn's steps (its tool calls) gather into a group whose
  one-line head says how many there were (Ran 31 steps), how many were done again and
  how many failed (as icons with numbers), and how long they took on the clock (steps
  that ran at once, such as a batch of tasks, count once); the running turn's group stays
  open (Step 3 of 3, its running step spinning with its time), and every group shuts when
  the turn ends. Each step is a row with an icon for its kind (a command, Python, a file,
  a folder, a task, tabs) saying what it does in plain words, in its tense (Running
  build_report.py, Ran build_report.py); a page step shows the site's icon and says what
  it did without the address (Read HTML, Typed into “Search”; the address shows on
  hover). The same step done again and again in a row shares one row (Read HTML ×3, the
  times added; opened, each shows). A long group shows its first rows and "N more
  steps" (a running one its last rows). Opened, a step shows the picture it took, its
  input and its output a line each, and copy, view (all of both) and download. A failed
  step shows a red ✕, and an amber ↻ once a later step did it again. A message sent
  while a turn runs is a bubble of its own at the right (three lines of a long one,
  with an arrow for the rest), checked once the running turn has read it; scrolled up
  while it runs, Latest takes you back down; while a turn runs (and the box is empty)
  Send is Stop, a red square. An answer has a copy button; a table's short cells stay
  on one line (a wide table scrolls). Chrome's site icons come through its `favicon`
  permission.
- **The panel:** the thread list groups threads into Running, Needs you (a turn that
  stopped part way, waiting for Continue, Retry or Take back), Today and Earlier, each
  row showing its state as a shape as well as a colour (a spinner, a warning, a dot for
  a reply you have not read here) and what it is doing; a search filters them. A
  thread's header has its name (its menu) and model, what it spent (opening its limit)
  and Settings, with a thin bar beneath for what it spent against its limit. The message
  box's top row says how the thread stands (Working and for how long, its tasks at work,
  a report waiting) and holds the model button (model · effort); its + opens Upload and
  Capture this tab. Settings and the model are pages of the panel, each with a back
  button.
- **Your instructions:** what every thread should keep to (Settings, Your instructions):
  new threads start with them, others take them before their next message.
  While it is off they read nothing, not even a provider's site Spindle may reach.
- **The model page** (the model · effort button in the box): whether a change is for
  this thread or for new threads; providers and models in lists you can filter (a model
  the list does not have can be typed), each model with its context, pictures and price
  from the catalog; the effort, from the levels that model takes, each said as what it
  sends (none chosen leaves the model's own default, which the page names: DeepSeek's
  thinking off, a Claude model's adaptive thinking); where commands run (a choice that
  cannot be made says why, with a link to set it up); and their network; whether it may
  start tasks; and a spending limit in US dollars (for new threads too), with what the
  thread spent so far (the same ledger a Spindle host keeps: a request that could pass
  the limit is not sent; raise it and Continue). Back leaves without saving.
- **Tasks** (off unless switched on in the model page): a thread can start tasks,
  threads of its own that work beside it in their own workspace and report back. Each
  shows as a one-line card below the step that started it: working (its steps and time,
  Stop, what it does now on hover), done, stopped or failed; opened, what it was asked
  and its answer. The box's top row counts the tasks at work (to the first's card), and
  says when a report waits for the thread's next turn (Continue reads it in). A turn
  waits for its tasks before it ends ("Waiting for its task"); Stop then stops the
  waiting, not the tasks. A task's report
  shows as the task's, folded; one that stops (or could not go on, saying why) tells the
  thread so, and the thread goes on.
  Their spend counts against the thread's limit.
- **Durable:** a turn cut off by closing Chrome, a crash or Stop says so; Retry sends the
  cut request again (Continue goes on when Spindle knows where it stopped). A turn that
  stopped before it finished (a provider refusing the key, say) can also be started over
  with another model or effort (the model page: the turn is taken back and its message
  sent again, its attachments too), or taken back (its message returns to the box).
- **Going back:** the back arrow beside any of your messages takes the thread back to just before it,
  conversation and workspace together, and returns the message (and its attachments)
  to the box to change or send again. What came after stays in the thread's record. (A
  thread started with another thread's files keeps them: its first turn can be started
  over, but not taken back alone.)
- **Long threads:** when a thread's history outgrows what its model takes, its earlier
  part is folded into a summary the thread's own model writes (one request, on your key;
  if it fails, a shorter digest Spindle makes stands in), and the thread goes on from
  it. The conversation says so where it happened, shows the summary, and shows the
  earlier messages themselves when asked. A long thread opens on its last 30 turns (the
  rest on a click), and a turn's end draws only that turn again. To start clean, the
  thread menu's **New thread with these files** starts a thread with this one's
  workspace and none of its conversation (its model too, to change in the model page).
- **Moving:** a thread moves into a `.spindle` file (Thread menu, Move to a file) and
  continues in native Spindle (`spindle --dir DIR import-session FILE`), or in Spindle
  in another browser (Import a .spindle thread). Native Spindle's
  `export-session FILE` moves one back. The moved copy keeps the file until you delete
  it (Save the file again), so a save you cancelled loses nothing; once the file is on
  disk (a strict IndexedDB commit) and reads back as written, it is the thread's one copy
  here (the store it left goes, all but the note of where it went). Export a copy leaves
  the thread here. A thread leaves without the page tools (a native host could not
  answer them) and, where it had the file tools, with native's full tool set (code
  mode's `run_code` too) run in the native host's own sandbox; the thread in Chrome
  keeps its own settings. Imported, a thread gets the page tools back, and one from a
  native host changes what Chrome cannot do (its commands move to the browser Linux,
  keeping the network they had; the Rust toolchain and host folders go, a dollar budget
  stays), as Spindle's `fit` gives it: the panel lists those changes and keeps the thread
  only if you say so, before anything runs.
- **Deleting:** Delete in a thread's ⋯ menu in the list (or Delete this thread in its
  menu) removes it and its workspace from this browser after a moment to Undo it
  (closing the panel deletes it then); stop a running thread first.
- **Storage:** Settings, Storage lists what Spindle keeps in this browser, largest
  first: each thread (with the free space inside its store that can be given back), a
  moved thread's file, the Linux snapshot v86 resumes from, and everything else (settings,
  keys, the browser's own bookkeeping). **Free** compacts a thread's store and its tasks'
  (a copy written beside it with VACUUM INTO, synced, checked whole, then renamed over
  it: a crash leaves the store or the copy whole), while no thread or background task is
  at work (it holds the engine for its seconds); **Free all** does every thread that has
  space to give back. A thread whose first request was
  tried again keeps the first attempt aside (its unknown request on record); **Remove
  first attempts** removes them (what they may have cost stays counted). A moved thread
  that still has its sealed store (one moved by an earlier version) frees it once its
  file checks whole. **Remove** takes the Linux snapshot away: the next command for v86
  boots it afresh (under a minute).
- **Appearance:** the native UI's Paper, Light, Dark or System (Light or Dark as the
  system is) theme, with a Textured or Plain
  background.

## Build and install

From the `spindle` directory, with the pinned wasm-bindgen CLI (see `embed/`). The
Linux is built first (curl, npm, python3 and user namespaces; no root, no Docker;
downloads are pinned by version and checksum, and Alpine packages are the branch's
current ones, checked against its signing keys):

```sh
chrome/linux/build.sh                                 # into chrome/linux/dist
chrome/build.sh /path/to/wasm-bindgen                 # into chrome/dist
chrome/build.sh /path/to/wasm-bindgen /mnt/c/Users/YOU/spindle-chrome   # a Windows folder, from WSL
```

The wasm Linux is built apart (a toolchain from source first: about half an hour, once;
see `chrome/wasm-linux/`, each script's header) and carried with `SPINDLE_WASM_LINUX`:

```sh
W=/path/to/wasm-linux-build     # any directory outside the repository
chrome/wasm-linux/build-kernel.sh $W/kernel
chrome/wasm-linux/build-toolchain.sh $W/toolchain
chrome/wasm-linux/build-userland.sh $W/toolchain $W/userland
chrome/wasm-linux/build-rust.sh $W/toolchain $W/rust
chrome/wasm-linux/build-python.sh $W/toolchain $W/python
chrome/wasm-linux/build-image.sh $W/kernel/vmlinux.wasm $W/userland $W/rust $W/python   # into chrome/wasm-linux/dist
SPINDLE_WASM_LINUX=chrome/wasm-linux/dist chrome/build.sh /path/to/wasm-bindgen
```

Then in Chrome:

1. Open `chrome://extensions`, turn on **Developer mode** (top right).
2. **Load unpacked**, and pick the folder you built.
3. Pin Spindle (the puzzle-piece menu) and click its icon: the panel opens on the side.
   (Settings, Open in a tab, opens the same Spindle in a tab; if Chrome cannot show a
   permission prompt in the side panel, Spindle offers it.)
4. **Settings** (⚙) → your provider → **Add key** → paste → **Save** (or **Sign in**, for
   ChatGPT). Chrome asks to let Spindle reach that provider's site; allow it. To use your Dext providers, **Import
   providers.json** first.
5. Write in the box at the bottom and press Enter.

Rebuilding into the same folder and pressing the extension's reload button on
`chrome://extensions` keeps your threads (they belong to the extension's ID, which comes
from the folder's path).

## How it is put together

| Part | Runs in | Does |
| --- | --- | --- |
| `panel.html`, `panel.mjs`, `panel.css` | the side panel (one per window) | the UI; asks the engine, draws its events; the workspace page |
| `steps.mjs` | the panel | steps in plain words (their tense, the page and part a browser step acted on), retried failures, durations, a group's time on the clock, scratch files |
| `engine.html`, `engine.mjs` | an offscreen document (one per profile) | keeps the engine worker alive; carries page tool calls |
| `engine-worker.mjs` | that document's Dedicated Worker | every thread: `EmbeddedClient` over the OPFS pool (a session stays open only while something uses it), credentials from the vault (`vault.mjs`), the thread index |
| `vault.mjs` | the engine worker | credentials at rest: sealed with AES-GCM, optionally behind a passphrase |
| `signin.mjs` | the panel (the sign-in tab), the engine worker (the tokens) | browser sign-in: PKCE, the redirect caught in its tab, code exchange and refreshes |
| `linux-runner.mjs` | the engine worker | the runner for `bash`: starts and rests the Linux, hands it each command and its files |
| `linux-worker.mjs` | a worker of its own | v86 and the Linux: boot once and save, resume, run a few commands at once (one per thread, one with network) and report their changes |
| `linux-workspace.mjs` | that worker | threads' workspaces on the Linux's share: each command's copy made of what is still there from the thread's last one (files left as they were keep their inode), the rest asked of the engine; what a command changed, told by what it wrote to |
| `linux/` | (data) | the built Linux: v86, BIOS, the 9p index and files (chrome/linux/build.sh); `guest/agent.py` is its runner agent, `command/` its `spindle` command and `spindle-sandbox` (namespaces for each command) |
| `linux-route.mjs` | the engine worker | which Linux runs a command, and when the wasm Linux's result means running it again on v86 |
| `wasm-linux-worker.mjs` | a worker of its own | the wasm Linux, with linux-worker.mjs's messages: its image, a few commands at once |
| `wasm-linux/` | that worker, and workers it starts | the wasm Linux's runtime (`machine.mjs` boots the kernel and starts a worker per task, `runner.mjs` runs commands and restarts a machine only to end a runaway, the virtio devices, v86's 9p server vendored) and, built in, its kernel and image; `guest/` is its agent and sandbox (Rust) |
| `background.js` | the service worker | opens the panel from the toolbar; `list_tabs`, `read_page` |
| `shared.mjs` | panel and engine | the channel (a `BroadcastChannel`) and the extension's IndexedDB |
| `page-tools.mjs` | panel, engine, background | the page tools' definitions and the access they need |

A redirect whose target Chrome hides is refused before it is followed. The gateway
bounds queued request data and the HTTPS origin preamble as well as individual bodies.

Short engine operations are ordered per thread (Stop bypasses that queue); global
settings changes are ordered too, and changing catalogs refuses while work is pending.
Closing an OPFS session waits for Chrome to finish releasing its Web Lock before the
same client reopens it. A first-turn reset validates settings and saves the original
session as a private bundle before replacing it: a failed replacement or interrupted
reset restores it (the record is written durably, a strict IndexedDB commit, before the
store is replaced). Once the reset is done the saved record goes (the engine's start
also drops any no reset is using). A move sealed before its index update is recognized
on engine restart, and a file a move wrote before the engine stopped short of sealing the
thread goes then (the thread stayed here, whole). Stop, a page-tool
deadline, and disabling page actions fence further browser steps; a request already
dispatched to Chrome cannot be undone. An action cut off once its input reached the page
(or Chrome was told to move or close a tab) answers unknown rather than failed, so the
model checks the page before repeating it and a script makes no further calls.

The UI's look, its Markdown and text helpers (`store/ui/text.js`) and its theme switch
(`store/ui/theme.js`) are the native UI's, copied in at build time.

## Tests

`chrome/tests/extension_real.mjs` loads the built extension into a fresh Chrome profile
(over the DevTools protocol; branded Chrome no longer loads extensions from the command
line) and drives the panel with real clicks and typing: bash in the browser Linux (and
what the model wrote before the call staying on screen while the turn goes on), a
script there making Spindle calls (a nested model and a page tool), a site added in
Settings reached by a script (and a change it does not allow refused), TypeScript, and,
with `SPINDLE_BIN`, bash in a Spindle host's sandbox; `read_page` refused while page
reading is off and reading the page once it is on, steering, a turn finishing while no
panel is open, Stop / Continue / Retry, Chrome killed mid-turn and restarted, a move to
native Spindle (without the page tools, with native's full tool set, a native turn and
budget) and back (page tools again, the budget kept and said), saving a moved thread's
file again, a copy,
attaching files and downloading them (one, and all as a .zip), a page of the workspace
opened in a tab running its script and data, browsing (page_elements, type, click,
screenshot, page_html, network, navigate) under standing instructions, a long thread
(scripted: a turn's end drawing only that turn, going back to before a message with its
workspace, compaction with a summary its model writes and the earlier messages shown, a
new thread with its files), Restart engine, delete (from the thread menu and the list's
×) freeing its storage, and no key in any bundle. `chrome/tests/linux_node.mjs` runs the
browser Linux itself in Node: commands side by side in their own namespaces, a thread's
workspace kept between its commands (and nothing stale when a result is not taken), the
network, the code-mode bridge. The wasm Linux has its own in Node:
`wasm_linux_runner.mjs` (the runner: commands, workspaces, side by side, Spindle calls,
timeouts, Stop, runaways), `wasm_linux_python.mjs` (Python from the built image),
`wasm_linux_rust.mjs`, `wasm_linux_shares.mjs`, `wasm_linux_busybox.mjs`,
`wasm_linux_userland.mjs`, `wasm_linux_node.mjs` (each says its arguments), and
`linux_route.mjs`; `linux_bench.mjs DIST` times the same commands on both Linuxes in
Chrome. `panel_long_run.mjs DIST [SHOTS]` drives one long turn (34 steps, failures, a
retry, a message sent mid-run, a page written) through the panel in each theme (Paper,
Light, Dark, Auto light and dark) and checks how it shows: groups, labels, Stop, Latest,
the workspace page, labels' contrast; `panel_steps.mjs` checks the step labels.
`signin_vault.mjs DIST` checks browser sign-in and the key vault against scripted OAuth and
model services (no account): the tab and its redirect, the code exchange, a refresh, the
account header, sealed records, a passphrase locking and unlocking, and that Settings fits
a side panel.

`review_regressions.mjs` checks engine ordering, reset rollback, early Stop, page-step
cancellation, gateway redirect/buffer bounds, IPv6 checks, and dotted CDP request IDs
without a browser. `review_browser.mjs DIST` uses a disposable real Chrome profile and
a scripted provider to check permission/send races, camera gating, interrupted-reset
and sealed-move recovery (no reset backup kept after), a moved thread keeping one copy,
Storage freeing a thread's free space (a store fragmented under Node by the build's own
wasm, then imported) and a set-aside first attempt, page Stop, and file previews after
idle close/reopen. The package's `embed/tests/browser_opfs.mjs` adds 80 same-worker
close/reopen cycles with concurrent reads while retaining its two-worker contention test,
and compacts a store with free pages in OPFS; `node_compact.mjs` compacts one with its
task's under Node.

```sh
node chrome/tests/review_regressions.mjs
SPINDLE_BROWSER=$(command -v google-chrome) node chrome/tests/review_browser.mjs chrome/dist
# A real provider (real, billed requests); the endpoint comes from the catalog.
SPINDLE_BROWSER=$(command -v google-chrome) SPINDLE_KEY=... SPINDLE_BIN=target/release/spindle \
  node chrome/tests/extension_real.mjs chrome/dist --provider deepseek [--catalog ~/.dext/providers.json]
# A scripted provider served by the test (no account, no cost).
SPINDLE_BROWSER=$(command -v google-chrome) SPINDLE_BIN=target/release/spindle \
  node chrome/tests/extension_real.mjs chrome/dist --scripted
```

Headless Chrome cannot click through a permission prompt, so the test's copy of the
extension grants in its manifest what the extension asks for at run time: the
provider's site and the test's page, then (after a restart) what Page reading asks for.
The test's work folder, whose Chrome profile holds the key, is always removed;
`SPINDLE_SHOTS=DIR` keeps only screenshots, in DIR.

## Limits

- Chrome on the desktop only (side panel API, Chrome 116 or later). Not in Chrome's web
  store yet: load it unpacked.
- The browser Linux is emulated: commands are slower than native (Python starts in about
  half a second; `pip install` of one small package takes over a minute), and it is
  32-bit x86, so packages with native code need a 32-bit musl build. Its network through
  Chrome is HTTP and HTTPS for clients that use a proxy from the environment (most do);
  others, and other protocols, need a relay. Through Chrome there are no cookies (Chrome
  neither sends nor shows them), downloads run at about 2 MB/s, a request body is at
  most 64 MiB. Chrome can hide a redirect's target, so those redirects are refused
  without being followed; use the final public address or your relay. Your host's
  sandbox has none of these gateway limits.
- The wasm Linux has no bash, Node, git or network, Python without ssl, ctypes or C
  extensions, and no `fork` (programs start others with posix_spawn); routing keeps
  what needs those on the emulated Linux. A program that computes without ever calling
  the kernel cannot be interrupted there: stopping it (or its timeout) restarts that
  machine in under a second, and commands it cuts off run again.
- Code mode here is bash plus the `spindle` calls (Python 3, Node, TypeScript and bash
  scripts); native `run_code` itself and code packs are not offered (the page tools and
  an app's tools are what scripts call instead). On your host's sandbox, scripts have no
  `spindle` calls (the bridge is the browser Linux's). A site you list receives whatever
  a script sends it; removing it from the list does not take back Chrome's access to it
  (Chrome's extension settings do).
- Browser sign-in is offered for providers whose catalog entry has a standard OAuth flow
  with a loopback redirect (ChatGPT among the built-ins). Anthropic takes an API key here.
- Without a passphrase, keys are sealed with a key kept in the same Chrome profile: they
  never sit there as text, but a copy of the whole profile can still open them. Chrome may
  keep earlier copies of records in its storage files until it compacts them (keys saved
  by an older build as text, or sealed before a passphrase was set): replace a key at its
  provider if that matters. A forgotten passphrase cannot be recovered: **Forgot it**
  deletes the saved keys and sign-ins (threads stay) so you can add them again.
- A page a thread reads can carry instructions; with page reading on, a thread can read
  any open tab and its text goes to your provider. With page actions on, such a page can
  steer a thread to click and type in your browser as you, on sites you are signed in
  to, and to go to any address (the sites list limits scripts' `http()`, not browsing):
  turn actions on for the work that needs them. Closing Chrome's debugging bar stops
  Spindle on that tab: its next action there is refused, telling the thread to ask you.
- A thread with network can send what is in its workspace to any website (through
  Chrome) or any address (through your relay), as a native thread with `--network` can:
  turn it on for the work that needs it. Turning page reading off leaves Chrome's access
  to sites in place for threads' network; Settings, Pages takes it back.
- The page tools see the top page, not inside frames; refs last until the page changes
  (read `page_elements` again after); a screenshot (the tool's, or one you attach) is not
  shown to the model directly (a script can ask a vision model about it). A page opened
  from the workspace gets its files put in (scripts, styles, pictures, and the small
  data files it or its scripts name, which its scripts fetch by relative address); links
  between its pages, larger data and files whose names a script puts together are not.
- A preview reaches nothing outside the workspace until you let it load from the web,
  and its scripts run as it shows. A page's policy cannot keep a script from sending
  what the page shows out of the browser (through WebRTC, a resource hint, or a host
  name it navigates to, which Chrome looks up before refusing), so the extension serves
  the preview's frame itself under a Connection-Allowlist: Chrome (152 and later) then
  makes no connection for the page, or for any frame or worker it makes, that the list
  does not name (nothing, or HTTPS once you let it load from the web; WebRTC never). The
  frame checks that the list holds before a page runs in it. Where it does not (an
  older Chrome), a page shows without its scripts, frames or outside links until you
  run that version of it, which then runs uncontained. Meta refreshes, resource hints
  and a page's own DNS-prefetch switch are taken out either way. A page that tries
  WebRTC from a `srcdoc` or `data:` frame it makes loses its preview (Chrome ends the
  frame's process; nothing leaves). Public names resolving into a private network remain a limitation of
  Chrome's fetch: the extension cannot inspect DNS resolution. Hidden redirects are
  refused before following, rather than after contacting their destination.
