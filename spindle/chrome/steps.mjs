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

/** A step's label and how to show it: {label, domain, url, title}. `state`: running,
 * done or failed; `tabs`: tabsFrom(the steps before it); `parts`: partsFrom(them). */
export function describe(name, input = {}, state = 'done', tabs = new Map(), parts = new Map()) {
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
    default: return { label: say('use', name.replace(/_/g, ' ')) };
  }
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

/** A duration as a row shows it: 820ms, 1.2s, 14s, 2m 5s. */
export function duration(ms) {
  if (!Number.isFinite(ms) || ms < 0) return '';
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 9950) return `${(ms / 1000).toFixed(1)}s`;
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  return `${Math.floor(seconds / 60)}m${seconds % 60 ? ` ${seconds % 60}s` : ''}`;
}

/** Files a command run in a sandbox is not meant to leave (kept out of sight by default). */
export function isScratch(path) {
  return /\.(txt|py|pyc|log|tmp)$/i.test(path) || /(^|\/)(tests?|__pycache__|\.cache)\//.test(path);
}
