// What a thread's steps (its tool calls) say in the panel: a short plain-English label for
// each, in the tense of where it is (running: "Running report.py"; done: "Ran report.py"),
// the page a browser step acted on (its domain or title, from what earlier steps found out
// about the tab) and the part of it (a button, a field, by what page_elements named it),
// and which failed steps a later one did again successfully. Pure: the panel draws what
// these return.

/** A verb's three forms: the plain one (a failed step), running, done. */
const VERBS = {
  run: ['Run', 'Running', 'Ran'],
  read: ['Read', 'Reading', 'Read'],
  edit: ['Edit', 'Editing', 'Edited'],
  write: ['Write', 'Writing', 'Wrote'],
  list: ['List', 'Listing', 'Listed'],
  search: ['Search', 'Searching', 'Searched'],
  find: ['Find', 'Finding', 'Found'],
  fetch: ['Fetch', 'Fetching', 'Fetched'],
  install: ['Install', 'Installing', 'Installed'],
  remove: ['Delete', 'Deleting', 'Deleted'],
  make: ['Make', 'Making', 'Made'],
  move: ['Move', 'Moving', 'Moved'],
  copy: ['Copy', 'Copying', 'Copied'],
  count: ['Count', 'Counting', 'Counted'],
  check: ['Check', 'Checking', 'Checked'],
  test: ['Test', 'Testing', 'Tested'],
  pack: ['Pack', 'Packing', 'Packed'],
  unpack: ['Unpack', 'Unpacking', 'Unpacked'],
  ask: ['Ask', 'Asking', 'Asked'],
  call: ['Call', 'Calling', 'Called'],
  use: ['Use', 'Using', 'Used'],
  go: ['Go to', 'Going to', 'Went to'],
  open: ['Open', 'Opening', 'Opened'],
  click: ['Click', 'Clicking', 'Clicked'],
  type: ['Type into', 'Typing into', 'Typed into'],
  choose: ['Choose in', 'Choosing in', 'Chose in'],
  press: ['Press', 'Pressing', 'Pressed'],
  scroll: ['Scroll', 'Scrolling', 'Scrolled'],
  wait: ['Wait for', 'Waiting for', 'Waited for'],
  picture: ['Picture', 'Picturing', 'Pictured'],
  watch: ['Watch', 'Watching', 'Watched'],
  close: ['Close', 'Closing', 'Closed'],
  answer: ['Answer', 'Answering', 'Answered'],
  start: ['Start', 'Starting', 'Started'],
};
const TENSE = { failed: 0, running: 1, done: 2 };

/** The tabs earlier steps told about: id -> {title, url} (a step that failed told
 * nothing). */
export function tabsFrom(steps) {
  const tabs = new Map();
  for (const { name, input, result, failed } of steps) {
    if (typeof result !== 'string' || failed) continue;
    if (name === 'list_tabs') {
      for (const match of result.matchAll(/^tab (\d+)\*?: (.*?) <([^>]+)>$/gm)) tabs.set(Number(match[1]), { title: match[2], url: match[3] });
    } else if (name === 'read_page' || name === 'page_elements' || name === 'page_html') {
      const url = /^Address: (\S+)/m.exec(result)?.[1];
      const title = /^Title: (.*)$/m.exec(result)?.[1];
      const tab = Number(input?.tab ?? /\btab (\d+)\b/.exec(result)?.[1]);
      if (url && Number.isFinite(tab)) tabs.set(tab, { title: title ?? '', url });
    } else if (name === 'navigate' && input?.url) {
      const tab = Number(/\btab (\d+)\b/.exec(result)?.[1] ?? input.tab);
      if (Number.isFinite(tab)) tabs.set(tab, { title: '', url: input.url });
    }
  }
  return tabs;
}

/** The parts of pages earlier steps read (page_elements: `button "Go" [k3]`): ref ->
 * {role, name}. */
