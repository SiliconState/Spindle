// Which browser Linux runs a command (linux-runner.mjs): the wasm Linux (chrome/wasm-linux:
// busybox `sh` (hush: no tilde expansion), its applets, `spindle`; no bash, no network)
// when it can, v86 otherwise,
// and v86 again when the wasm Linux turns out not to have what the command needs. No
// switch for the user: the decision is the command's. The parser is cautious: a command it
// cannot tell to be plain POSIX sh with programs the wasm Linux has goes to v86. So does
// one that uses the packages and caches each Linux keeps for itself, and, in a thread
// with network on, one that may use the network (`needsV86`) or that the wasm Linux's
// missing network turns out to fail.

import { ignored } from './linux-workspace.mjs';

/** Shell words that start or join commands (the next word is a command), and hush's
 * builtins (POSIX's and its own: they run without a program). */
const KEYWORDS = new Set(['if', 'then', 'else', 'elif', 'fi', 'for', 'while', 'until', 'do', 'done', 'case', 'esac', 'in', '!', '{', '}']);
const LEADS = new Set(['if', 'then', 'else', 'elif', 'while', 'until', 'do', '!', '{']);
const BUILTINS = new Set([
  '.', ':', '[', 'alias', 'bg', 'break', 'cd', 'command', 'continue', 'echo', 'exec', 'exit',
  'export', 'false', 'fg', 'getopts', 'hash', 'help', 'jobs', 'kill', 'local', 'printf', 'pwd', 'read',
  'readonly', 'return', 'set', 'shift', 'test', 'times', 'trap', 'true', 'type', 'ulimit', 'umask',
  'unalias', 'unset', 'wait',
]);
/** Where the wasm Linux's programs are, for a command named by its path. */
const DIRS = ['/bin/', '/sbin/', '/usr/bin/', '/usr/sbin/', '/usr/local/bin/'];
/** What only bash (or ksh) takes: such a command goes to v86 whole. */
const BASHISMS = [
  /\[\[|\]\]/, /(^|[^$])\(\(/, /[<>]\(/, /<<</, /&>/, /\|&/, /\$\{!/, /\$\{[^}]*\[/, /\$\{[^}]*(\^|,)/,
  /(^|[\s;&|(])(function|select|coproc|shopt|declare|typeset|mapfile|readarray|source|let|disown|pushd|popd|dirs|enable|complete|compgen|caller|suspend)(?=$|[\s;&|)])/,
  /[A-Za-z_][A-Za-z0-9_]*\+?=\(/,
  // Brace expansion: {a,b} and {1..3} outside quotes (sh takes them as they are).
  /(^|[^$\\])\{[^\s{}'"]*(,|\.\.)[^\s{}'"]*\}/,
];

/** Whether the wasm Linux can run `command`: plain POSIX sh whose every command it can
 * name (a builtin, or one of `programs`, the names in its image), those inside `$( )` and
 * backticks too. */
export function wasmCanRun(command, programs) {
  if (typeof command !== 'string' || !command.trim()) return false;
  const words = tokens(command);
  if (!words) return false;
  if (BASHISMS.some((pattern) => pattern.test(words.bare))) return false;
  return [words.list, ...words.nested].every((list) => commandsKnown(list, programs));
}

/** Whether every command a list of words names is one the wasm Linux has. */
function commandsKnown(list, programs) {
  // The next word names a command; a redirection's target comes next; `for NAME [in
  // WORDS]` and `case WORD in` take words that are not commands; a case item's pattern
  // runs to its `)`, and `esac` ends the case.
  let expect = true;
  let target = false;
  let pending = null;
  let pattern = false;
  for (let at = 0; at < list.length; at++) {
    const word = list[at];
    if (word.operator) {
      if (word.text === ';;') {
        pattern = true;
        expect = false;
      } else {
        if (word.text === ')' && pattern) pattern = false;
        expect = true;
      }
      continue;
    }
    if (target) {
      target = false;
      continue;
    }
    if (word.redirect) {
      target = !word.carried;
      continue;
    }
    if (pattern) {
      if (word.text === 'esac') {
        pattern = false;
        expect = false;
      }
      continue;
    }
    if (pending) {
      if (word.text === 'in') {
        if (pending === 'case') pattern = true;
        pending = pending === 'for' ? 'for in' : null;
      } else if (word.text === 'do' && pending.startsWith('for')) {
        pending = null;
        expect = true;
      }
      continue;
    }
    if (!expect) continue;
    if (/^[A-Za-z_][A-Za-z0-9_]*=/.test(word.text) && !word.quotedName) continue;
    if (word.text === 'for' || word.text === 'case') {
      pending = word.text;
      continue;
    }
    if (KEYWORDS.has(word.text)) {
      expect = LEADS.has(word.text);
      continue;
    }
    if (!known(word, programs) || !wrappedKnown(list, at, programs)) return false;
    expect = false;
  }
  return true;
}

/** Programs that run the command named after their own options, and how many words their
 * options take: those commands must be known too. */
const TAKES = {
  env: { flags: /^-[i0]$|^-$/, arg: /^-u$/ },
  nice: { flags: /^-\d+$|^--adjustment=/, arg: /^-n$/ },
  nohup: {},
  time: { flags: /^-[pv]$/ },
  exec: { flags: /^-[cl]$/, arg: /^-a$/ },
  stdbuf: { flags: /^-[ioe]./, arg: /^-[ioe]$/ },
  timeout: { flags: /^-[fpv]$|^--(preserve-status|foreground|verbose)$|^--(signal|kill-after)=/, arg: /^-[sk]$/, before: 1 },
  xargs: { flags: /^-[0prtx]$|^-[nILPsdEa]./, arg: /^-[nILPsdEa]$/ },
};
const SHELLS = new Set(['sh', 'ash', 'hush', 'dash']);

/** Whether what the command at `list[at]` runs in turn is known: the command after a
 * wrapper's options (env, nice, timeout, xargs…), each command of `find -exec`, the script
 * of `sh -c`. `eval` cannot be told. */
function wrappedKnown(list, at, programs) {
  const name = list[at].text.split('/').pop();
  if (name === 'eval') return false;
  // This simple command's words (to its end; redirections and their targets aside).
  const words = [];
  for (let next = at + 1; next < list.length && !list[next].operator; next++) {
    if (list[next].redirect) {
      if (!list[next].carried) next += 1;
      continue;
    }
    words.push(next);
  }
  if (name === 'find') {
    for (let index = 0; index < words.length - 1; index++) {
      if (/^-(exec|execdir|ok|okdir)$/.test(list[words[index]].text) && !commandAt(list, words[index + 1], programs)) return false;
    }
    return true;
  }
  if (SHELLS.has(name) || (name === 'busybox' && SHELLS.has(list[words[0]]?.text))) {
    const from = name === 'busybox' ? 1 : 0;
    const flag = words.slice(from).findIndex((index) => /^-[a-z]*c[a-z]*$/.test(list[index].text));
    if (flag < 0) return true;
    const script = list[words[from + flag + 1]];
    return Boolean(script) && !script.expanded && wasmCanRun(script.text, programs);
  }
  if (name === 'busybox') return !words.length || commandAt(list, words[0], programs);
  if (name === 'command') return !words.length || /^-[vV]$/.test(list[words[0]].text) || commandAt(list, words[0], programs);
  const rule = TAKES[name];
  if (!rule) return true;
  let index = 0;
  let before = rule.before ?? 0;
  while (index < words.length) {
    const text = list[words[index]].text;
    if (rule.arg?.test(text)) index += 2;
    else if (rule.flags?.test(text) || (name === 'env' && /^[A-Za-z_][A-Za-z0-9_]*=/.test(text))) index += 1;
    else if (before > 0) {
      // timeout's duration.
      before -= 1;
      index += 1;
    } else break;
  }
  return index >= words.length || commandAt(list, words[index], programs);
}

/** Whether the word at `list[at]`, run as a command (and what it runs in turn), is known. */
function commandAt(list, at, programs) {
  return known(list[at], programs) && wrappedKnown(list, at, programs);
}

function known(word, programs) {
  // A command whose name is made at run time cannot be told.
  if (word.expanded) return false;
  const name = word.text;
  if (BUILTINS.has(name) || programs.has(name)) return true;
  const dir = DIRS.find((prefix) => name.startsWith(prefix));
  return Boolean(dir) && programs.has(name.slice(dir.length));
}

/** `command` as words and operators (quotes kept to the words they make), or null for
 * what does not read (an open quote); `nested`: the word lists of the commands inside
 * `$( )` and backticks; `bare`: the command with quoted text blanked, for the bash-only
 * patterns. */
function tokens(command) {
  const list = [];
  const nested = [];
  let bare = '';
  const inside = (text) => {
    const words = tokens(text);
    if (!words) return false;
    nested.push(words.list, ...words.nested);
    bare += ` ${words.bare} `;
    return true;
  };
  let at = 0;
  let current = null;
  const flush = () => {
    if (current) list.push(current);
    current = null;
  };
  const word = () => (current ??= { text: '', expanded: false, quotedName: false });
  const operators = ['&&', '||', ';;', '|', '&', ';', '(', ')', '\n'];
  while (at < command.length) {
    const char = command[at];
    if (char === '\\') {
      word().text += command[at + 1] ?? '';
      bare += '  ';
      at += 2;
      continue;
    }
    if (char === "'") {
      const end = command.indexOf("'", at + 1);
      if (end < 0) return null;
      const inner = command.slice(at + 1, end);
      const target = word();
      if (!target.text.includes('=')) target.quotedName = true;
      target.text += inner;
      bare += ' '.repeat(end - at + 1);
      at = end + 1;
      continue;
    }
    if (char === '"') {
      let end = at + 1;
      while (end < command.length && command[end] !== '"') {
        if (command[end] === '\\') end += 1;
        end += 1;
      }
      if (end >= command.length) return null;
      const inner = command.slice(at + 1, end);
      const target = word();
      if (!target.text.includes('=')) target.quotedName = true;
      if (inner.includes('$') || inner.includes('`')) {
        target.expanded = true;
        // What runs inside a quoted $( ) or ` ` is a command too.
        const found = substitutions(inner);
        if (found === null || !found.every(inside)) return null;
      }
      target.text += inner;
      bare += ' '.repeat(end - at + 1);
      at = end + 1;
      continue;
    }
    // $'...' and $"..." are bash's (C escapes, translation): v86's to run.
    if (char === '$' && (command[at + 1] === "'" || command[at + 1] === '"')) return null;
    if (char === '$' && command.startsWith('((', at + 1)) {
      // Arithmetic, part of its word (what it substitutes runs as commands).
      const end = closing(command, at + 1);
      if (end < 0) return null;
      const found = substitutions(command.slice(at + 3, end - 1));
      if (found === null || !found.every(inside)) return null;
      word().expanded = true;
      word().text += command.slice(at, end + 1);
      bare += ' '.repeat(end + 1 - at);
      at = end + 1;
      continue;
    }
    if (char === '$' && command[at + 1] === '(' && command[at + 2] !== '(') {
      const end = closing(command, at + 1);
      if (end < 0 || !inside(command.slice(at + 2, end))) return null;
      word().expanded = true;
      at = end + 1;
      continue;
    }
    if (char === '`') {
      const end = command.indexOf('`', at + 1);
      if (end < 0 || !inside(command.slice(at + 1, end))) return null;
      word().expanded = true;
      at = end + 1;
      continue;
    }
    if (char === '$') {
      word().expanded = true;
      word().text += char;
      bare += char;
      at += 1;
      continue;
    }
    if (char === '#' && !current) {
      // A comment, to the end of its line.
      const end = command.indexOf('\n', at);
      at = end < 0 ? command.length : end;
      continue;
    }
    if (/[<>]/.test(char) || (/[0-9]/.test(char) && !current && /^[0-9]+[<>]/.test(command.slice(at)))) {
      // A redirection: [n]> >> < <> >& <& >| <<[-] WORD (a heredoc's body is skipped).
      flush();
      const match = /^[0-9]*(<<-?|>>|<>|>&|<&|>\||<|>)/.exec(command.slice(at));
      const operator = match[0];
      bare += operator;
      at += operator.length;
      while (command[at] === ' ' || command[at] === '\t') at += 1;
      if (operator.endsWith('<<') || operator.endsWith('<<-')) {
        const delimiter = /^(['"]?)([A-Za-z0-9_-]+)\1/.exec(command.slice(at));
        if (!delimiter) return null;
        at += delimiter[0].length;
        const lineEnd = command.indexOf('\n', at);
        if (lineEnd < 0) return null;
        const close = new RegExp(`\\n\\t*${delimiter[2]}(\\n|$)`).exec(command.slice(lineEnd));
        if (!close) return null;
        // An unquoted delimiter: the body's $( ) and backticks run.
        if (!delimiter[1]) {
          const found = substitutions(command.slice(lineEnd + 1, lineEnd + close.index));
          if (found === null || !found.every(inside)) return null;
        }
        list.push({ text: operator, redirect: true, carried: true });
        // The rest of the line holds commands; the body (to the delimiter) does not.
        const rest = tokens(command.slice(at, lineEnd));
        if (!rest) return null;
        list.push(...rest.list);
        nested.push(...rest.nested);
        bare += ` ${rest.bare} `;
        at = lineEnd + close.index + close[0].length;
        list.push({ text: '\n', operator: true });
        continue;
      }
      // The target follows (2>&1 carries it: a number or `-`).
      const carried = /^(&?[0-9]+|&-|-)(?=$|[\s;&|)])/.test(command.slice(at)) && operator.includes('&');
      list.push({ text: operator, redirect: true, carried: false });
      if (carried) {
        const target = /^(&?[0-9]+|&-|-)/.exec(command.slice(at))[0];
        list.push({ text: target });
        at += target.length;
      }
      continue;
    }
    const operator = operators.find((candidate) => command.startsWith(candidate, at));
    if (operator) {
      flush();
      list.push({ text: operator, operator: true });
      bare += operator;
      at += operator.length;
      continue;
    }
    if (char === ' ' || char === '\t') {
      flush();
      bare += char;
      at += 1;
      continue;
    }
    // A tilde prefix (~, ~/x, X=~/x, PATH=a:~/b): hush, the wasm Linux's sh, leaves it
    // as it is.
    if (char === '~' && (!current || (current.text.includes('=') && /[=:]$/.test(current.text)))) return null;
    word().text += char;
    bare += char;
    at += 1;
  }
  flush();
  return { list, nested, bare };
}

/** Where the `(` at `open` closes (quotes and nesting followed), or -1. */
function closing(text, open) {
  let depth = 0;
  for (let at = open; at < text.length; at++) {
    const char = text[at];
    if (char === '\\') at += 1;
    else if (char === "'") {
      at = text.indexOf("'", at + 1);
      if (at < 0) return -1;
    } else if (char === '"') {
      at += 1;
      while (at < text.length && text[at] !== '"') at += text[at] === '\\' ? 2 : 1;
    } else if (char === '(') depth += 1;
    else if (char === ')' && --depth === 0) return at;
  }
  return -1;
}

/** The commands inside `$( )` and backticks of a double-quoted string, or null. */
function substitutions(inner) {
  const found = [];
  for (let at = 0; at < inner.length; at++) {
    if (inner[at] === '\\') at += 1;
    else if (inner[at] === '$' && inner[at + 1] === '(' && inner[at + 2] !== '(') {
      const end = closing(inner, at + 1);
      if (end < 0) return null;
      found.push(inner.slice(at + 2, end));
      at = end;
    } else if (inner[at] === '`') {
      const end = inner.indexOf('`', at + 1);
      if (end < 0) return null;
      found.push(inner.slice(at + 1, end));
      at = end;
    }
  }
  return found;
}

/** The wasm Linux's programs (busybox applets) that reach the network. The wasm Linux has
 * none (no card, no name server: they fail), so in a thread with network on a command
 * naming one goes to v86. */
export const NETWORK_PROGRAMS = new Set([
  'wget', 'nc', 'telnet', 'ftpget', 'ftpput', 'ping', 'ping6', 'nslookup', 'traceroute', 'traceroute6',
  'tftp', 'whois', 'ntpd', 'udhcpc', 'httpd', 'ssl_client', 'sendmail', 'fetchmail', 'popmaildir', 'arping',
]);

/** Python (in a command, a heredoc or a script it runs) that may open a connection: a
 * module that connects, imported as such (`NETWORK_IMPORTS`); a call that does; a program
 * that does, named in quotes (for subprocess or os.system); pip. What reaches the network
 * otherwise fails loudly in the wasm Linux and runs again on v86 (`wantsV86`); these keep
 * a script that would catch that failure and carry on from running where its connections
 * cannot work. */
const NETWORK_IMPORTS = [
  /(^|[\n;"'])[ \t]*(import|from)[ \t]+(socket|ssl|socketserver|selectors|requests|httpx|aiohttp|urllib3|httplib2|websockets?|websocket|ftplib|smtplib|imaplib|poplib|nntplib|telnetlib|xmlrpc|http\.client|http\.server|urllib\.request|urllib\.error|urllib\.robotparser|paramiko|pycurl|grpc|boto3|botocore|openai|anthropic|yfinance|feedparser|tweepy|praw)\b/,
  /(^|[\n;"'])[ \t]*import[ \t]+[^\n#]*,[ \t]*(socket|ssl|requests|httpx|urllib3|ftplib|smtplib)\b/,
  /\bfrom[ \t]+urllib[ \t]+import[ \t]+[^\n#]*\b(request|error|robotparser)\b/,
  /\bfrom[ \t]+http[ \t]+import[ \t]+[^\n#]*\b(client|server)\b/,
  /\b(__import__|import_module)[ \t]*\([ \t]*['"](socket|ssl|urllib\.request|http\.client|requests|httpx|urllib3)\b/,
];
const NETWORK_CODE = [
  ...NETWORK_IMPORTS,
  /\b(urlopen|urlretrieve|open_connection|start_server|create_connection|sock_connect|getaddrinfo|gethostbyname)[ \t]*\(/,
  /['"](curl|wget|git|pip3?|npm|npx|yarn|pnpm|ssh|scp|sftp|rsync|nc|ncat|nslookup|dig|apk|ping|ftp|telnet)['"\s]/,
  /(^|\s)-m[ \t]+(pip|ensurepip)\b/,
];

/** At most this many workspace files, and bytes in all, are read to tell whether a
 * command needs v86 (`needsV86`); a command whose Python reaches further goes there. */
const SCAN_FILES = 256;
const SCAN_BYTES = 16 * 1024 * 1024;

/** Caches only the programs that made them read (Python's, pytest's, mypy's...), each in
 * the Linux it runs in: naming one does not tie a command to v86. */
const OWN_CACHES = /^(__pycache__|\.\w+_cache)$/;

/** Whether `text` names a directory of packages or caches a later command may use
 * (`.venv`, `node_modules`, `.cache`...: linux-workspace.mjs `ignored`), whose files are
 * never workspace files: each Linux keeps its own between commands. */
function namesKept(text) {
  return (text.match(/[\w.@+-]+(?:\/[\w.@+-]+)*/g) ?? [])
    .some((word) => word.split('/').some((part) => ignored(part) && !OWN_CACHES.test(part)));
}

/** Whether a command the wasm Linux could run must run on v86 all the same: it uses the
 * packages and caches each Linux keeps for itself (pip, npm and curl, which run on v86,
 * fill v86's: the wasm Linux's would not hold them, and removing or looking for them there
 * would not touch v86's), or, with `network` (its thread's on), it may use the network the
 * wasm Linux lacks. Told from its text (heredocs included) and the workspace's Python it
 * reaches (the scripts and modules it names, and what they import from the workspace,
 * matched by name wherever they are): a package or cache directory named, or
 * `NETWORK_CODE` (in what pip installed there, `NETWORK_IMPORTS` alone). `files`: the
 * workspace's ({path, size, object}); `read(object)`: a file's bytes. What cannot be read
 * or reaches too far counts. `cache` (object -> what a file holds) spares reading a file
 * again. */
export function needsV86(command, files, read, { network = true, cache = new Map() } = {}) {
  const connects = (text) => NETWORK_CODE.some((pattern) => pattern.test(text));
  if (namesKept(command) || (network && connects(command))) return true;
  const roots = [...scripts(command)];
  const own = imports(command).filter((one) => !one.dots);
  if (!roots.length && !own.length) return false;
  // The workspace's Python by path, and by file name; the directories pip installed
  // packages into (each holds a package's `*.dist-info`).
  const at = new Map();
  const byName = new Map();
  const installed = new Set();
  for (const file of files ?? []) {
    const info = /^(.+\/)[^/]+\.dist-info\//.exec(file.path);
    if (info) installed.add(info[1]);
    if (!file.path.endsWith('.py')) continue;
    at.set(file.path, file);
    const name = file.path.slice(file.path.lastIndexOf('/') + 1);
    if (!byName.has(name)) byName.set(name, []);
    byName.get(name).push(file);
  }
  /** The workspace's files a name (`a/b.py`, `b.py`) may mean. */
  const named = (name) => {
    const clean = name.replace(/^(\.\.?\/|\/workspace\/)+/, '');
    const found = byName.get(clean.slice(clean.lastIndexOf('/') + 1)) ?? [];
    return found.filter((file) => file.path === clean || file.path.endsWith(`/${clean}`));
  };
  /** The workspace's files a module imported from `dir` may be (relative: `dots`). */
  const modules = (dir, { dots, path }) => {
    const candidates = [`${path}.py`, `${path}/__init__.py`];
    if (!dots) return candidates.flatMap(named);
    let base = dir;
    for (let up = 1; up < dots; up++) base = base.replace(/[^/]*\/$/, '');
    return candidates.flatMap((candidate) => at.get(`${base}${candidate}`) ?? []);
  };
  const decoder = new TextDecoder();
  const seen = new Set();
  let bytes = 0;
  // The scripts it names, and the modules its own Python (`python3 - <<EOF`, `-c`) imports.
  const queue = [...roots.flatMap(named), ...own.flatMap((one) => modules('', one))];
  while (queue.length) {
    const file = queue.shift();
    if (seen.has(file.path)) continue;
    seen.add(file.path);
    bytes += file.size ?? 0;
    if (seen.size > SCAN_FILES || bytes > SCAN_BYTES) return true;
    let held = cache.get(file.object);
    if (!held) {
      let text;
      try {
        text = decoder.decode(read(file.object));
      } catch {
        return true;
      }
      held = {
        kept: namesKept(text),
        connects: connects(text),
        importsNetwork: NETWORK_IMPORTS.some((pattern) => pattern.test(text)),
        imports: imports(text),
      };
      if (cache.size >= 4096) cache.clear();
      cache.set(file.object, held);
    }
    // A package pip installed counts only for the modules it imports: its text names
    // programs, calls and directories in data and documentation too (html5lib's sanitizer
    // lists the `ping` attribute, pypdf's messages say `pip install`).
    const library = [...installed].some((dir) => file.path.startsWith(dir));
    if (library ? network && held.importsNetwork : held.kept || (network && held.connects)) return true;
    const dir = file.path.includes('/') ? file.path.slice(0, file.path.lastIndexOf('/') + 1) : '';
    for (const one of held.imports) queue.push(...modules(dir, one));
  }
  return false;
}

/** The Python files and modules a command names: `x.py` words (`./x.py`, `/workspace/x.py`
 * too), and `-m module`. */
function scripts(command) {
  const found = new Set();
  for (const match of command.matchAll(/(?:^|[\s'"=(])(\/?(?:[\w.-]+\/)*[\w.-]+\.py)(?=$|[\s'";|&)<>])/g)) found.add(match[1]);
  for (const match of command.matchAll(/(?:^|\s)-m[ \t]+([\w.]+)/g)) {
    const path = match[1].replace(/\./g, '/');
    found.add(`${path}.py`).add(`${path}/__init__.py`).add(`${path}/__main__.py`);
  }
  return found;
}

/** What a Python file imports, as paths (`a/b`), with the dots of a relative import. */
function imports(text) {
  const found = [];
  const add = (dots, module) => {
    const parts = module.split('.').filter(Boolean);
    // `import a.b` runs a/__init__.py too.
    for (let end = 1; end <= parts.length; end++) found.push({ dots, path: parts.slice(0, end).join('/') });
  };
  // At a line's start, after `;`, or a quote's (`python3 -c "import x"`); `from a import
  // (b, c)` over lines too.
  for (const match of text.matchAll(/(?:^|[;"'])[ \t]*(?:from[ \t]+(\.*)([\w.]*)[ \t]+import[ \t]+(\([^)]*\)|[^\n#;"']+)|import[ \t]+([^\n#;"']+))/gm)) {
    if (match[4] !== undefined) {
      for (const part of match[4].split(',')) add(0, part.trim().split(/\s+/)[0] ?? '');
      continue;
    }
    const dots = match[1].length;
    const module = match[2];
    if (module) add(dots, module);
    // `from pkg import mod`: mod may be a module of the package.
    for (const part of match[3].replace(/#[^\n]*/g, '').replace(/[()\\]/g, ' ').split(',')) {
      const name = part.trim().split(/\s+/)[0];
      if (/^\w+$/.test(name)) add(dots, module ? `${module}.${name}` : name);
    }
  }
  return found;
}

/** What the wasm Linux prints when a command reaches for the network it lacks (checked
 * on the image): a name it cannot look up, an address it cannot reach, HTTPS without its
 * Python's ssl. */
const NETWORK_FAILED = /\[Errno -3\] Try again|\[Errno 101\] Network unreachable|unknown url type: https|SSL module is not available|(^|\n)(wget|nc|telnet|ftpget|ftpput|whois|tftp|traceroute6?|ping6?): bad address |no servers could be reached/;

/** Whether a wasm Linux result says the command needs what it lacks (a program it does
 * not have, syntax its sh does not take, an option only GNU's tools take, ripgrep's
 * PCRE2, a Python module, pip, a program Python asked for, more memory, the network when
 * its thread has `network` on, or a module of the `compiled` code its workspace holds:
 * `.so` files, as pip puts in a directory of it, which its Python cannot load): run again
 * on v86, unless it made a Spindle call (journaled, maybe billed) or was stopped or timed
 * out (its result stands). */
export function wantsV86(result, { network = false, compiled = false } = {}) {
  if (result.called || result.success) return false;
  if (result.unavailable) return true;
  if (result.failure) return false;
  const output = result.output ?? '';
  if (network && NETWORK_FAILED.test(output)) return true;
  // `from . import _speedups` of a package whose module is compiled: ImportError (a
  // module imported by its full name says ModuleNotFoundError, below).
  if (compiled && /(^|\n)ImportError: /.test(output)) return true;
  if (result.code === 127 && /(^|\n)(sh: )?[^\n]*: (not found|applet not found)\s*(\n|$)/.test(output)) return true;
  // How busybox's sh and its applets (env, timeout, xargs…) say a program is not there.
  if (result.code === 127 && /can't execute '[^'\n]+': No such file or directory|(^|\n)xargs: [^:\n]+: No such file or directory/.test(output)) return true;
  // A program it may not run: a file a command saved (its workspace and scratch are data
  // only here) or one built for another machine.
  if (result.code === 126 && /(^|\n)(sh: )?[^\n]*: (Permission denied|Exec format error)\s*(\n|$)/.test(output)) return true;
  if (result.code === 126 && /can't execute '[^'\n]+': (Permission denied|Exec format error)/.test(output)) return true;
  if (/(^|\n)sh: [^\n]*syntax error/.test(output)) return true;
  // A busybox program refusing an option GNU's takes (grep -P, find -printf, date -d):
  // v86 has GNU coreutils, grep, sed and findutils.
  if (/(^|\n)[\w.-]+: (unrecognized|invalid) option\b/.test(output) || /(^|\n)BusyBox v[\d.]+[^\n]* multi-call binary/.test(output)) return true;
  // ripgrep's PCRE2 (`rg -P`): the wasm Linux's ripgrep is built without it, v86's with.
  if (/(^|\n)(rg: )?PCRE2 is not available in this build of ripgrep/.test(output)) return true;
  // GNU date reads dates busybox's does not (`yesterday`, `next friday`).
  if (/(^|\n)date: invalid date /.test(output)) return true;
  // Python: a module the wasm Linux's Python lacks (v86's may have it: pip installs
  // there), or a program its subprocess found nowhere or may not run (a saved one).
  if (/(^|\n)ModuleNotFoundError: No module named /.test(output)) return true;
  // `python3 -m MODULE` naming one it lacks (pip, pytest: what pip installs is v86's),
  // which Python says without a traceback.
  if (/(^|\n)\S*python[\d.]*: No module named /.test(output)) return true;
  // A venv made here could never get its pip (pip and what it installs are v86's): venv's
  // ensurepip fails, and v86 makes the venv afresh.
  if (/(^|\n)Error: Command '\[[^\n]*'-m', 'ensurepip'/.test(output)) return true;
  // Out of memory: each wasm Linux program has at most 256 MiB, v86's machine 512 MiB
  // (Python's MemoryError, a Rust program's failed allocation, busybox's).
  if (/(^|\n)MemoryError\b/.test(output) || /(^|\n)memory allocation of \d+ bytes failed/.test(output)
    || /(^|\n)[\w.-]+: out of memory\s*(\n|$)/.test(output)) return true;
  return /subprocess\.py", line \d+/.test(output)
    && /(^|\n)(FileNotFoundError: \[Errno 2\] No such file or directory|PermissionError: \[Errno 13\] Permission denied): /.test(output);
}
