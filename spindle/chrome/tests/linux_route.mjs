// Which browser Linux runs a command (chrome/linux-route.mjs): plain POSIX sh with
// programs the wasm Linux has goes there; bash-only syntax, programs it lacks and what
// cannot be told go to v86; a wasm result saying a program or syntax was missing runs
// again on v86 unless a Spindle call was made. A command that uses the packages and
// caches each Linux keeps for itself goes to v86, and so, in a thread with network on,
// does one that may use the network (a program that connects, Python that does, in the
// command or the workspace's scripts and modules it reaches); one that the wasm Linux's
// missing network fails, or (in a workspace holding compiled code) a compiled module it
// cannot load, runs again there.
//
// usage: node chrome/tests/linux_route.mjs
import assert from 'node:assert/strict';
import { NETWORK_PROGRAMS, needsV86, wantsV86, wasmCanRun } from '../linux-route.mjs';

const programs = new Set(['python3', 'python', 'ls', 'cat', 'grep', 'sed', 'awk', 'wc', 'head', 'sort', 'find', 'xargs', 'sleep', 'mkdir', 'rm', 'cp', 'tr', 'cut', 'date', 'env', 'sh', 'spindle', 'uname', 'tee', 'basename', 'dirname', 'touch', 'uniq', 'tail', 'timeout', 'nice', 'busybox']);
const wasm = [
  'echo a~b "~" \'~/x\'',
  'ls',
  'ls -la /workspace | grep txt | wc -l',
  'echo hi > out.txt && cat out.txt',
  'for f in *.txt; do wc -l "$f"; done',
  'if [ -f x ]; then cat x; else echo none; fi',
  'while read line; do echo "$line"; done < in.txt',
  'FOO=1 BAR="a b" env | grep FOO',
  "cat <<'EOF' > notes.md\n# Notes\n[[not bash]] $(nor this)\nEOF\nwc -l notes.md",
  'cat <<EOF > today.md\n# $(date +%F)\nEOF',
  'env FOO=1 ls',
  'timeout 5 sleep 1',
  'nice -n 5 sort words.txt',
  'xargs -n 1 echo < list.txt',
  'find . -name "*.py" -exec wc -l {} \\;',
  'sh -c "ls | wc -l"',
  'case x in a) ls;; esac; echo done',
  'echo $(( 1 + $(ls | wc -l) ))',
  'n=$(ls | wc -l); echo "count $n"',
  'echo "today $(date +%F)"',
  'find . -name "*.py" | xargs grep -l import 2>/dev/null',
  'mkdir -p a/b && touch a/b/c && ls -R a',
  'case "$x" in a) echo A ;; *) echo other ;; esac',
  'spindle tool list_tabs',
  '/bin/ls /tmp',
  'echo $((1 + 2))',
  'cd sub && ls; cd ..',
  '# a comment\nls',
  'sort -u words.txt | head -5 >&2',
  "awk '{ print $1 }' data.txt | sort | uniq -c",
  'python3 script.py',
  'python3 -c "print(1 + 1)" > out.txt',
  'cd src && python3 -m unittest discover 2>&1 | tail -5',
];
const v86 = [
  'ls ~',
  'cd ~/project && ls',
  'X=~/a ls',
  'PATH=/bin:~/bin ls',
  'node index.js',
  'git status',
  'npm install',
  '[[ -f x ]] && echo yes',
  'for i in {1..3}; do echo $i; done',
  'echo {a,b}.txt',
  'diff <(sort a) <(sort b)',
  'cat <<< "here string"',
  'arr=(1 2 3); echo ${arr[1]}',
  'echo ${name^^}',
  'function f { echo hi; }; f',
  'source env.sh',
  'ls &> out.txt',
  "echo $'tab\\there'",
  './run.sh',
  'bash -c "echo hi"',
  '$CMD --version',
  'echo "$(node -e 1)"',
  'ls | bash',
  'curl https://example.com',
  "echo 'unclosed",
  '',
  'declare -A map',
  '(( n++ ))',
  'env node x.js',
  'env -u HOME FOO=1 node x.js',
  'timeout 10 node x.js',
  'xargs node',
  'find . -name "*.js" -exec node {} \\;',
  'sh -c "node x.js"',
  'case x in a) ls;; esac; node x.js',
  'case x in a) ls;; esac && git status',
  'cat <<EOF\n$(node -e 1)\nEOF',
  'echo $(( $(node -e 1) + 1 ))',
  'eval "ls"',
  'nice -n 5 git status',
  'busybox sh -c "git status"',
];
for (const command of wasm) assert.equal(wasmCanRun(command, programs), true, `to the wasm Linux: ${JSON.stringify(command)}`);
for (const command of v86) assert.equal(wasmCanRun(command, programs), false, `to v86: ${JSON.stringify(command)}`);

