// Which browser Linux runs a command (chrome/linux-route.mjs): plain POSIX sh with
// programs the wasm Linux has goes there; bash-only syntax, programs it lacks and what
// cannot be told go to v86; a wasm result saying a program or syntax was missing runs
// again on v86 unless a Spindle call was made.
//
// usage: node chrome/tests/linux_route.mjs
import assert from 'node:assert/strict';
import { wantsV86, wasmCanRun } from '../linux-route.mjs';

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
const missingProgram = 'Traceback (most recent call last):\n  File "/usr/lib/python3.13/subprocess.py", line 1955, in _execute_child\nFileNotFoundError: [Errno 2] No such file or directory: \'git\'\n';
assert.equal(wantsV86({ success: false, code: 1, output: missingProgram }), true);
assert.equal(wantsV86({ success: false, code: 1, output: 'Traceback (most recent call last):\n  File "a.py", line 1, in <module>\nFileNotFoundError: [Errno 2] No such file or directory: \'data.csv\'\n' }), false, 'a missing data file is the script\'s');
console.log(`linux-route passed: ${wasm.length} commands to the wasm Linux, ${v86.length} to v86, fallbacks told (GNU options too)`);
