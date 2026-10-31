// Pure text helpers: transcript search, line diff, Markdown export, labels.

// Start offsets of every case-insensitive occurrence of query in text.
export function matches(text, query) {
  const needle = String(query || '').toLowerCase();
  if (!needle) return [];
  const haystack = String(text || '').toLowerCase();
  const found = [];
  for (let at = haystack.indexOf(needle); at !== -1; at = haystack.indexOf(needle, at + needle.length)) found.push(at);
  return found;
}

const lines = text => {const all = String(text).split('\n'); if (all.at(-1) === '') all.pop(); return all;};

// Myers' O(ND) shortest edit script; null when the files differ in more than `limit` lines.
function edits(a, b, limit) {
  const max = a.length + b.length, offset = max + 1, v = new Int32Array(2 * max + 3), trace = [];
  let found = -1;
  for (let d = 0; d <= max && found < 0; d++) {
    if (d > limit) return null;
    for (let k = -d; k <= d; k += 2) {
      let x = k === -d || (k !== d && v[offset + k - 1] < v[offset + k + 1]) ? v[offset + k + 1] : v[offset + k - 1] + 1;
      let y = x - k;
      while (x < a.length && y < b.length && a[x] === b[y]) {x++; y++;}
      v[offset + k] = x;
      if (x >= a.length && y >= b.length) {found = d; break;}
    }
    trace.push(v.slice(offset - d, offset + d + 1));
  }
  const out = [];
  let x = a.length, y = b.length;
  for (let d = found; d > 0; d--) {
    const previous = trace[d - 1], at = k => previous[k + d - 1], k = x - y;
    const down = k === -d || (k !== d && at(k - 1) < at(k + 1));
    const fromX = at(down ? k + 1 : k - 1), fromY = fromX - (down ? k + 1 : k - 1);
    while (x > fromX && y > fromY) out.push([' ', a[--x]]), y--;
    if (down) out.push(['+', b[--y]]); else out.push(['-', a[--x]]);
  }
  while (x > 0 && y > 0) out.push([' ', a[--x]]), y--;
  return out.reverse();
}

// Unified diff as [kind, text] rows: '@' hunk headers, ' ' context, '-' removed, '+' added.
// [] when the texts are equal; null when the change is too large to show.
export function unified(before, after, context = 3, limit = 2000) {
  const a = lines(before), b = lines(after);
  if (a.length + b.length > 40000) return null;
  const script = edits(a, b, limit);
  if (!script) return null;
  const changed = script.map((row, i) => row[0] !== ' ' ? i : -1).filter(i => i >= 0);
  if (!changed.length) return [];
  const out = [];
  let index = 0;
  while (index < changed.length) {
    const start = Math.max(0, changed[index] - context);
    let end = changed[index];
    while (index + 1 < changed.length && changed[index + 1] - end <= 2 * context) end = changed[++index];
    index++;
    end = Math.min(script.length - 1, end + context);
    let oldLine = 1, newLine = 1;
    for (let i = 0; i < start; i++) {if (script[i][0] !== '+') oldLine++; if (script[i][0] !== '-') newLine++;}
    const rows = script.slice(start, end + 1);
    const oldCount = rows.filter(r => r[0] !== '+').length, newCount = rows.filter(r => r[0] !== '-').length;
    out.push(['@', `@@ -${oldCount ? oldLine : oldLine - 1},${oldCount} +${newCount ? newLine : newLine - 1},${newCount} @@`], ...rows);
  }
  return out;
}