// Run again on v86: a program or syntax the wasm Linux lacks, nothing else.
assert.equal(wantsV86({ success: false, code: 127, output: 'sh: python3: not found\n' }), true);
assert.equal(wantsV86({ success: false, code: 127, output: 'xargs: bash: not found\n' }), true);
// What busybox really prints (checked on the image).
assert.equal(wantsV86({ success: false, code: 127, output: "sh: can't execute 'nosuchprog': No such file or directory\n" }), true);
assert.equal(wantsV86({ success: false, code: 127, output: "env: can't execute 'node': No such file or directory\n" }), true);
assert.equal(wantsV86({ success: false, code: 127, output: 'xargs: node: No such file or directory\n' }), true);
assert.equal(wantsV86({ success: false, code: 2, output: "python3: can't open file '/workspace/x.py': [Errno 2] No such file or directory\n" }), false, 'a missing script is the command\'s');
assert.equal(wantsV86({ success: false, code: 2, output: 'sh: syntax error: unexpected "("\n' }), true);
assert.equal(wantsV86({ success: false, unavailable: true, output: '' }), true);
assert.equal(wantsV86({ success: false, code: 127, output: 'sh: python3: not found\n', called: true }), false, 'a Spindle call was made');
assert.equal(wantsV86({ success: false, code: 1, output: 'grep found nothing\n' }), false);
assert.equal(wantsV86({ success: false, code: 1, output: "grep: unrecognized option: P\nBusyBox v1.38.0 multi-call binary.\n\nUsage: grep [-HhnlLoqvsrRiwFE] PATTERN [FILE]...\n" }), true, 'a GNU option busybox lacks');
assert.equal(wantsV86({ success: false, code: 1, output: "date: invalid option -- 'd'\n" }), true);
assert.equal(wantsV86({ success: false, code: 1, output: "date: invalid date 'yesterday'\n" }), true);
assert.equal(wantsV86({ success: false, code: 127, output: 'my script failed on purpose\n' }), false);
assert.equal(wantsV86({ success: false, failure: 'the command did not finish within 5 s', output: '' }), false);
assert.equal(wantsV86({ success: true, code: 0, output: 'sh: x: not found\n' }), false);
assert.equal(wantsV86({ success: false, code: 1, output: 'Traceback (most recent call last):\n  File "a.py", line 1, in <module>\nModuleNotFoundError: No module named \'numpy\'\n' }), true);
assert.equal(wantsV86({ success: false, code: 1, output: '/usr/bin/python3: No module named pip\n' }), true, 'python3 -m pip: pip is v86\'s');
assert.equal(wantsV86({ success: false, code: 2, output: 'rg: PCRE2 is not available in this build of ripgrep\n' }), true, 'rg -P: v86\'s ripgrep has PCRE2');
assert.equal(wantsV86({ success: false, code: 1, output: '' }), false, 'rg finding nothing is its answer');
assert.equal(wantsV86({ success: false, code: 1, output: "Error: Command '['/workspace/.venv/bin/python3', '-m', 'ensurepip', '--upgrade', '--default-pip']' returned non-zero exit status 1.\n" }), true, 'a venv gets its pip on v86');
assert.equal(wantsV86({ success: false, code: 1, output: "Error: no file named 'pip' here\n" }), false);
assert.equal(wantsV86({ success: false, code: 1, output: 'Traceback (most recent call last):\n  File "<string>", line 1, in <module>\nMemoryError\n' }), true, 'v86 has more memory');
assert.equal(wantsV86({ success: false, code: 134, output: 'memory allocation of 536870912 bytes failed\n' }), true);
assert.equal(wantsV86({ success: false, code: 1, output: 'sort: out of memory\n' }), true);
assert.equal(wantsV86({ success: false, code: 1, output: 'ran out of memory budget: 3 items left\n' }), false, 'a script\'s own words');
const missingProgram = 'Traceback (most recent call last):\n  File "/usr/lib/python3.14/subprocess.py", line 1955, in _execute_child\nFileNotFoundError: [Errno 2] No such file or directory: \'git\'\n';
assert.equal(wantsV86({ success: false, code: 1, output: missingProgram }), true);
assert.equal(wantsV86({ success: false, code: 1, output: 'Traceback (most recent call last):\n  File "a.py", line 1, in <module>\nFileNotFoundError: [Errno 2] No such file or directory: \'data.csv\'\n' }), false, 'a missing data file is the script\'s');
// A program a command saved does not run in the wasm Linux (its workspace and scratch
// are data only): v86 runs the command again.
assert.equal(wantsV86({ success: false, code: 126, output: 'sh: ./tool: Permission denied\n' }), true);
assert.equal(wantsV86({ success: false, code: 126, output: "env: can't execute './tool': Permission denied\n" }), true);
const savedProgram = 'Traceback (most recent call last):\n  File "/usr/lib/python3.14/subprocess.py", line 1955, in _execute_child\nPermissionError: [Errno 13] Permission denied: \'./tool\'\n';
assert.equal(wantsV86({ success: false, code: 1, output: savedProgram }), true);
assert.equal(wantsV86({ success: false, code: 1, output: 'Traceback (most recent call last):\n  File "a.py", line 1, in <module>\nPermissionError: [Errno 13] Permission denied: \'/etc/shadow\'\n' }), false, 'a file the script may not read is the script\'s');
// A thread with network on: the wasm Linux has none, so what may use it goes to v86.
const text = (value) => new TextEncoder().encode(value);
const workspace = {
  'build.py': 'import json, csv\nfrom pathlib import Path\nfrom helpers import tidy\nprint(tidy(json.load(open("a.json"))))\n',
  'helpers.py': 'import re\nfrom urllib.parse import urljoin\ndef tidy(x): return x\n',
  'gfl/fetch.py': 'import urllib.request, json\ndef search(q): return urllib.request.urlopen(q).read()\n',
  'gfl/parse.py': 'import re\n',
  'pkg/__init__.py': 'from .net import get\n',
  'pkg/net.py': 'import socket\ndef get(): return socket.create_connection(("x", 80))\n',
  'pkg2/__init__.py': '',
  'pkg2/calc.py': 'from . import util\n',
  'pkg2/util.py': 'import math\n',
  'tools/run.py': 'import subprocess\nsubprocess.run(["curl", "-s", "https://example.com"])\n',
  '.packages/bs4/__init__.py': 'from .element import Tag\n',
  '.packages/bs4/element.py': 'import re\n',
  'scrape.py': 'import requests\nprint(requests.get("https://example.com").text)\n',
  'reads_cache.py': 'from pathlib import Path\nprint(Path(".cache/sub/page.html").read_text()[:80])\n',
  'pkg3/__init__.py': '',
  'pkg3/util.py': 'import math\n',
  'pkg3/net.py': 'import socket\n',
  'multi.py': 'from pkg3 import (\n    util,  # the helpers\n    net,\n)\n',
  // What pip installed (`--target pylib`): only the network modules it imports count.
  'pylib/html5lib-1.1.dist-info/RECORD': '',
  'pylib/html5lib/__init__.py': 'from .filters import sanitizer\n',
  'pylib/html5lib/filters/__init__.py': '',
  'pylib/html5lib/filters/sanitizer.py': "allowed = frozenset(((None, 'ping'), (None, 'href')))\n# see urlopen(url) in the docs; pip install html5lib[all]\n",
  'pylib/fetcher-2.0.dist-info/RECORD': '',
  'pylib/fetcher/__init__.py': 'import urllib3\n',
  'tidy.py': 'import os\nfor root, dirs, names in os.walk("."):\n    dirs[:] = [d for d in dirs if d != "__pycache__"]\n',
};
const files = Object.entries(workspace).map(([path, body]) => ({ path, size: body.length, object: `o-${path}` }));
const read = (object) => text(workspace[object.slice(2)]);
const offline = new Set([...programs].filter((name) => !NETWORK_PROGRAMS.has(name)));
const reaches = (command) => needsV86(command, files, read);
for (const command of [
  'python3 build.py',
  'python3 /workspace/build.py > out.txt',
  'cd gfl && python3 parse.py',
  "python3 - <<'PY'\nimport json\nprint(json.dumps({'a': 1}))\nPY",
  "PYTHONPATH=.packages python3 - <<'PY'\nfrom bs4 import BeautifulSoup\nPY",
  'python3 -c "from pkg2 import calc"',
  'python3 -m pkg2.calc',
  'PYTHONPATH=pylib python3 -c "import html5lib"',
  'rm -rf __pycache__ .pytest_cache',
  'python3 tidy.py',
  "python3 - <<'PY'\nprint('see https://example.com for the data')\nPY",
  'spindle http https://api.example.com/v1/x',
]) assert.equal(reaches(command), false, `no network: ${JSON.stringify(command)}`);
for (const command of [
  "cd /workspace && python3 - <<'EOF'\nimport sys; sys.path.insert(0, '/workspace/gfl')\nimport fetch as F\nprint(F.search('x'))\nEOF",
  'python3 scrape.py',
  'python3 -c "import pkg"',
  'python3 tools/run.py',
  "python3 - <<'PY'\nimport urllib.request\nurllib.request.urlopen('https://example.com')\nPY",
  "python3 - <<'PY'\nimport os, socket\nPY",
  "python3 - <<'PY'\nfrom urllib import request\nPY",
  "python3 - <<'PY'\nimport subprocess\nsubprocess.run('git pull', shell=True)\nPY",
  'python3 -m pip install requests',
  'python3 -c "import socket; print(socket.gethostname())"',
  'cd sub && python3 ../scrape.py',
  // What v86 keeps for the thread (packages and caches): curl filled .cache there.
  'python3 parse.py .cache/sub/page.html',
  'ls .venv/bin/python 2>/dev/null || true',
  'rm -rf __pycache__ .venv',
  'python3 reads_cache.py',
  'python3 multi.py',
  'PYTHONPATH=pylib python3 -c "import fetcher"',
]) assert.equal(reaches(command), true, `needs v86: ${JSON.stringify(command)}`);
// Without network, only what each Linux keeps for itself counts (the network is refused
// in both).
const offlineThread = (command) => needsV86(command, files, read, { network: false });
for (const command of ['python3 scrape.py', 'python3 -m pip install requests', 'python3 tools/run.py']) {
  assert.equal(offlineThread(command), false, `no network to use: ${JSON.stringify(command)}`);
}
for (const command of ['python3 -m venv --without-pip .venv', 'ls node_modules', 'python3 reads_cache.py']) {
  assert.equal(offlineThread(command), true, `kept by v86: ${JSON.stringify(command)}`);
}
assert.equal(needsV86('python3 x.py', [{ path: 'x.py', size: 4, object: 'gone' }], () => { throw new Error('no such object'); }), true, 'what cannot be read counts');
const many = Array.from({ length: 300 }, (_, index) => ({ path: `m${index}.py`, size: 10, object: `m${index}` }));
assert.equal(needsV86('python3 m0.py', many, (object) => text(`import m${Number(object.slice(1)) + 1}\n`)), true, 'too far to tell counts');
const cache = new Map();
let reads = 0;
needsV86('python3 build.py', files, (object) => (reads++, read(object)), { cache });
needsV86('python3 build.py', files, (object) => (reads++, read(object)), { cache });
assert.equal(reads, 2, 'each file is read once (build.py, helpers.py)');
assert.equal(wasmCanRun('wget -q -O- https://example.com', offline), false, 'busybox wget');
assert.equal(wasmCanRun('nslookup example.com', offline), false);
assert.equal(wasmCanRun('python3 build.py && ls', offline), true);
// Run again on v86 when the network it lacks failed a command (what the image prints).
const gai = 'Traceback (most recent call last):\n  File "<string>", line 1, in <module>\nsocket.gaierror: [Errno -3] Try again\n';
assert.equal(wantsV86({ success: false, code: 1, output: gai }, { network: true }), true);
assert.equal(wantsV86({ success: false, code: 1, output: gai }), false, 'without network, the refusal is the answer');
assert.equal(wantsV86({ success: false, code: 1, output: 'OSError: [Errno 101] Network unreachable\n' }, { network: true }), true);
assert.equal(wantsV86({ success: false, code: 1, output: 'urllib.error.URLError: <urlopen error unknown url type: https>\n' }, { network: true }), true);
assert.equal(wantsV86({ success: false, code: 1, output: "urllib.error.URLError: <urlopen error [Errno -3] Try again>\n" }, { network: true }), true);
assert.equal(wantsV86({ success: false, code: 1, output: "wget: bad address 'example.com'\n" }, { network: true }), true);
assert.equal(wantsV86({ success: true, code: 0, output: 'fetch failed ([Errno -3] Try again), using the cache\n' }, { network: true }), false, 'a result taken stands');
assert.equal(wantsV86({ success: false, code: 1, output: gai, called: true }, { network: true }), false, 'a Spindle call was made');
// A package pip put in the workspace with a compiled module (`from . import _speedups`).
const notLoaded = "Traceback (most recent call last):\n  File \"/workspace/pylib/fast/__init__.py\", line 1, in <module>\n    from . import _speed\nImportError: cannot import name '_speed' from partially initialized module 'fast' (most likely due to a circular import) (/workspace/pylib/fast/__init__.py)\n";
assert.equal(wantsV86({ success: false, code: 1, output: notLoaded }, { compiled: true }), true);
assert.equal(wantsV86({ success: false, code: 1, output: notLoaded }), false, 'no compiled code in the workspace: the import error is the script\'s');
console.log(`linux-route passed: ${wasm.length} commands to the wasm Linux, ${v86.length} to v86, fallbacks told (GNU options, saved programs, network too)`);