export function partsFrom(steps) {
  const parts = new Map();
  for (const { name, result } of steps) {
    if (name !== 'page_elements' || typeof result !== 'string') continue;
    for (const match of result.matchAll(/^[\s-]*([a-z]+)(?: "((?:[^"\\]|\\.)*)")?[^\n[]*\[([A-Za-z0-9]+)\]\s*$/gm)) {
      parts.set(match[3], { role: match[1], name: match[2] ?? '' });
    }
  }
  return parts;
}

const ROLE_WORDS = {
  textbox: 'field', searchbox: 'search box', combobox: 'list', listbox: 'list', spinbutton: 'field',
  menuitem: 'menu item', menuitemcheckbox: 'menu item', menuitemradio: 'menu item', radio: 'option',
  switch: 'switch', slider: 'slider', tab: 'tab', link: 'link', button: 'button', checkbox: 'checkbox',
  heading: 'heading', option: 'option', img: 'picture',
};

/** A part of a page in words: the “Go” button; a field; the part (when not known). */
function partWords(ref, parts, fallback) {
  const part = ref != null ? parts.get(String(ref)) : null;
  if (!part) return fallback;
  const role = ROLE_WORDS[part.role] ?? part.role;
  return part.name ? `the ${quote(part.name, 24)} ${role}` : `a ${role}`;
}

/** `url`'s host without www., or null. */
export function domainOf(url) {
  try {
    const { hostname, pathname } = new URL(url);
    const host = hostname.replace(/^www\./, '');
    // A site's own pages are told apart by their first path part (x.com/someone).
    const first = pathname.split('/').filter(Boolean)[0];
    return first && first.length <= 24 && !first.includes('.') ? `${host}/${first}` : host;
  } catch {
    return null;
  }
}

const base = (path) => String(path ?? '').split('/').filter(Boolean).pop() ?? String(path ?? '');
const quote = (text, most = 32) => {
  const short = String(text).replace(/\s+/g, ' ').trim();
  return `“${short.length > most ? `${short.slice(0, most - 1)}…` : short}”`;
};

/** A shell command's first real program and its words (what it starts with: cd, env
 * assignments and sudo set it up; a `cd x && …` names what follows). */