// A fence longer than any backtick run inside the text.
const fence = text => '`'.repeat(Math.max(3, ...[...String(text).matchAll(/`+/g)].map(m => m[0].length + 1)));
const block = (text, info = '') => {const f = fence(text); return `${f}${info}\n${text}\n${f}`;};

// The conversation of a snapshot as Markdown: messages, tool calls and recorded results.
export function transcript(data, title = 'Spindle thread') {
  const out = [`# ${title}`, '', `r${data.revision} of r${data.head}${data.provider ? ` · ${data.provider}/${data.model}` : ''} · exported ${new Date().toISOString()}`];
  for (const message of data.messages || []) {
    for (const part of message.content || []) {
      if (part.type === 'text' && part.text.trim()) out.push('', `## ${message.role === 'user' ? 'You' : 'Spindle'}`, '', part.text);
      if (part.type === 'tool_use') {
        const input = part.input || {};
        const shown = typeof input.command === 'string' ? input.command : typeof input.code === 'string' ? input.code : JSON.stringify(input, null, 2);
        out.push('', `### Tool call: ${part.name}`, '', block(shown, part.name === 'bash' ? 'sh' : ''));
      }
      if (part.type === 'tool_result') {
        const content = typeof part.content === 'string' ? part.content : JSON.stringify(part.content, null, 2);
        out.push('', `### Result${part.is_error ? ' (failed)' : ''}`, '', block(content || ''));
      }
    }
  }
  return out.join('\n') + '\n';
}

export function ago(ms, now = Date.now()) {
  if (!ms) return '';
  const seconds = Math.max(0, Math.round((now - ms) / 1000));
  if (seconds < 60) return 'just now';
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
  return `${Math.floor(seconds / 86400)}d ago`;
}

export function size(bytes) {
  if (bytes === null || bytes === undefined) return 'unknown';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let value = bytes, unit = 0;
  while (value >= 1024 && unit < units.length - 1) {value /= 1024; unit++;}
  return `${unit ? value.toFixed(1) : value} ${units[unit]}`;
}

export function duration(seconds) {
  const d = Math.floor(seconds / 86400), h = Math.floor(seconds % 86400 / 3600), m = Math.floor(seconds % 3600 / 60);
  return d ? `${d}d ${h}h` : h ? `${h}h ${m}m` : `${m}m`;
}

// 1234 -> 1.2K, 1234567 -> 1.2M.
export function tokens(n) {
  const value = Number(n) || 0;
  if (value < 1000) return String(value);
  if (value < 1e6) return `${(value / 1e3).toFixed(value < 1e4 ? 1 : 0)}K`;
  return `${(value / 1e6).toFixed(value < 1e7 ? 1 : 0)}M`;
}

// Dollars: four decimals under a cent, three under a dollar, else cents.
export function money(usd) {
  const value = Number(usd) || 0;
  return `$${value.toFixed(value > 0 && value < 0.01 ? 4 : value < 1 ? 3 : 2)}`;
}

// A thread's model usage: calls, tokens, cost when its models have prices, and what
// no price covers. `short` is the header form: cost when priced, `subscription` for a
// subscription login's calls, else total tokens.
export function usageLine(usage, short = false) {
  if (!usage?.calls) return '';
  if (short) return usage.priced ? money(usage.usd) : usage.subscription ? 'subscription' : `${tokens(usage.input + usage.output)} tok`;
  const notes = [usage.subscription ? `${usage.subscription} subscription` : '', usage.unpriced ? `${usage.unpriced} unpriced` : '', usage.unknown ? `${usage.unknown} may have been billed` : ''];
  return [`${usage.calls} call${usage.calls === 1 ? '' : 's'}`, `${tokens(usage.input)} in`, `${tokens(usage.output)} out`, usage.cache_read ? `${tokens(usage.cache_read)} cached` : '', usage.priced ? money(usage.usd) : '', ...notes].filter(Boolean).join(' · ');
}

// What one turn cost (its replies, nested calls and compaction), for a muted label.
// While a run is busy its request in flight has no outcome yet, so none is called
// uncertain.
export function turnCost(cost, running = false) {
  const unknown = running ? 0 : cost?.unknown || 0;
  if (!cost || !(cost.calls || unknown || cost.refused)) return '';
  const paid = cost.priced ? money(cost.usd) : cost.subscription ? 'subscription' : cost.calls ? 'unpriced' : '';
  return [paid, cost.calls ? `${cost.calls} call${cost.calls === 1 ? '' : 's'}` : '', unknown ? `${unknown} may have been billed` : '', cost.refused ? `${cost.refused} refused` : ''].filter(Boolean).join(' · ');
}

// Message text split at Markdown code fences, in order: {text} prose and {code, lang}
// blocks. An unclosed fence (a reply still being written) runs to the end.
export function blocks(text) {
  const parts = [];
  let prose = [], code = null;
  const flush = () => {const value = prose.join('\n').replace(/^\n+|\n+$/g, ''); if (value.trim()) parts.push({text: value}); prose = [];};
  for (const line of String(text).split('\n')) {
    const fence = line.match(/^ {0,3}(`{3,}|~{3,})(.*)$/);
    if (code) {
      const closing = fence && fence[1][0] === code.mark[0] && fence[1].length >= code.mark.length && !fence[2].trim();
      if (closing) {parts.push({code: code.lines.join('\n').replace(/\n+$/, ''), lang: code.lang}); code = null;}
      else code.lines.push(line);
    } else if (fence && !(fence[1][0] === '`' && fence[2].includes('`'))) {
      flush(); code = {mark: fence[1], lang: fence[2].trim().split(/\s/)[0], lines: []};
    } else prose.push(line);
  }
  if (code) parts.push({code: code.lines.join('\n'), lang: code.lang});
  flush();
  return parts;
}

// A thread name from its first message: the first non-blank line, at most 60 characters,
// cut at a word boundary when one is near.
export function titleFrom(text) {
  const line = (String(text).split('\n').find(l => l.trim()) || '').replace(/\s+/g, ' ').trim();
  if (line.length <= 60) return line;
  const cut = line.slice(0, 59), space = cut.lastIndexOf(' ');
  return `${space > 40 ? cut.slice(0, space) : cut}…`;
}

// Plain text with every http(s) URL as a separate part, so a page can link them
// without parsing markup: [{text}, {url}, ...].
export function linkParts(text) {
  const parts = [];
  let last = 0;
  for (const match of String(text).matchAll(/https?:\/\/[^\s<>"']+[^\s<>"'.,;:!?)]/g)) {
    if (match.index > last) parts.push({text: text.slice(last, match.index)});
    parts.push({url: match[0]});
    last = match.index + match[0].length;
  }
  if (last < String(text).length) parts.push({text: String(text).slice(last)});
  return parts;
}

// Markdown as a small tree the page turns into elements (never HTML): blocks
// {type: paragraph|heading|code|list|quote|rule|table} with inline runs
// {type: text|code|strong|em|del|link}. Links keep only http(s) targets.
const FENCE = /^ {0,3}(`{3,}|~{3,})(.*)$/;
const BULLET = /^( *)([-*+]|\d{1,9}[.)])[ \t]+(.*)$/;
const RULE = /^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$/;
const HEADING = /^ {0,3}(#{1,6})[ \t]+(.*?)(?:[ \t]+#+)?[ \t]*$/;
const DELIMITER = /^[ \t]*\|?[ \t]*:?-+:?[ \t]*(?:\|[ \t]*:?-+:?[ \t]*)*\|?[ \t]*$/;
const tableStart = (line, next) => line.includes('|') && next !== undefined && next.includes('-') && DELIMITER.test(next);
const opensFence = line => {const m = line.match(FENCE); return Boolean(m) && !(m[1][0] === '`' && m[2].includes('`'));};
function interrupts(line, next) {
  if (!line.trim()) return true;
  if (opensFence(line) || HEADING.test(line) || RULE.test(line) || /^ {0,3}>/.test(line) || tableStart(line, next)) return true;
  const bullet = line.match(BULLET);
  return Boolean(bullet) && bullet[1].length <= 3 && (!/\d/.test(bullet[2]) || parseInt(bullet[2], 10) === 1);
}
function cells(row) {
  let text = row.trim();
  if (text.startsWith('|')) text = text.slice(1);
  if (text.endsWith('|') && !text.endsWith('\\|')) text = text.slice(0, -1);
  return text.split(/(?<!\\)\|/).map(cell => cell.trim().replace(/\\\|/g, '|'));
}
function parseBlocks(lines, depth) {
  const out = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) {i++; continue;}
    let m = line.match(FENCE);
    if (m && opensFence(line)) {
      const mark = m[1], lang = m[2].trim().split(/\s/)[0], body = [];
      for (i++; i < lines.length; i++) {
        const close = lines[i].match(/^ {0,3}(`{3,}|~{3,})[ \t]*$/);
        if (close && close[1][0] === mark[0] && close[1].length >= mark.length) {i++; break;}
        body.push(lines[i]);
      }
      out.push({type: 'code', lang, code: body.join('\n')});
      continue;
    }
    if ((m = line.match(HEADING))) {out.push({type: 'heading', level: m[1].length, inline: inline(m[2])}); i++; continue;}
    if (RULE.test(line)) {out.push({type: 'rule'}); i++; continue;}
    if (/^ {0,3}>/.test(line)) {
      const body = [];
      while (i < lines.length && /^ {0,3}>/.test(lines[i])) body.push(lines[i++].replace(/^ {0,3}> ?/, ''));
      out.push({type: 'quote', blocks: depth < 8 ? parseBlocks(body, depth + 1) : [{type: 'paragraph', inline: [{type: 'text', text: body.join('\n')}]}]});
      continue;
    }
    if (tableStart(line, lines[i + 1])) {
      const head = cells(line), align = cells(lines[i + 1]).map(c => c.startsWith(':') && c.endsWith(':') ? 'center' : c.endsWith(':') ? 'right' : c.startsWith(':') ? 'left' : '');
      const rows = [];
      for (i += 2; i < lines.length && lines[i].trim() && lines[i].includes('|'); i++) rows.push(cells(lines[i]));
      out.push({type: 'table', align, head: head.map(inline), rows: rows.map(row => head.map((_, k) => inline(row[k] || '')))});
      continue;
    }
    m = line.match(BULLET);
    if (m && m[1].length <= 3 && depth < 8) {
      const indent = m[1].length, ordered = /\d/.test(m[2]), items = [];
      while (i < lines.length) {
        const item = lines[i].match(BULLET);
        if (item && item[1].length === indent && /\d/.test(item[2]) === ordered) {
          items.push({width: item[0].length - item[3].length, lines: [item[3]]}); i++; continue;
        }
        const current = items.at(-1), text = lines[i];
        if (!text.trim()) {
          // A blank line continues the list only before another item or indented content.
          let next = i + 1; while (next < lines.length && !lines[next].trim()) next++;
          const after = lines[next], bullet = after?.match(BULLET);
          if (after !== undefined && ((bullet && bullet[1].length === indent && /\d/.test(bullet[2]) === ordered) || /^ +/.test(after) && after.match(/^ */)[0].length > indent)) {current.lines.push(''); i++; continue;}
          break;
        }
        const lead = text.match(/^ */)[0].length;
        if (lead > indent) {current.lines.push(text.slice(Math.min(lead, current.width))); i++; continue;}
        // A lazy continuation line of the item's paragraph.
        if (current.lines.at(-1)?.trim() && !interrupts(text, lines[i + 1])) {current.lines.push(text); i++; continue;}
        break;
      }
      out.push({type: 'list', ordered, start: ordered ? parseInt(m[2], 10) : null, items: items.map(item => parseBlocks(item.lines, depth + 1))});
      continue;
    }
    const paragraph = [line];
    for (i++; i < lines.length && !interrupts(lines[i], lines[i + 1]); i++) paragraph.push(lines[i]);
    out.push({type: 'paragraph', inline: inline(paragraph.join('\n'))});
  }
  return out;
}
export function markdown(text) {
  return parseBlocks(String(text ?? '').replace(/\r\n?/g, '\n').split('\n'), 0);
}
// The end of an emphasis run that opens at `from`, or -1.
function closing(text, from, marker) {
  const char = marker[0];
  for (let j = from; j <= text.length - marker.length; j++) {
    if (text[j] === '\\') {j++; continue;}
    if (text[j] === '`') {const run = text.slice(j).match(/^`+/)[0], end = text.indexOf(run, j + run.length); if (end > -1) j = end + run.length - 1; continue;}
    if (!text.startsWith(marker, j) || j === from || /\s/.test(text[j - 1])) continue;
    if (marker.length === 1 && text[j + 1] === char) {j++; continue;}
    if (char === '_' && /[\p{L}\p{N}]/u.test(text[j + marker.length] || '')) continue;
    return j;
  }
  return -1;
}
export function inline(text) {
  text = String(text ?? '');
  // Long runs stay literal: emphasis matching is quadratic in the worst case.
  if (text.length > 20000) return [{type: 'text', text}];
  const out = [];
  let buffer = '';
  const push = node => {if (buffer) out.push({type: 'text', text: buffer}); buffer = ''; out.push(node);};
  for (let i = 0; i < text.length;) {
    const c = text[i];
    if (c === '\\' && /[\\`*_{}[\]()#+\-.!|~>]/.test(text[i + 1] || '')) {buffer += text[i + 1]; i += 2; continue;}
    if (c === '`') {
      const run = text.slice(i).match(/^`+/)[0];
      let end = text.indexOf(run, i + run.length);
      while (end > -1 && text[end + run.length] === '`') end = text.indexOf(run, end + run.length + 1);
      if (end > -1) {const code = text.slice(i + run.length, end); push({type: 'code', text: /^ .* $/.test(code) ? code.slice(1, -1) : code}); i = end + run.length; continue;}
      buffer += run; i += run.length; continue;
    }
    if (c === '*' || c === '_') {
      const marker = text[i + 1] === c ? c + c : c, after = text[i + marker.length] || '', before = text[i - 1] || ' ';
      if (after && !/\s/.test(after) && !(c === '_' && /[\p{L}\p{N}]/u.test(before))) {
        const end = closing(text, i + marker.length, marker);
        if (end > -1) {push({type: marker.length === 2 ? 'strong' : 'em', children: inline(text.slice(i + marker.length, end))}); i = end + marker.length; continue;}
      }
      buffer += marker; i += marker.length; continue;
    }
    if (c === '~' && text[i + 1] === '~') {
      const end = text.indexOf('~~', i + 2);
      if (end > i + 2) {push({type: 'del', children: inline(text.slice(i + 2, end))}); i = end + 2; continue;}
    }
    if (c === '[') {
      const link = text.slice(i).match(/^\[([^\]\n]+)\]\((https?:\/\/[^\s)]+)\)/);
      if (link) {push({type: 'link', href: link[2], children: inline(link[1])}); i += link[0].length; continue;}
    }
    if (c === 'h' && !/[\p{L}\p{N}]/u.test(text[i - 1] || '')) {
      const url = text.slice(i).match(/^https?:\/\/[^\s<>"'`]+[^\s<>"'`.,;:!?)\]]/);
      if (url) {push({type: 'link', href: url[0], children: [{type: 'text', text: url[0]}]}); i += url[0].length; continue;}
    }
    buffer += c; i++;
  }
  if (buffer) out.push({type: 'text', text: buffer});
  return out;
}

// Workspace paths named in text, as parts the page can make tappable: [{text}, {path,
// text}, ...]. Only files the revision has count (`known` holds their paths); a leading
// "./" or the sandbox root "/workspace/" is allowed, a ":12:3" line suffix stays text.
export function pathParts(text, known) {
  text = String(text ?? '');
  if (!known?.size) return [{text}];
  const parts = [];
  let last = 0;
  for (const match of text.matchAll(/(?:\/workspace\/|\.\/)?[\p{L}\p{N}_@~+-][\p{L}\p{N}_.@~+-]*(?:\/[\p{L}\p{N}_.@~+-]+)*/gu)) {
    let token = match[0];
    while (token.endsWith('.')) token = token.slice(0, -1);
    const path = token.replace(/^\/workspace\/|^\.\//, '');
    if (!/[./]/.test(path) || !known.has(path)) continue;
    // Part of a URL or a longer path: left as text.
    const before = text[match.index - 1] || '';
    if (before === '/' || before === ':' && text.slice(Math.max(0, match.index - 8), match.index).includes('//')) continue;
    if (match.index > last) parts.push({text: text.slice(last, match.index)});
    parts.push({path, text: token});
    last = match.index + token.length;
  }
  if (last < text.length) parts.push({text: text.slice(last)});
  return parts.length ? parts : [{text}];
}

// Files as folders a list opens one at a time: each folder {name, path, paths, files,
// folders, count, marks}, `count` the files beneath it and `marks` how many of those
// `mark(file)` names each way, in the order new, edited, changed, deleted ({new: 2}; ''
// names none). Files come before folders, each in name order (numbers by value:
// file2 before file10), and a folder holding nothing but one folder shares its row (name
// "lib/python3.14", path the inner one's; `paths` every folder the row stands for, outer
// first). `order` is every file in that order, open or shut.
export function fileTree(files, mark = () => '') {
  const compare = new Intl.Collator(undefined, {numeric: true}).compare;
  const ranks = ['new', 'edited', 'changed', 'deleted'];
  const rank = kind => (ranks.includes(kind) ? ranks.indexOf(kind) : ranks.length);
  const folder = (name, path) => ({name, path, paths: [path], files: [], folders: [], count: 0, marks: {}, named: new Map()});
  const root = folder('', ''), order = [];
  for (const file of files) {
    const parts = file.path.split('/');
    let node = root;
    for (let i = 0; i < parts.length - 1; i++) {
      let next = node.named.get(parts[i]);
      if (!next) node.named.set(parts[i], next = folder(parts[i], node.path ? `${node.path}/${parts[i]}` : parts[i]));
      node = next;
    }
    node.files.push(file);
  }
  const finish = node => {
    node.files.sort((a, b) => compare(a.path, b.path));
    for (const file of node.files) {
      order.push(file);
      const kind = mark(file);
      if (kind) node.marks[kind] = (node.marks[kind] || 0) + 1;
    }
    node.count = node.files.length;
    node.folders = [...node.named.values()].sort((a, b) => compare(a.name, b.name)).map(inner => {
      finish(inner);
      node.count += inner.count;
      for (const [kind, n] of Object.entries(inner.marks)) node.marks[kind] = (node.marks[kind] || 0) + n;
      while (!inner.files.length && inner.folders.length === 1) {
        const [only] = inner.folders;
        inner = {...only, name: `${inner.name}/${only.name}`, paths: [...inner.paths, ...only.paths]};
      }
      return inner;
    });
    node.marks = Object.fromEntries(Object.entries(node.marks).sort(([a], [b]) => rank(a) - rank(b)));
    delete node.named;
    return node;
  };
  return {...finish(root), order};
}

// One quiet line for a collapsed tool card: what came back, without opening it.
export function resultSummary(call, result) {
  if (!result) return '';
  const content = typeof result.content === 'string' ? result.content : JSON.stringify(result.content ?? '');
  const rows = content.split('\n').map(line => line.trim()).filter(Boolean);
  const count = n => `${n} line${n === 1 ? '' : 's'}`;
  const input = call?.input || {};
  if (result.is_error) return (rows[0] || 'failed').slice(0, 160);
  if (call?.name === 'edit_file' && typeof input.old_string === 'string' && typeof input.new_string === 'string') {
    const diff = unified(`${input.old_string}\n`, `${input.new_string}\n`, 0) || [];
    return `+${diff.filter(r => r[0] === '+').length} −${diff.filter(r => r[0] === '-').length}`;
  }
  if (call?.name === 'write_file' && typeof input.content === 'string') return count(lines(input.content).length);
  if (call?.name === 'read_file') return count(lines(content).length);
  if (!rows.length) return 'no output';
  return (['bash', 'run_code'].includes(call?.name) ? rows.at(-1) : rows[0]).slice(0, 160);
}

// Reasoning levels as the picker names them; "default" follows the model.
export const EFFORT_NAMES = {default: 'auto', off: 'off', low: 'low', medium: 'med', high: 'high', xhigh: 'xhigh', max: 'max'};
// Thread settings as one short line: "glm-5.3 · max · network on · budget $5.00".
export function settingsLine(fields) {
  if (!fields || typeof fields !== 'object') return '';
  const out = [];
  if (fields.model) out.push(fields.model); else if (fields.provider) out.push(`${fields.provider} default`);
  if (fields.effort) out.push(fields.effort === 'default' ? 'auto effort' : fields.effort);
  if (typeof fields.network === 'boolean') out.push(`network ${fields.network ? 'on' : 'off'}`);
  if (typeof fields.toolchain === 'boolean') out.push(`toolchain ${fields.toolchain ? 'on' : 'off'}`);
  if (fields.compaction) out.push(fields.compaction === 'digest' ? 'digest compaction' : 'model compaction');
  if ('max_usd' in fields) out.push(fields.max_usd === null ? 'no budget' : `budget ${money(fields.max_usd)}`);
  if (fields.name) out.push(`named “${fields.name}”`);
  return out.join(' · ');
}
// "$1.40 / $4.40" per million tokens, or '' without a price.
export function priceLine(price) {
  if (!price || typeof price.input !== 'number' || typeof price.output !== 'number') return '';
  const dollars = value => `$${value < 1 ? value.toFixed(value < 0.1 ? 3 : 2).replace(/0$/, '') : value.toFixed(2)}`;
  return `${dollars(price.input)} / ${dollars(price.output)}`;
}
