// The panel's step labels (chrome/steps.mjs): plain words in the tense of where a step is,
// the page and part a browser step acted on, retried failures, durations, a group's time on
// the clock (steps run at once counted once), scratch files.
//
// usage: node chrome/tests/panel_steps.mjs
import assert from 'node:assert/strict';
import { clock, describe, duration, elapsed, isScratch, isSpan, partsFrom, retried, shownName, shownPath, tabsFrom } from '../steps.mjs';

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

// A page step's row says what it did without its page (its site's icon shows that, its
// address on hover); each step names the icon of its kind.
const short = (name, input, state = 'done') => describe(name, input, state, tabs, parts).short;
assert.equal(short('page_html', { tab: 7 }), 'Read HTML');
assert.equal(short('page_elements', { tab: 7 }, 'running'), 'Reading parts');
assert.equal(short('read_page', { tab: 9 }), 'Read page');
assert.equal(short('screenshot', { tab: 9 }), 'Pictured the tab');
assert.equal(short('type', { ref: 'k2', text: 'heron' }), 'Typed into “Search”');
assert.equal(short('type', { ref: 'zz' }), 'Typed into a field');
assert.equal(short('click', { ref: 'k3' }), 'Clicked “Go”');
assert.equal(short('press', { key: 'Ctrl+Alt+Z' }), 'Pressed Ctrl+Alt+Z');
assert.equal(short('list_tabs', {}), 'Listed open tabs');
assert.equal(short('navigate', { url: 'https://www.example.com/x/y' }), 'Went to example.com/x', 'where it went is what it did');
assert.equal(short('navigate', { action: 'back', tab: 7 }), 'Went back');
assert.equal(short('navigate', { action: 'reload', tab: 7 }, 'running'), 'Reloading the tab');
assert.equal(short('bash', { command: 'python3 build_report.py' }), 'Ran build_report.py');
assert.equal(describe('page_html', { tab: 7 }, 'done', tabs).url, 'https://docs.example.org/guide');
const kind = (name, input) => describe(name, input).kind;
assert.deepEqual([kind('bash', { command: 'python3 a.py' }), kind('bash', { command: 'ls -la' }), kind('bash', { command: 'make' }), kind('bash', { command: 'curl -s https://a.example' }),
  kind('write_file', { path: 'a' }), kind('list_tabs', {}), kind('task', {}), kind('click', {}), kind('some_new_tool', {})],
['code', 'folder', 'command', 'web', 'file', 'tabs', 'task', 'page', 'tool']);

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

assert.deepEqual([0, 820, 1234, 9949, 9960, 14_400, 59_600, 61_000, 125_000, 3_047_000, 3_599_400, 3_600_000, 3_870_000, 7_190_000, -1].map(duration),
  ['0ms', '820ms', '1.2s', '9.9s', '10s', '14s', '1:00', '1:01', '2:05', '50:47', '59:59', '1:00:00', '1:04:30', '1:59:50', '']);
// A running time ticks on the clock from the start.
assert.deepEqual([0, 999, 42_000, 378_000, 3_723_000, -1].map(clock), ['0:00', '0:00', '0:42', '6:18', '1:02:03', '']);

// A group's time is the time any of its steps was running: five tasks run at once for ten
// minutes took ten minutes, not fifty; steps one after another add up; gaps between them
// (the model thinking) are not steps' time.
const minute = 60_000;
const at = (start, end) => ({ span: [start * minute, end * minute] });
assert.equal(elapsed([at(0, 10), at(0, 10), at(0, 9), at(0, 10), at(1, 10)]), 10 * minute);
assert.equal(elapsed([at(0, 2), at(2, 5), at(6, 7)]), 6 * minute);
assert.equal(elapsed([at(5, 9), at(0, 3), at(2, 6)]), 9 * minute, 'out of order and overlapping');
assert.equal(elapsed([at(0, 10), at(2, 4)]), 10 * minute, 'one inside another');
assert.equal(elapsed([at(0, 1), { ms: 30_000 }, { ms: undefined }]), 1.5 * minute, 'a duration alone is added');
assert.equal(elapsed([]), 0);
// What is not a span (a damaged stored timing, a clock set back) is left out, never
// subtracted; a negative duration alone too.
assert.equal(elapsed([at(0, 4), { span: [3 * minute, 1 * minute] }, { span: ['a', 'b'] }, { span: [NaN, 5] }, { ms: -500 }]), 4 * minute);
assert.ok(isSpan([1, 1]) && isSpan([0, 5]) && !isSpan([5, 0]) && !isSpan([1]) && !isSpan('1,2') && !isSpan([1, Infinity]) && !isSpan(null));
assert.ok(isScratch('notes.txt') && isScratch('build_report.py') && isScratch('tests/test_a.js') && isScratch('src/__pycache__/a.pyc'));
assert.ok(!isScratch('report.html') && !isScratch('data/table.csv') && !isScratch('README.md') && !isScratch('latest/index.html'));
// A picture named for its moment shows as one: its time, the day too when not today; any
// other file by its name.
const taken = new Date(Date.UTC(2026, 9, 30, 23, 26, 33));
const time = taken.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit', second: '2-digit' });
assert.equal(shownName('screenshots/20261030T232633287-tab2054620173.jpg', taken), `Screenshot ${time}`);
assert.equal(shownName('screenshot-20261030-232633.png', taken), `Screenshot ${time}`, 'a capture sent with a message');
const later = new Date(taken.getTime() + 3 * 86_400_000);
assert.equal(shownName('screenshots/20261030T232633287-tab7.jpg', later), `Screenshot ${taken.toLocaleDateString([], { month: 'short', day: 'numeric' })}, ${time}`);
assert.match(shownPath('screenshots/20261030T232633287-tab7.jpg'), /^screenshots\/Screenshot /);
for (const plain of ['report.html', 'data/20261030-232633.png', 'screenshots/20261030T232633287.jpg', 'notes/screenshot-2026-10-30.png']) {
  assert.equal(shownName(plain), plain.split('/').pop(), plain);
}
console.log('panel-steps passed: labels in three tenses, pages and parts named, short page labels and kinds, retried failures, durations, a group\'s time on the clock, scratch files, screenshots by their time');