function firstProgram(command) {
  const parts = String(command).split(/\s*(?:&&|\|\||;|\n)\s*/).map((part) => part.trim()).filter(Boolean);
  for (const whole of parts) {
    // A pipeline is named by its first program.
    const part = whole.split('|')[0].trim();
    const words = part.split(/\s+/).filter((word) => word && !/^[A-Za-z_][A-Za-z0-9_]*=/.test(word));
    while (['sudo', 'time', 'env', 'exec', 'nohup'].includes(words[0])) words.shift();
    if (!words.length || ['cd', 'set', 'export', 'source', '.', 'true'].includes(words[0])) continue;
    return { program: base(words[0]), words: words.map((word) => word.replace(/^['"]|['"]$/g, '')), more: parts.length > 1 };
  }
  return { program: '', words: [], more: false };
}

/** The file a command's first line sends its output to (`> file`, `>> file`), or null. */
function written(command) {
  const line = String(command).split('\n')[0];
  return /(?:^|[^>0-9&])>{1,2}\s*([^\s<>&|;]+)/.exec(line)?.[1]?.replace(/^['"]|['"]$/g, '') ?? null;
}

/** What a shell command does, as a verb and its object. */
function shell(command) {
  const { program, words } = firstProgram(command);
  const args = words.slice(1).filter((word) => !word.startsWith('-'));
  const file = args.find((word) => /\.[A-Za-z0-9]{1,6}$/.test(word) || word.includes('/'));
  switch (program) {
    case 'python': case 'python3': {
      if (words.includes('-c')) return ['run', 'Python code'];
      const module = words[words.indexOf('-m') + 1];
      if (words.includes('-m') && module) {
        if (module === 'pip') return ['install', words.slice(words.indexOf('-m') + 3).filter((w) => !w.startsWith('-')).join(' ') || 'packages'];
        if (['pytest', 'unittest'].includes(module)) return ['run', 'the tests'];
        return ['run', module];
      }
      return file ? ['run', base(file)] : ['run', 'Python'];
    }
    case 'node': case 'deno': case 'bun': case 'tsx': case 'ts-node':
      return words.includes('-e') ? ['run', 'JavaScript code'] : file ? ['run', base(file)] : ['run', program];
    case 'pip': case 'pip3': case 'npm': case 'pnpm': case 'yarn': case 'apk': case 'apt': case 'apt-get': case 'cargo': case 'gem':
      if (['install', 'add', 'i'].includes(args[0])) return ['install', args.slice(1).join(' ') || 'packages'];
      if (['test', 't'].includes(args[0])) return ['run', 'the tests'];
      if (args[0] === 'run' && args[1]) return ['run', `${program} ${args[1]}`];
      return ['run', `${program}${args[0] ? ` ${args[0]}` : ''}`];
    case 'pytest': case 'jest': case 'vitest': case 'mocha': return ['run', 'the tests'];
    case 'ls': case 'tree': return ['list', args[0] ? `files in ${args[0]}` : 'files'];
    case 'find': case 'fd': return ['find', args[0] && args[0] !== '.' ? `files in ${args[0]}` : 'files'];
    case 'cat': case 'head': case 'tail': case 'less': case 'more': case 'bat': {
      // cat > file (<<EOF): a file written.
      const target = program === 'cat' && written(command);
      if (target) return ['write', base(target)];
      return ['read', args.length ? args.slice(0, 2).map(base).join(', ') : 'a file'];
    }
    case 'wc': return ['count', args.length ? `lines in ${base(args[0])}` : 'lines'];
    case 'grep': case 'rg': case 'ag': case 'egrep': return ['search', args[0] ? `for ${quote(args[0], 24)}` : 'files'];
    case 'curl': case 'wget': case 'http': {
      const url = words.find((word) => /^https?:\/\//.test(word));
      return ['fetch', (url && domainOf(url)) || 'a page'];
    }
    case 'git': return args[0] === 'clone' ? ['fetch', args[1] ? domainOf(args[1]) ?? base(args[1]) : 'a repository'] : ['run', `git ${args[0] ?? ''}`.trim()];
    case 'mkdir': return ['make', args[0] ? `folder ${args[0]}` : 'a folder'];
    case 'touch': return ['make', args.map(base).join(', ') || 'a file'];
    case 'rm': case 'rmdir': return ['remove', args.map(base).slice(0, 2).join(', ') || 'files'];
    case 'mv': return ['move', args[0] ? base(args[0]) : 'files'];
    case 'cp': return ['copy', args[0] ? base(args[0]) : 'files'];
    case 'sed': return words.includes('-i') ? ['edit', base(args.at(-1)) || 'a file'] : ['run', 'sed'];
    case 'tar': case 'zip': case 'gzip': return words.some((word) => /^-?[a-z]*x/.test(word)) && program === 'tar' ? ['unpack', base(args.at(-1) ?? '')] : ['pack', base(args[0] ?? '') || 'files'];
    case 'unzip': case 'gunzip': return ['unpack', base(args[0] ?? '') || 'files'];
    case 'echo': case 'printf': case 'tee': {
      const target = program === 'tee' ? args[0] : written(command);
      return target ? ['write', base(target)] : ['run', program];
    }
    case 'spindle':
      if (args[0] === 'model') return ['ask', 'a model'];
      if (args[0] === 'tool') return ['call', args[1] ?? 'a tool'];
      if (args[0] === 'http') return ['fetch', domainOf(words.find((word) => /^https?:/.test(word)) ?? '') ?? 'a site'];
      return ['run', `spindle ${args[0] ?? ''}`.trim()];
    case 'uname': case 'id': case 'whoami': case 'pwd': case 'date': case 'env': case 'which': case 'df': case 'du':
      return ['check', program === 'du' ? 'sizes' : program];
    case '': return ['run', 'a command'];
    default: return ['run', program];
  }
}

/** The steps that act on a page (their rows show its site's icon). */
export const PAGE_STEPS = new Set(['read_page', 'page_elements', 'page_html', 'screenshot', 'navigate', 'click', 'type', 'select', 'press', 'scroll', 'wait_for', 'dialog', 'close_tab', 'network']);

/** A part of a page in a few words, without the page: “Go”; a link; `fallback`. */
function partShort(ref, parts, fallback) {
  const part = ref != null ? parts.get(String(ref)) : null;
  if (!part) return fallback;
  return part.name ? quote(part.name, 24) : `a ${ROLE_WORDS[part.role] ?? part.role}`;
}

/** What kind of step a command is, for its row's icon: code (Python, Node), folder, file,
 * web or command. */
function shellKind(command) {
  const { program } = firstProgram(command);
  if (/^(python3?|node|deno|bun|tsx|ts-node|pytest|jest|vitest|mocha)$/.test(program)) return 'code';
  if (/^(ls|tree|find|fd|mkdir|rmdir|du)$/.test(program)) return 'folder';
  if (/^(cat|head|tail|less|more|bat|wc|touch|sed|cp|mv|rm|echo|printf|tee)$/.test(program)) return 'file';
  if (/^(curl|wget|http)$/.test(program)) return 'web';
  return 'command';
}

/** A step's label and how to show it: {label, short, kind, domain, url, title}. `short`
 * says what a page step did without its page (the row shows the page's icon, and its
 * address on hover); `kind` names its icon (command, code, file, folder, tabs, page,
 * task, web, tool). `state`: running, done or failed; `tabs`: tabsFrom(the steps before
 * it); `parts`: partsFrom(them). */
export function describe(name, input = {}, state = 'done', tabs = new Map(), parts = new Map()) {
  const said = describeLong(name, input, state, tabs, parts);
  const tense = TENSE[state] ?? 2;
  const say = (verb, object) => `${VERBS[verb][tense]}${object ? ` ${object}` : ''}`;
  const short = {
    read_page: () => say('read', 'page'),
    page_elements: () => say('read', 'parts'),
    page_html: () => say('read', 'HTML'),
    screenshot: () => say('picture', 'the tab'),
    click: () => say('click', partShort(input?.ref, parts, 'on the page')),
    type: () => say('type', partShort(input?.ref, parts, 'a field')),
    select: () => say('choose', partShort(input?.ref, parts, 'a list')),
    press: () => say('press', input?.key ?? 'a key'),
    scroll: () => say('scroll', 'the page'),
    wait_for: () => say('wait', input?.text ? quote(input.text, 20) : 'the page'),
    dialog: () => say('answer', 'a dialog'),
    close_tab: () => say('close', 'the tab'),
    network: () => say('watch', 'the network'),
    // Back, forward and reload act on the page it is on; going to an address names where.
    navigate: () => ({
      back: ['Go back', 'Going back', 'Went back'],
      forward: ['Go forward', 'Going forward', 'Went forward'],
      reload: ['Reload the tab', 'Reloading the tab', 'Reloaded the tab'],
    }[input?.action]?.[tense]),
  }[name]?.() ?? said.label;
  const kind = name === 'bash' ? shellKind(input?.command ?? '')
    : /^(read_file|write_file|edit_file)$/.test(name) ? 'file'
    : name === 'list_tabs' ? 'tabs'
    : name === 'task' ? 'task'
    : PAGE_STEPS.has(name) ? 'page'
    : 'tool';
  return { ...said, short, kind };
}

function describeLong(name, input = {}, state = 'done', tabs = new Map(), parts = new Map()) {
  const tense = TENSE[state] ?? 2;
  const say = (verb, object) => `${VERBS[verb][tense]}${object ? ` ${object}` : ''}`;
  const page = () => {
    const known = input?.tab != null ? tabs.get(Number(input.tab)) : null;
    const url = known?.url ?? (typeof input?.url === 'string' ? input.url : null);
    const title = known?.title?.trim() || null;
    return { url, title, domain: url ? domainOf(url) : null, where: (url && domainOf(url)) || title || (input?.tab != null ? 'a tab' : 'the page you are on') };
  };
  switch (name) {
    case 'bash': {
      const [verb, object] = shell(input?.command ?? '');
      return { label: say(verb, object) };
    }
    case 'read_file': return { label: say('read', base(input?.path)) };
    case 'write_file': return { label: say('write', base(input?.path)) };
    case 'edit_file': return { label: say('edit', base(input?.path)) };
    case 'list_tabs': return { label: say('list', 'open tabs') };
    case 'read_page': { const p = page(); return { label: say('read', p.where), ...p }; }
    case 'page_elements': { const p = page(); return { label: say('read', `the parts of ${p.where}`), ...p }; }
    case 'page_html': { const p = page(); return { label: say('read', `the HTML of ${p.where}`), ...p }; }
    case 'screenshot': { const p = page(); return { label: say('picture', p.where), ...p }; }
    case 'navigate': {
      if (input?.action === 'back' || input?.action === 'forward' || input?.action === 'reload') {
        const p = page();
        const verb = { back: ['Go back on', 'Going back on', 'Went back on'], forward: ['Go forward on', 'Going forward on', 'Went forward on'], reload: ['Reload', 'Reloading', 'Reloaded'] }[input.action];
        return { label: `${verb[tense]} ${p.where}`, ...p };
      }
      const url = input?.url ?? input?.path;
      const p = { url: /^https?:/.test(url ?? '') ? url : null, title: null, domain: /^https?:/.test(url ?? '') ? domainOf(url) : null };
      return { label: say(input?.new_tab ? 'open' : 'go', p.domain ?? (url ? base(url) : 'a page')), ...p };
    }
    case 'click': { const p = page(); const part = partWords(input?.ref, parts, null); return { label: say('click', `${part ? `${part} ` : ''}on ${p.where}`), ...p }; }
    case 'type': { const p = page(); return { label: say('type', `${partWords(input?.ref, parts, 'a field')} on ${p.where}`), ...p }; }
    case 'select': { const p = page(); return { label: say('choose', `${partWords(input?.ref, parts, 'a list')} on ${p.where}`), ...p }; }
    case 'press': { const p = page(); return { label: say('press', `${input?.key ?? 'a key'} on ${p.where}`), ...p }; }
    case 'scroll': { const p = page(); return { label: say('scroll', p.where), ...p }; }
    case 'wait_for': { const p = page(); return { label: say('wait', input?.text ? quote(input.text, 20) : p.where), ...p }; }
    case 'dialog': { const p = page(); return { label: say('answer', `a dialog on ${p.where}`), ...p }; }
    case 'close_tab': { const p = page(); return { label: say('close', p.where), ...p }; }
    case 'network': { const p = page(); return { label: say('watch', `the network of ${p.where}`), ...p }; }
    // A background task's call only starts it; a task in the foreground runs in it.
    case 'task': return { label: say(input?.background === true ? 'start' : 'run', `a task: ${taskTitle(input?.prompt)}`) };
    default: return { label: say('use', name.replace(/_/g, ' ')) };
  }
}

/** What a task is called: the first line of what it was asked, without Markdown's marks,
 * cut at a word to about `most` characters. */
export function taskTitle(prompt, most = 60) {
  const line = String(prompt ?? '').split('\n').map((part) => part.replace(/^\s*(?:#{1,6}|>|[-*+]|\d+[.)])\s+/, '').replace(/[*_`]+/g, '').trim()).find(Boolean) ?? '';
  if (line.length <= most) return line || 'untitled';
  const cut = line.slice(0, most - 1);
  const word = cut.lastIndexOf(' ');
  return `${(word > most / 2 ? cut.slice(0, word) : cut).replace(/[\s,;:.]+$/, '')}…`;
}

/** What makes two steps the same action (for "failed → retried"). */
export function sameAction(a, b) {
  if (a.name !== b.name) return false;
  if (a.name === 'bash') return String(a.input?.command ?? '').trim() === String(b.input?.command ?? '').trim()
    || firstProgram(a.input?.command ?? '').program === firstProgram(b.input?.command ?? '').program && shell(a.input?.command ?? '').join() === shell(b.input?.command ?? '').join();
  const key = (input) => JSON.stringify([input?.path, input?.url, input?.tab, input?.ref, input?.key]);
  return key(a.input) === key(b.input);
}

/** Of `steps` ({name, input, failed}), in order, the indexes of failed ones a later step
 * did again without failing. */
export function retried(steps) {
  const found = new Set();
  steps.forEach((step, index) => {
    if (!step.failed) return;
    if (steps.slice(index + 1).some((later) => !later.failed && !later.running && sameAction(step, later))) found.add(index);
  });
  return found;
}

/** A duration as a row shows it: 820ms, 1.2s, 14s, then on the clock: 2:05, 1:04:00. */
export function duration(ms) {
  if (!Number.isFinite(ms) || ms < 0) return '';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 9950) return `${(ms / 1000).toFixed(1)}s`;
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  return clock(seconds * 1000);
}

/** A time on the clock, as a running one ticks: 0:42, 6:18, 1:02:03. */
export function clock(ms) {
  if (!Number.isFinite(ms) || ms < 0) return '';
  const seconds = Math.floor(ms / 1000);
  const two = (n) => String(n).padStart(2, '0');
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor(seconds / 60) % 60;
  return hours ? `${hours}:${two(minutes)}:${two(seconds % 60)}` : `${minutes}:${two(seconds % 60)}`;
}

/** Whether `span` is a step's [start, end] in ms: two numbers, the end not before the start
 * (anything else, such as a damaged stored timing, is not one). */
export function isSpan(span) {
  return Array.isArray(span) && span.length === 2 && span.every(Number.isFinite) && span[1] >= span[0];
}

/** How long a run of steps took on the clock: the time at least one of them was running,
 * so steps that ran at once (a batch of tasks) count once, never added up. `steps` are
 * {span: [start, end]} (ms) or, when only a duration is known, {ms}; those are added. */
export function elapsed(steps) {
  const spans = steps.map((step) => step.span).filter(isSpan).sort((a, b) => a[0] - b[0]);
  let total = 0;
  let reached = -Infinity;
  for (const [start, end] of spans) {
    if (end <= reached) continue;
    total += end - Math.max(start, reached);
    reached = end;
  }
  const alone = (step) => !isSpan(step.span) && Number.isFinite(step.ms) && step.ms >= 0;
  return steps.reduce((sum, step) => (alone(step) ? sum + step.ms : sum), total);
}

/** Files a command run in a sandbox is not meant to leave (kept out of sight by default). */
export function isScratch(path) {
  return /\.(txt|py|pyc|log|tmp)$/i.test(path) || /(^|\/)(tests?|__pycache__|\.cache)\//.test(path);
}

/** A file's name as the panel shows it. A picture Spindle named for the moment it was taken
 * (a page's, `screenshots/20261030T232633287-tab12.jpg`, or one captured with the message,
 * `screenshot-20261030-232633.png`; both in UTC) is "Screenshot 11:26:33 PM", the day added
 * when not today; any other file is its own name. The file keeps its real name. */
const SHOT = /^(?:screenshot-(\d{8})-(\d{6})\.png|(\d{8})T(\d{6})\d{3}-tab\d+\.jpg)$/;
export function shownName(path, now = new Date()) {
  const name = path.slice(path.lastIndexOf('/') + 1);
  const found = SHOT.exec(name);
  if (!found) return name;
  const [day, time] = found[1] ? [found[1], found[2]] : [found[3], found[4]];
  const n = (text, from, to) => Number(text.slice(from, to));
  const taken = new Date(Date.UTC(n(day, 0, 4), n(day, 4, 6) - 1, n(day, 6, 8), n(time, 0, 2), n(time, 2, 4), n(time, 4, 6)));
  if (Number.isNaN(taken.getTime())) return name;
  const clockTime = taken.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit', second: '2-digit' });
  if (taken.toDateString() === now.toDateString()) return `Screenshot ${clockTime}`;
  const date = taken.toLocaleDateString([], { month: 'short', day: 'numeric', ...(taken.getFullYear() === now.getFullYear() ? {} : { year: 'numeric' }) });
  return `Screenshot ${date}, ${clockTime}`;
}

/** A path as the panel shows it: its folders, then its shown name. */
export function shownPath(path) {
  const at = path.lastIndexOf('/');
  return `${at >= 0 ? path.slice(0, at + 1) : ''}${shownName(path)}`;
}
