// The panel's step labels (chrome/steps.mjs): plain words in the tense of where a step is,
// the page and part a browser step acted on, retried failures, durations, scratch files.
//
// usage: node chrome/tests/panel_steps.mjs
import assert from 'node:assert/strict';
import { describe, duration, isScratch, partsFrom, retried, tabsFrom } from '../steps.mjs';

const label = (name, input, state = 'done', tabs, parts) => describe(name, input, state, tabs, parts).label;

// Shell commands, each tense.
assert.equal(label('bash', { command: 'python3 build_report.py' }, 'running'), 'Running build_report.py');
assert.equal(label('bash', { command: 'python3 build_report.py' }), 'Ran build_report.py');
assert.equal(label('bash', { command: 'python3 build_report.py' }, 'failed'), 'Run build_report.py');
assert.equal(label('bash', { command: 'python3 -c "print(1)"' }), 'Ran Python code');
assert.equal(label('bash', { command: 'cd src && python3 -m unittest discover 2>&1 | tail -5' }), 'Ran the tests');
assert.equal(label('bash', { command: 'pip install requests rich' }, 'running'), 'Installing requests rich');
assert.equal(label('bash', { command: 'ls -la /bin | wc -l' }), 'Listed files in /bin');
assert.equal(label('bash', { command: 'cat data.csv | head -3' }), 'Read data.csv');
assert.equal(label('bash', { command: "cat > report.py << 'EOF'\nprint(1)\nEOF" }), 'Wrote report.py');
assert.equal(label('bash', { command: 'echo done >> log.txt' }, 'running'), 'Writing log.txt');
assert.equal(label('bash', { command: 'curl -s https://www.example.com/a/b' }), 'Fetched example.com/a');
assert.equal(label('bash', { command: 'grep -rn TODO src' }), 'Searched for “TODO”');
assert.equal(label('bash', { command: 'rm -rf build dist' }), 'Deleted build, dist');
assert.equal(label('bash', { command: 'mkdir -p out' }), 'Made folder out');
assert.equal(label('bash', { command: 'uname -a' }), 'Checked uname');
assert.equal(label('bash', { command: 'FOO=1 sudo node index.js' }), 'Ran index.js');
// File tools.
assert.equal(label('edit_file', { path: '/workspace/src/build_report.py' }, 'running'), 'Editing build_report.py');
assert.equal(label('write_file', { path: 'notes.md' }), 'Wrote notes.md');
assert.equal(label('read_file', { path: 'a/b.txt' }, 'failed'), 'Read b.txt');

// Browser steps: the tab named by what earlier steps found out, the part by page_elements.
const tabs = tabsFrom([
  { name: 'list_tabs', input: {}, result: 'tab 1734920815*: Field notes <https://notes.example.net/field>\ntab 7: Docs <https://docs.example.org/guide>' },
  { name: 'read_page', input: { tab: 9 }, result: 'Address: https://news.example.com/today\nTitle: Today\n...' },
]);
assert.deepEqual(tabs.get(1734920815), { title: 'Field notes', url: 'https://notes.example.net/field' });
assert.equal(tabs.get(9).url, 'https://news.example.com/today');
const parts = partsFrom([{ name: 'page_elements', result: 'heading "Field notes" level=1 [k1]\n  textbox "Search" value="" [k2]\n  button "Go" [k3]\n  link "Second page" → /second.html [k4]' }]);
assert.deepEqual(parts.get('k3'), { role: 'button', name: 'Go' });
assert.equal(label('read_page', { tab: 1734920815 }, 'running', tabs), 'Reading notes.example.net/field');
assert.equal(describe('read_page', { tab: 1734920815 }, 'done', tabs).url, 'https://notes.example.net/field');
assert.equal(label('read_page', {}, 'done', tabs), 'Read the page you are on');
assert.equal(label('click', { ref: 'k3', tab: 7 }, 'done', tabs, parts), 'Clicked the “Go” button on docs.example.org/guide');
assert.equal(label('type', { ref: 'k2', text: 'heron' }, 'running', tabs, parts), 'Typing into the “Search” field on the page you are on');
assert.equal(label('click', { ref: 'q9' }, 'done', tabs, parts), 'Clicked on the page you are on');
assert.equal(label('navigate', { url: 'https://www.example.com/x/y' }, 'running'), 'Going to example.com/x');
assert.equal(label('navigate', { url: 'https://example.com', new_tab: true }), 'Opened example.com');
assert.equal(label('navigate', { action: 'back', tab: 7 }, 'done', tabs), 'Went back on docs.example.org/guide');
assert.equal(label('screenshot', { tab: 9 }, 'done', tabs), 'Pictured news.example.com/today');
assert.equal(label('some_new_tool', {}), 'Used some new tool');

// A failed step a later one did again successfully is retried; one not done again is not.
const steps = [
  { name: 'bash', input: { command: 'python3 report.py' }, failed: true },
  { name: 'read_file', input: { path: 'missing.txt' }, failed: true },
  { name: 'edit_file', input: { path: 'report.py' }, failed: false },
  { name: 'bash', input: { command: 'python3 report.py' }, failed: false },
  { name: 'bash', input: { command: 'python3 other.py' }, failed: true },
];
assert.deepEqual([...retried(steps)], [0]);
assert.deepEqual([...retried([{ name: 'bash', input: { command: 'ls x' }, failed: true }, { name: 'bash', input: { command: 'ls x' }, running: true }])], []);

assert.deepEqual([0, 820, 1234, 9949, 9960, 14_400, 59_600, 61_000, 125_000, -1].map(duration),
  ['0ms', '820ms', '1.2s', '9.9s', '10s', '14s', '1m', '1m 1s', '2m 5s', '']);
assert.ok(isScratch('notes.txt') && isScratch('build_report.py') && isScratch('tests/test_a.js') && isScratch('src/__pycache__/a.pyc'));
assert.ok(!isScratch('report.html') && !isScratch('data/table.csv') && !isScratch('README.md') && !isScratch('latest/index.html'));
console.log('panel-steps passed: labels in three tenses, pages and parts named, retried failures, durations, scratch files');
