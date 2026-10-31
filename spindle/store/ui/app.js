import {Terminal} from './terminal.js';
import {intent} from './commands.js';
import {matches, unified, transcript, ago, size, duration, tokens, usageLine, money, turnCost, linkParts, blocks, titleFrom, markdown, pathParts, resultSummary, settingsLine, priceLine, EFFORT_NAMES, fileTree} from './text.js';
const $ = id => document.getElementById(id);
const el = (tag, cls, text) => {const node = document.createElement(tag); if(cls) node.className = cls; if(text !== undefined) node.textContent = text; return node;};
const decode = text => Uint8Array.from(atob(text), c => c.charCodeAt(0));
// Line icons in the text's colour, as inline SVG (the page's CSP loads no image data):
// paths on a 24-unit grid; `w` the stroke width, `fill` a filled shape.
const ring = (x, y, r) => `M${x - r} ${y}a${r} ${r} 0 1 0 ${2 * r} 0a${r} ${r} 0 1 0 ${-2 * r} 0`;
const ICONS = {
  check: {d: ['M5 12.5l4.5 4.5L19 7.5'], w: 2.4}, x: {d: ['M6 6l12 12M18 6L6 18'], w: 2.2}, spinner: {d: ['M12 3a9 9 0 1 0 9 9'], w: 2.6},
  next: {d: ['M9 6l6 6-6 6'], w: 2.2}, down: {d: ['M6 9l6 6 6-6'], w: 2.2}, warning: {d: ['M12 4l9 16H3z', 'M12 10v4M12 17v.5'], w: 2},
  command: {d: ['M5 7l4 5-4 5M12 17h7'], w: 2}, code: {d: ['M9 7l-5 5 5 5M15 7l5 5-5 5'], w: 2}, file: {d: ['M14 3H7a1 1 0 0 0-1 1v16a1 1 0 0 0 1 1h10a1 1 0 0 0 1-1V7z', 'M14 3v4h4']},
  job: {d: ['M5 12h4l2-5 3 10 2-5h3'], w: 2}, show: {d: ['M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z', ring(12, 12, 3)]}, tool: {d: ['M14.5 6.5a4 4 0 0 0-5 5L4 17l3 3 5.5-5.5a4 4 0 0 0 5-5l-2.5 2.5-2.5-.5-.5-2.5z']},
  more: {fill: true, d: [ring(5, 12, 1.6), ring(12, 12, 1.6), ring(19, 12, 1.6)]}, trash: {d: ['M5 7h14M10 7V4h4v3M7 7l1 13h8l1-13']},
  signin: {d: ['M14 4h5v16h-5M4 12h10M10 8l4 4-4 4'], w: 2}, signout: {d: ['M14 4h5v16h-5M10 8l-4 4 4 4M6 12h10'], w: 2}, retry: {d: ['M20 12a8 8 0 1 1-2.3-5.7', 'M20 4v4h-4'], w: 2.2},
};
function icon(name, size = 16) {
  const spec = ICONS[name] || {}, ns = 'http://www.w3.org/2000/svg', svg = document.createElementNS(ns, 'svg');
  for(const [key, value] of Object.entries({viewBox: '0 0 24 24', width: size, height: size, 'aria-hidden': 'true', class: `icon icon-${name}`})) svg.setAttribute(key, String(value));
  if(spec.fill) svg.setAttribute('fill', 'currentColor');
  else for(const [key, value] of Object.entries({fill: 'none', stroke: 'currentColor', 'stroke-width': String(spec.w || 1.8), 'stroke-linecap': 'round', 'stroke-linejoin': 'round'})) svg.setAttribute(key, value);
  for(const d of spec.d || []) {const path = document.createElementNS(ns, 'path'); path.setAttribute('d', d); svg.append(path);}
  return svg;
}
let sessions = [], selected = null, snapshot = null, source = null, cursor = '', historical = false, following = true, activeTool = null, modelText = null, refreshTimer = 0, generation = 0, submitting = false, stopping = false;
// Catch-up: events at or below this sequence happened before this screen subscribed.
let catchup = 0, buffered = null, activity = {label:'', since:0};
// Search: committed blocks of the rendered thread and the current hit.
let searchable = [], found = [], foundAt = 0, lastError = '', probed = 0;
// Files uploaded for the open thread's next message: {name, path, size, digest?, error?}.
let attached = [];
// The Budget field as the thread menu filled it: saving sends max_usd only once it changed.
let budgetShown = '';
// The host's model catalog (GET /api/models, or one model per provider from the thread
// list on an older host), the open model picker, and the workspace paths of the rendered
// revision (mentions of them become links to the file viewer).
let catalog = null, picking = null, knownFiles = new Set(), lastThread = null;
// What the operator holds open (attention) and the last request's context size (gauge).
let attention = null, liveContext = null;
// Messages waiting for the running turn ({messages, now}), as the host last said.
let queued = null;
const ALL_EFFORTS = ['default', 'off', 'low', 'medium', 'high', 'xhigh', 'max'];
const drafts = {get: key => {try{return localStorage.getItem(key) || '';}catch{return '';}}, set:(key,value) => {try{localStorage.setItem(key,value);}catch{}}};
// A short notice over the page; a tap dismisses it. An open modal dialog makes the rest
// of the page inert (unseen, unannounced, untappable), so the notice joins the dialog on
// top: a confirmation over another sheet, else the last open one.
// `action`: [label, run], a button in the notice (Undo, say), which then sits above the
// composer, in a thumb's reach; `ms`: how long it stays.
function toast(text, kind = 'error', {action = null, ms = null} = {}) {
  const node = $('toast'), open = [...document.querySelectorAll('dialog[open]')];
  const host = open.find(d => d.id === 'confirm-dialog') || open.find(d => d.id === 'pair-dialog') || open.at(-1) || document.body;
  if(node.parentNode !== host) host.append(node);
  node.textContent = text; node.dataset.kind = kind; node.hidden = false;
  node.classList.toggle('actionable', Boolean(action));
  if(action) {
    const button = el('button', 'toast-action', action[0]); button.type = 'button';
    button.onclick = event => {event?.stopPropagation?.(); hideToast(); action[1]();};
    node.append(button);
    node.style.setProperty?.('--dock-h', `${$('dock').offsetHeight || 0}px`);
  }
  clearTimeout(toast.timer); toast.timer = setTimeout(hideToast, ms ?? (kind === 'error' ? 12000 : 5000));
}
function hideToast() {$('toast').hidden = true;}
const note = text => toast(text, 'info');
// An expired sign-in already shows its banner; a toast would cover it and its button.
// A command's refusal was already reported with its context (command()): not again.
const fail = error => {if(error?.status !== 0 && !error?.reported) toast(error.message);};
// The header dot; its words only show when something is wrong.
function connection(text) {$('connection').textContent = text; $('connection').dataset.ok = String(text === 'Connected');}
// One persistent notice line under the header, most important first.
const banners = new Map(), BANNERS = ['signin','gone','stream','pending','update'];
function banner(key, text, actions = []) {
  if(!text && !banners.has(key)) return;
  if(text) banners.set(key,{text,actions}); else banners.delete(key);
  const top = BANNERS.find(k => banners.has(k));
  $('banner').hidden = !top; if(!top) return;
  $('banner-text').textContent = banners.get(top).text;
  $('banner-actions').replaceChildren(...banners.get(top).actions.map(([label, action, cls]) => {const button = el('button', cls || '', label); button.type = 'button'; button.onclick = action; return button;}));
}
// Cloudflare Access answers an expired sign-in with a redirect to its login page; fetches
// would otherwise fail as opaque CORS errors that look like a lost connection.
function signin() {connection('Signed out'); banner('signin','Your Cloudflare sign-in expired.',[['Sign in again',() => location.reload(),'primary']]);}
async function api(path, data, raw = false) {
  const response = await fetch(path, data === undefined ? {cache:'no-store',redirect:'manual'} : {method:'POST',redirect:'manual',headers:{'Content-Type':raw ? 'application/octet-stream' : 'application/json','X-Spindle':'1'},body:raw ? data : JSON.stringify(data)});
  if(response.type === 'opaqueredirect' || response.status === 0) {signin(); const error = Error('Sign in again to continue.'); error.status = 0; throw error;}
  const text = await response.text();
  let result = {}, json = true; try {result = JSON.parse(text);} catch {json = false; result = {error: text.trim().slice(0, 200)};}
  if(!json && [401,403].includes(response.status)) {signin(); const error = Error('Sign in again to continue.'); error.status = 0; throw error;}
  banner('signin');
  if(response.status === 401) {if(!$('pair-dialog').open) $('pair-dialog').showModal(); const error = Error('Pair this screen to continue.'); error.status = 401; throw error;}
  if(!response.ok) {const error = Error(result.error || `Request failed (${response.status})`); error.status = response.status; throw error;}
  return result;
}
// An empty composer stays one line: a long placeholder must not grow it.
function fitPrompt() {const p = $('prompt'); p.style.height = 'auto'; const h = p.value ? p.scrollHeight : 0; p.style.height = h ? `${Math.min(140,h)}px` : ''; p.classList.toggle('scrollable', h > 140);}
// Following the live end jumps (never animates): an animated scroll reports positions
// short of the end, which would read as the reader scrolling away.
function scroll() {if(following) requestAnimationFrame(() => {$('main').scrollTop = $('main').scrollHeight;});}
function setActivity(label) {activity = {label, since: Date.now()}; tick();}
function tick() {
  if(!activity.label) return;
  const seconds = Math.round((Date.now() - activity.since) / 1000);
  $('phase-text').textContent = seconds >= 2 ? `${activity.label} · ${seconds}s` : activity.label;
}
setInterval(tick, 1000);
// Committed model text renders as Markdown (built as elements, never parsed as HTML);
// a person's text keeps its literal form with code fences as blocks; a preview still
// being written stays literal text. Workspace paths in either open the file viewer.
function message(parent, role, text, preview = false, label = '') {
  const article = el('article', `message ${role}`); if(preview) article.classList.add('preview');
  const head = el('span','role-label',label || (role === 'user' ? 'YOU' : preview ? 'SPINDLE · LIVE PREVIEW' : 'SPINDLE'));
  article.append(head);
  const body = el('div');
  if(preview) body.textContent = text;
  else if(role === 'assistant') {body.className = 'md'; mdNodes(body, markdown(text));}
  else {
    const parts = blocks(text);
    if(parts.some(part => 'code' in part)) body.append(...parts.map(part => 'code' in part ? codeBlock(part) : linked(el('div','prose'), part.text)));
    else linked(body, text);
  }
  article.append(body); parent.append(article);
  // A message sent while a run went: a bubble at the right, checked once the run read it.
  if(label.startsWith('YOU · ')) steerBubble(article, head, body, label === 'YOU · WHILE RUNNING', parent !== $('queued'));
  return body;
}
// Three lines of a long bubble until it is tapped; whether it is long is measured once it
// is laid out (and again if its width changes), and marked a frame later (a class changed
// inside the observer's callback would resize what it observes: a loop to the browser).
const clamped = typeof ResizeObserver === 'function' ? new ResizeObserver(entries => {
  const changed = [];
  for(const {target} of entries) {
    if(!target.isConnected) {clamped.unobserve(target); continue;}
    const article = target.parentElement;
    if(!article || article.classList.contains('whole') || !target.clientHeight) continue;
    const long = target.scrollHeight > target.clientHeight + 1;
    if(long !== article.classList.contains('long')) changed.push([article, target, long]);
  }
  if(changed.length) requestAnimationFrame(() => {for(const [article, body, long] of changed) markLong(article, body, long);});
}) : null;
// Waiting to be read (`clamp` false), a bubble stays whole.
function steerBubble(article, head, body, read, clamp = true) {
  article.classList.add('steer');
  if(read) {const check = icon('check', 12); check.removeAttribute('aria-hidden'); check.setAttribute('role','img'); check.setAttribute('aria-label','Read by the running turn'); head.append(check);}
  if(!clamp) return;
  body.onclick = event => {if(!article.classList.contains('long') || event?.target?.closest?.('a, button')) return; const whole = !article.classList.contains('whole'); article.classList.toggle('whole', whole); body.setAttribute('aria-expanded', String(whole));};
  body.onkeydown = event => {if((event.key === 'Enter' || event.key === ' ') && article.classList.contains('long')) {event.preventDefault(); body.onclick();}};
  clamped?.observe(body);
}
function markLong(article, body, long) {
  article.classList.toggle('long', long);
  if(long) {
    body.setAttribute('role','button'); body.tabIndex = 0; body.setAttribute('aria-expanded', String(article.classList.contains('whole')));
    if(!body.querySelector('.steer-arrow')) {const arrow = el('span','steer-arrow'); arrow.append(icon('down', 14)); body.append(arrow);}
  } else {
    body.removeAttribute('role'); body.removeAttribute('tabindex'); body.removeAttribute('aria-expanded'); body.querySelector('.steer-arrow')?.remove();
  }
}
function codeBlock(part) {const pre = el('pre','code',part.code); pre.tabIndex = 0; if(part.lang) pre.dataset.lang = part.lang; return pre;}
// Text with every known workspace path as a link; plain text when none is named.
function linked(parent, text) {
  const parts = pathParts(text, knownFiles);
  if(parts.length === 1 && !parts[0].path) {parent.textContent = text; return parent;}
  return appendLinked(parent, text, parts);
}
function appendLinked(parent, text, parts = pathParts(text, knownFiles)) {
  for(const part of parts) parent.append(part.path ? fileLink(part.path, part.text) : document.createTextNode(part.text));
  return parent;
}
// A link to a file of the rendered revision: the viewer sheet opens over the chat; its
// href is the raw file, for "open in new tab".
function fileLink(path, label = path, options = {}) {
  const id = selected, revision = snapshot?.revision;
  const link = el('a', 'path-link', label);
  link.href = `/api/sessions/${id}/file?revision=${revision}&path=${encodeURIComponent(path)}`;
  link.onclick = event => {event.preventDefault?.(); openViewer(id, revision, path, options).catch(fail);};
  return link;
}
function inlineNodes(parent, runs, inLink = false) {
  for(const run of runs) {
    if(run.type === 'text') {if(inLink) parent.append(document.createTextNode(run.text)); else appendLinked(parent, run.text);}
    else if(run.type === 'code') {const code = el('code', '', run.text), path = run.text.replace(/^\/workspace\/|^\.\//, ''); parent.append(!inLink && knownFiles.has(path) ? wrapLink(path, code) : code);}
    else if(run.type === 'link') {const a = el('a'); a.href = run.href; a.target = '_blank'; a.rel = 'noopener noreferrer'; inlineNodes(a, run.children, true); parent.append(a);}
    else {const node = el(run.type === 'strong' ? 'strong' : run.type === 'em' ? 'em' : 'del'); inlineNodes(node, run.children || [], inLink); parent.append(node);}
  }
}
function wrapLink(path, child) {const link = fileLink(path, ''); link.append(child); return link;}
function mdNodes(parent, list) {
  for(const block of list) {
    if(block.type === 'paragraph') {const p = el('p'); inlineNodes(p, block.inline); parent.append(p);}
    else if(block.type === 'heading') {const h = el(`h${Math.min(6, block.level + 2)}`); inlineNodes(h, block.inline); parent.append(h);}
    else if(block.type === 'code') parent.append(codeBlock({code: block.code, lang: block.lang}));
    else if(block.type === 'list') {
      const list = el(block.ordered ? 'ol' : 'ul'); if(block.ordered && block.start !== 1) list.start = block.start;
      for(const item of block.items) {const li = el('li'); mdNodes(li, item); list.append(li);}
      parent.append(list);
    }
    else if(block.type === 'quote') {const quote = el('blockquote'); mdNodes(quote, block.blocks); parent.append(quote);}
    else if(block.type === 'rule') parent.append(el('hr'));
    else if(block.type === 'table') {
      const wrap = el('div','md-table'), table = el('table'), head = el('tr');
      wrap.tabIndex = 0;
      const cell = (tag, runs, k) => {const c = el(tag, block.align[k] ? `align-${block.align[k]}` : ''); inlineNodes(c, runs); return c;};
      head.append(...block.head.map((runs, k) => cell('th', runs, k)));
      const thead = el('thead'), tbody = el('tbody'); thead.append(head);
      for(const row of block.rows) {const tr = el('tr'); tr.append(...row.map((runs, k) => cell('td', runs, k))); tbody.append(tr);}
      table.append(thead, tbody); wrap.append(table); parent.append(wrap);
    }
  }
  return parent;
}
// The kind of a recorded call, shown where the card would otherwise say "recorded".
const KINDS = {read_file:'read', write_file:'write', edit_file:'edit', read_output:'output', start_job:'job', read_job:'job', stop_job:'job'};
const firstLine = value => String(value).split('\n').find(line => line.trim()) || String(value);
function tool(parent, call, result, preview = false) {
  const card = el('details',`tool-card${preview ? ' preview' : ''}${result?.is_error ? ' failed' : ''}`);
  card.open = preview;
  card.dataset.call = call.id; card.dataset.recorded = String(Boolean(result));
  const summary = el('summary');
  const input = call.input || {};
  // An icon for its kind: a command, code, a job, a file, a shown file, any other tool.
  const kindIcon = el('span','tool-icon'); kindIcon.setAttribute('aria-hidden','true');
  kindIcon.append(icon(call.name === 'bash' ? 'command' : call.name === 'run_code' ? 'code' : ['start_job','read_job','stop_job'].includes(call.name) ? 'job' : ['read_file','write_file','edit_file','read_output'].includes(call.name) ? 'file' : call.name === 'show' ? 'show' : 'tool', 15));
  summary.append(kindIcon);
  const title = input.command || input.path || (call.name === 'read_job' ? (input.job ? `read ${input.job}` : 'list jobs') : call.name === 'stop_job' ? `stop ${input.job || ''}` : '') || (call.name === 'run_code' && typeof input.code === 'string' ? firstLine(input.code) : call.name === 'read_output' ? `read_output ${String(input.artifact || '').slice(0,12)}` : `${input.language || ''} ${call.name}`);
  const kind = call.name === 'run_code' ? String(input.language || 'code') : KINDS[call.name] || call.name;
  // Collapsed, a card still says what came back: one quiet line under its title.
  const text = el('span','tool-text'), gist = result ? resultSummary(call, result) : '';
  text.append(el('span','tool-title',String(title).split('\n')[0]));
  if(gist) text.append(el('small','tool-sub',gist));
  summary.append(text,el('span','badge',preview ? 'Live' : result ? result.is_error ? 'Failed' : kind : 'Pending'));
  const body = el('div','tool-body');
  const edit = call.name === 'edit_file' && typeof input.old_string === 'string' && typeof input.new_string === 'string';
  const command = call.name === 'bash' ? input.command : call.name === 'run_code' ? input.code : edit ? `− ${input.old_string}\n+ ${input.new_string}` : call.name === 'write_file' && typeof input.content === 'string' ? input.content : JSON.stringify(input,null,2);
  body.append(edit ? editDiff(input.old_string, input.new_string) : el('pre','input',command || ''));
  let terminal;
  if(preview && ['bash','run_code'].includes(call.name)) {
    body.append(el('div','tool-result-label','OUTPUT · live preview'));
    const output = el('pre','terminal'); output.tabIndex = 0; body.append(output); terminal = new Terminal(output);
  }
  if(result) body.append(el('div','tool-result-label','RESULT · what the model saw'),appendLinked(el('pre','result'),typeof result.content === 'string' ? result.content : JSON.stringify(result.content ?? '', null, 2)));
  const footer = el('div','tool-footer');
  const copy = el('button','', 'Copy'); copy.type = 'button'; copy.onclick = async () => {try {await navigator.clipboard.writeText(terminal?.text() || result?.content || command || ''); note('Copied.');} catch {toast('Clipboard unavailable.');}};
  footer.append(copy); body.append(footer); card.append(summary,body); parent.append(card);
  return {card,body,terminal,text:`${title}\n${command || ''}\n${typeof result?.content === 'string' ? result.content : ''}`};
}
// An edit as the lines it removes and adds; positions are within the edited snippet,
// not the file, so hunks are separated without line numbers.
function editDiff(before, after) {
  const pre = el('pre','input diff'), rows = unified(`${before}\n`, `${after}\n`, 2);
  if(!rows) {pre.textContent = `− ${before}\n+ ${after}`; return pre;}
  pre.append(...rows.map(([kind, line], i) => kind === '@' ? el('span','diff-hunk', i ? '⋯\n' : '') : el('span', kind === '+' ? 'diff-add' : kind === '-' ? 'diff-del' : '', `${kind === ' ' ? ' ' : kind === '+' ? '+' : '−'} ${line}\n`)));
  return pre;
}
const STOP_HINTS = {
  'stopped: too many incomplete responses': 'The model hit the output limit three times without finishing. Recovering re-runs the step from an earlier point (a new model request).',
  'stopped: budget reached': 'This thread reached its history budget. Rewind to an earlier step, or start a new thread.',
  'stopped: step limit reached': 'This turn used its step budget. Continue grants another full budget from here; History rewinds instead.',
  "stopped by the provider's content filter": 'The provider stopped this turn. Rewind to continue from an earlier point.',
  'stopped by tool policy': 'A tool call was refused and the turn stopped. Rewind to continue from an earlier point.',
  'cancelled': 'This run was stopped. Recover re-runs from the last step before a model request; History rewinds further.',
};
// A provider that refuses the credential (expired key, revoked OAuth) needs a sign-in.
const needsSignin = data => Boolean(data.rejected) && !data.busy && [401,403].includes(Number(data.rejected.status));
// Why an idle thread cannot take a message, and what each action would do. Shown at the
// end of the thread and again when the status is tapped.
function statusHelp(data) {
  if(data.missing) return 'The host stopped while creating this thread, so it has no store. Delete it from Settings.';
  if(data.rejected && !data.busy) {
    const why = `The provider refused the model request (HTTP ${data.rejected.status})${data.rejected.detail ? `: ${data.rejected.detail}` : ''}.`;
    return needsSignin(data) ? `${why} Sign in below, then Retry sends the same request; Continue would meet the same refusal.` : `${why} Fix the cause (sign in, model, settings), then Retry sends it again; Continue would meet the same refusal.`;
  }
  if(data.budget?.blocked && !data.busy) return `${data.budget.blocked}.`;
  if(data.phase?.includes('ModelWait') && !data.busy && data.recorded) return 'The model reply is already saved. Continue records it and carries on; nothing is sent or billed again.';
  if(data.phase?.includes('ModelWait') && !data.busy) return 'A model request was interrupted. Retry re-runs it from the step before (it may be billed again); Continue only works if it was never sent.';
  if(data.stop && !data.complete && !data.busy) return `${STOP_HINTS[data.stop] || data.stop}${data.recover ? '' : ' Open the revision list to pick a rewind point.'}`;
  if(data.complete && data.follow_up && !data.busy) return 'A session pack has a follow-up for this turn that the host stopped before sending. Continue delivers it now; a new message works too.';
  return '';
}
// One control row: revision chip, status, and only the actions valid right now.
function renderBar(data) {
  const stopped = Boolean(data.stop) && !data.complete && !data.busy && !historical;
  $('bar').dataset.mode = historical ? 'history' : stopped ? 'stopped' : data.busy ? 'busy' : 'live';
  $('hist-chip').hidden = false;
  $('hist-chip').textContent = historical ? `r${data.revision}/${data.head}` : `r${data.head} ▾`;
  $('hist-prev').hidden = !historical; $('hist-next').hidden = !historical;
  $('hist-prev').disabled = data.revision <= 1;
  $('hist-next').disabled = data.revision >= data.head;
  if(historical) $('phase-text').textContent = data.target ? `viewing r${data.revision} · Rewind restores it` : `viewing r${data.revision} · not a rewind point`;
  else if(stopped) $('phase-text').textContent = `stopped · ${data.stop.replace(/^stopped:?\s*/,'')}`;
  $('recover').hidden = !stopped || !data.recover;
  if(data.recover) $('recover').textContent = `Recover r${data.recover}`;
  renderStop();
  const extendable = data.stop === 'stopped: step limit reached';
  // A request the provider refused: Retry sends it again (after fixing the cause);
  // Continue would only meet the same refusal.
  const refused = Boolean(data.rejected) && !data.busy;
  // A completed turn has nothing to continue, unless a session pack's follow-up waits.
  const followUp = Boolean(data.complete && data.follow_up);
  $('continue').hidden = data.busy || historical || (data.complete && !followUp) || (Boolean(data.stop) && !extendable && !followUp) || !data.head || refused;
  $('continue').classList.toggle('primary', extendable || followUp || Boolean(data.recorded) || Boolean(data.budget?.blocked));
  $('retry').hidden = data.busy || historical || !data.phase?.includes('ModelWait') || (Boolean(data.recorded) && !refused) || Boolean(data.budget?.blocked);
  $('bar-rewind').hidden = !historical || !data.target;
  $('bar-live').hidden = !historical;
  $('bar-signin').hidden = historical || !needsSignin(data);
  $('retry').classList.toggle('primary', refused && $('bar-signin').hidden);
  $('hist-chip').setAttribute('aria-label', `Revision history, ${$('hist-chip').textContent.replace(' ▾','')}`);
  $('prompt').placeholder = historical ? `Read-only: viewing r${data.revision}` : data.busy ? 'Steer the run, or: stop' : data.complete || !data.head ? 'Message' : 'Not ready for a message';
  // A crowded row keeps the chip to model · effort.
  $('bar').dataset.actions = String(['bar-signin','recover','continue','retry'].filter(id => !$(id).hidden).length);
  renderChip();
}
const BAR_ACTIONS = ['hist-prev','hist-next','bar-signin','recover','stop','continue','retry','bar-rewind','bar-live'];
// While a run goes and nothing is written, Stop takes Send's place (a red square); a
// message written then steers the run, and Send comes back for it.
function renderStop() {
  const busy = Boolean(selected && snapshot?.busy && !historical);
  $('stop').hidden = !busy || Boolean($('prompt').value.trim());
  $('send').hidden = !$('stop').hidden;
}
// Before a thread's first snapshot nothing of the previous thread's state may show.
function resetBar() {
  activity = {label:'', since:0};
  for(const id of BAR_ACTIONS) $(id).hidden = true;
  $('bar').dataset.mode = 'live'; $('hist-chip').hidden = false; $('hist-chip').textContent = 'r…'; $('phase-text').textContent = 'loading';
  $('send').hidden = false; $('pending-settings').hidden = true; renderChip();
}
// On the thread list the dock starts a new thread: the row keeps only the model chip.
function homeBar() {
  activity = {label:'', since:0};
  for(const id of [...BAR_ACTIONS, 'hist-chip']) $(id).hidden = true;
  const busy = sessions.filter(s => running(s.status?.state)).length;
  $('bar').dataset.mode = 'home'; $('phase-text').textContent = busy ? `new thread · ${busy} running` : 'new thread';
  $('send').hidden = false; $('pending-settings').hidden = true; renderChip();
}
function explainStatus() {
  if(!selected) {$('prompt').focus?.(); return;}
  if(!snapshot) return;
  if(historical) {note(`Viewing r${snapshot.revision} of r${snapshot.head}. Viewing never changes the thread. ${snapshot.target ? 'Rewind restores this point' : 'This step is not a rewind point; History marks the ones that are (↺)'}; Live returns.`); return;}
  const help = statusHelp(snapshot);
  if(help) note(help); else openHistory();
}
// The sign-in for this thread's provider, from the status row.
async function signinProvider() {
  const provider = snapshot?.settings?.provider || snapshot?.provider; if(!provider) return;
  await signInTo(provider);
}
// Host sheet > Sign-ins, open on this provider's sign-in form.
async function signInTo(provider, name = provider) {
  const opening = openHost('login-settings');
  openSignIn({provider, name});
  $('login-settings').scrollIntoView?.({block:'start'});
  await opening;
}
function confirmRecover() {
  const id = selected, head = snapshot?.head, revision = snapshot?.recover;
  if(!revision) return;
  confirmAction(`Recover from r${revision}?`, 'Spindle rewinds to the point before the stuck model step and runs it again. The abandoned future stays recorded; external effects cannot be undone; the new request may be billed.', async () => {
    if(id !== selected || head !== snapshot?.head) throw Error('The thread changed. Review it before confirming again.');
    await recoverThread();
  });
}
async function recoverThread() {
  const id = selected, revision = snapshot?.recover, current = generation;
  if(!id || !revision || snapshot.busy) throw Error('Refresh this thread before recovering.');
  const receipt = await command('rewind',{revision});
  const deadline = Date.now() + 30000;
  while(true) {
    if(selected !== id || current !== generation) return;
    const outcome = await api(`/api/receipts/${receipt.id}`);
    if(outcome.state !== 'running') {
      if(outcome.state !== 'completed') throw Error(outcome.error || 'Rewind did not complete. Inspect the thread before continuing.');
      const data = await api(`/api/sessions/${id}/snapshot`);
      if(selected !== id || current !== generation) return;
      if(!data.busy) {
        historical = false; cursor = ''; generation++; render(data);
        await command('continue'); connect(); return;
      }
    }
    if(Date.now() >= deadline) throw Error('Rewind is still pending. Inspect its receipt before continuing.');
    await new Promise(resolve => setTimeout(resolve, 100));
  }
}
function render(data) {
  // A re-render (run finished, settings saved) keeps the cards and lists the reader opened.
  const opened = new Set([...$('history').querySelectorAll('details[open]')].map(node => node.dataset.call || node.dataset.key));
  snapshot = data; activeTool = null; modelText = null; buffered = null; searchable = []; if(liveContext && liveContext.id !== selected) liveContext = null; $('history').replaceChildren(); $('live').replaceChildren();
  knownFiles = new Set((data.files || []).map(file => file.path));
  const results = new Map();
  for(const m of data.messages || []) for(const b of m.content) if(b.type === 'tool_result') results.set(b.tool_use_id,b);
  // What each prompt's turn cost, under the prompt (muted).
  const costs = new Map((data.turn_costs || []).map(cost => [cost.message, cost]));
  // The files a turn wrote or edited, as one collapsed line at the end of the turn.
  let edited = new Map(), spentLine = '';
  // A turn's ledger: the files it changed and what it cost. While the run is busy the
  // cost still reads under its prompt, where the eye already is.
  const endTurn = () => {
    if(edited.size) $('history').append(ledger([...edited.keys()], spentLine, opened));
    else if(spentLine) $('history').append(el('div','meta turn-cost',spentLine));
    edited = new Map(); spentLine = '';
  };
  for(const [index, m] of (data.messages || []).entries()) {
    // A message sent while the turn ran belongs to that turn; the host gives its words
    // (`steer`) apart from the guidance the model read with them.
    if(m.role === 'user' && m.content.some(b => b.type === 'text' && b.text.trim() && typeof b.steer !== 'string' && !b.text.startsWith('[Earlier conversation compacted'))) endTurn();
    for(const b of m.content) {
      if(b.type === 'text' && typeof b.steer === 'string' && m.role === 'user') {const body = message($('history'),'user',b.steer,false,'YOU · WHILE RUNNING'); searchable.push({node:body.parentElement || body,text:b.steer});}
      else if(b.type === 'text' && b.text.trim()) {
        if(b.text.startsWith('[Earlier conversation compacted')) {const notice = el('div','notice',b.text); $('history').append(notice); searchable.push({node:notice,text:b.text});}
        else {const body = message($('history'),m.role === 'user' ? 'user' : 'assistant',b.text); searchable.push({node:body.parentElement || body,text:b.text});}
      }
      if(b.type === 'tool_use' && (results.has(b.id) || !data.busy || historical)) {
        const result = results.get(b.id), shown = !result?.is_error && Array.isArray(result?.display) ? result.display.filter(item => item && typeof item.path === 'string') : [];
        // A `show` call is its display: the card of the call itself would only repeat it.
        if(!(shown.length && b.name === 'show')) {const card = tool($('history'),b,result); if(opened.has(b.id)) card.card.open = true; searchable.push({node:card.card,text:card.text});}
        for(const item of shown) {const card = displayCard(item, selected); $('history').append(card); searchable.push({node:card,text:`${item.title || ''}\n${item.path}`});}
        if(result && !result.is_error && ['write_file','edit_file'].includes(b.name) && typeof b.input?.path === 'string') edited.set(b.input.path, b.id);
      }
    }
    const spent = turnCost(costs.get(index), data.busy);
    if(spent && data.busy) $('history').append(el('div','meta turn-cost',spent));
    else if(spent) spentLine = spent;
  }
  endTurn();
  foldSteps($('history'), opened);
  packCards(data);
  if(data.self_edit && !historical && pendingChange(data.self_edit)) $('history').append(changeCard(data, opened));
  if(!$('history').children.length && !data.busy && !data.missing) $('history').append(el('p','note empty','No messages yet. Write the first one below.'));
  queued = data.busy && !historical ? data.queued || null : null; renderQueued();
  // With replay, the retained live events rebuild this area themselves.
  if(data.busy && !historical && !data.replay) for(const call of data.pending || []) {
    activeTool = tool($('live'),call,null,true);
  }
  // This thread's background jobs while any still run: a quiet line, not cards.
  if(!historical) for(const job of data.jobs || []) if(job.running) $('live').append(el('div','meta jobs',`& ${job.command} · ${job.id} · running`));
  // Replayed events name the current step themselves; until then the run is just running.
  if(data.busy) setActivity(!data.replay && (data.pending || [])[0] ? `Running ${data.pending[0].name}` : 'Working on the host');
  else {activity = {label:'', since:0}; $('phase-text').textContent = data.missing ? 'no store · delete this thread' : needsSignin(data) ? `needs sign-in · ${data.provider || 'provider'}` : data.rejected ? refusal(data.rejected) : data.complete && data.follow_up ? 'pack follow-up waiting · Continue' : data.complete ? 'ready' : data.stop ? data.stop : data.budget?.blocked ? 'budget reached · raise it in the thread menu, then Continue' : data.phase?.includes('ModelWait') ? (data.recorded ? 'reply saved · Continue' : 'interrupted model request · Retry') : data.phase || '';}
  // The model under the title; what the thread spent at the right (its limit and usage are
  // in the thread menu), and a hairline under the header of it against the limit.
  const settings = threadSettings(data);
  $('subtitle').textContent = [settings.provider, settings.model || data.model].filter(Boolean).join(' · ');
  $('subtitle').title = [data.reasoning ? `Reasoning: ${data.reasoning.label}` : '', data.context?.window ? `Context window: ${tokens(data.context.window)} tokens` : ''].filter(Boolean).join('\n');
  renderSpend(data);
  renderGauge(data); renderAttention(data.attention);
  renderBar(data);
  const pending = !historical && data.pending_settings && typeof data.pending_settings === 'object' ? settingsLine(data.pending_settings) : '';
  $('pending-settings').hidden = !pending; $('pending-settings').textContent = pending ? `Applies next turn: ${pending}` : '';
  const help = historical || data.busy ? '' : statusHelp(data);
  if(help) $('live').append(el('div', `notice state${data.rejected || data.stop || data.missing ? ' warning' : ''}`, help));
  if(data.source_moved && !data.self_edit && !historical && !data.busy) $('live').append(sourceMoved(data.source_moved.paths || []));
  $('send').disabled = historical;
  $('prompt').disabled = historical;
  // A new thread's first message is kept until the thread has really started.
  if(data.head > 0 && newDraft()?.session === selected) drafts.set('draft:new','');
  if(!$('findbar').hidden) search($('find-text').value, false);
  scroll();
}
// The header's spend: what the thread spent ("$0.041", "subscription", "12K tok"), and the
// bar under the header of it against its limit.
function renderSpend(data) {
  const button = $('spend'), bar = $('spend-bar');
  const max = data ? data.budget?.max_usd ?? threadSettings(data).max_usd : null, usd = Number(data?.usage?.usd) || 0;
  const short = data ? usageLine(data.usage, true) : '';
  button.hidden = !data || (!short && typeof max !== 'number');
  bar.hidden = !data;
  if(!data) return;
  button.textContent = short || money(0);
  const over = typeof max === 'number' && usd >= max;
  button.classList.toggle('over', over); bar.classList.toggle('over', over);
  bar.firstElementChild?.style?.setProperty?.('width', typeof max === 'number' ? `${max > 0 ? Math.min(100, usd / max * 100) : 100}%` : '0');
  const said = spendLine(data);
  button.setAttribute('aria-label', `Spent ${said}. Usage and limit`);
  button.title = [said, usageLine(data.usage)].filter(Boolean).join('\n');
}
// Consecutive tool cards share one folding row, "Ran N steps" (a failure counted with a
// cross); one alone stands by itself. Opened groups stay open through a re-render.
function foldSteps(box, opened) {
  const out = []; let run = [];
  const flush = () => {if(run.length > 1) out.push(stepGroup(run, opened)); else out.push(...run); run = [];};
  for(const node of [...box.children]) {
    if(hasClass(node, 'tool-card') && node.dataset?.call) run.push(node);
    else {flush(); out.push(node);}
  }
  flush();
  box.replaceChildren(...out);
}
function stepGroup(cards, opened) {
  const group = el('details','steps'); group.dataset.key = `steps:${cards[0].dataset.call}`;
  // Open as left, or when a card in it was left open.
  group.open = opened.has(group.dataset.key) || cards.some(card => card.open);
  const failed = cards.filter(card => hasClass(card, 'failed')).length;
  if(failed) group.classList.add('has-failed');
  const summary = el('summary'), status = el('span','steps-status'); status.setAttribute('aria-hidden','true');
  status.append(icon(failed ? 'x' : 'check', 14));
  summary.append(status, el('span','steps-label',`Ran ${cards.length} steps`));
  if(failed) {const count = el('span','steps-count'); count.setAttribute('role','img'); count.setAttribute('aria-label',`${failed} failed`); count.append(icon('x', 11), document.createTextNode(String(failed))); summary.append(count);}
  const chevron = el('span','steps-chevron'); chevron.setAttribute('aria-hidden','true'); chevron.append(icon('next', 14));
  summary.append(chevron);
  group.append(summary, ...cards);
  return group;
}
// "$0.041 of $5.00", "subscription", "12K tok" or "no model calls yet".
function spendLine(data) {
  const spent = usageLine(data.usage, true), max = data.budget?.max_usd ?? data.settings?.max_usd;
  if(typeof max === 'number') return `${spent || money(0)} of ${money(max)}`;
  return spent || 'no model calls yet';
}
function ledger(paths, spent, opened) {
  const box = el('details','turn-changes'); box.dataset.key = `changes:${paths.join('\n')}`; box.open = opened.has(box.dataset.key);
  box.append(el('summary','',`✎ ${paths.length} file${paths.length === 1 ? '' : 's'} changed${spent ? ` · ${spent}` : ''}`));
  const list = el('div','change-list');
  for(const path of paths) {
    const row = el('div','change-row'); row.append(fileLink(path));
    const diff = el('button','diff-link','Δ'); diff.type = 'button'; diff.title = `The latest change to ${path}`; diff.setAttribute('aria-label', `Latest change to ${path}`);
    diff.onclick = () => openViewer(selected, snapshot?.revision ?? snapshot?.head, path, {diff: true}).catch(fail);
    row.append(diff); list.append(row);
  }
  box.append(list);
  return box;
}
// The operator's attention (serve/attention.rs): the files held open in the viewer,
// posted as it happens. Advisory: a post that fails is dropped, never raised. Like
// every POST it carries the same-origin header, or the host refuses it.
function attend(kind, name, at) {
  const id = selected; if(!id || historical || !name) return;
  const body = at ? {kind, name, what: 'close', ms: Date.now() - at} : {kind, name, what: 'open'};
  fetch(`/api/sessions/${id}/attention`, {method: 'POST', headers: {'Content-Type': 'application/json', 'X-Spindle': '1'}, body: JSON.stringify(body)}).catch(() => {});
}
// The context gauge: the newest model request against this thread's window — a
// hairline over the composer, warm as it nears the end. Only a running turn reports
// the request size exactly (each step resends the context, so a thread's token
// totals would overstate it); totals stay in the subtitle and thread sheet.
function renderGauge(data) {
  const node = $('gauge'); if(!node) return;
  const window = data?.context?.window, used = liveContext?.id === selected ? liveContext.tokens : 0;
  if(!window || !used) {node.hidden = true; node.title = ''; return;}
  const ratio = Math.min(used / window, 1);
  node.hidden = false; node.classList.toggle('high', ratio >= 0.85);
  node.firstElementChild.style.width = `${Math.max(2, Math.round(ratio * 100))}%`;
  node.title = `context ≈ ${tokens(used)} of ${tokens(window)} tokens${data?.model ? ` · ${data.model}'s window` : ''}`;
}
// The composer's flag: a note kept, or looks still held, for the next turn.
function renderAttention(data) {
  attention = data && !historical ? data : null;
  const node = $('attention'); if(!node) return;
  const held = attention?.held || 0, notes = (attention?.notes || []).length;
  node.hidden = !(held || notes);
  if(!node.hidden) node.textContent = `⚑ ${[notes ? `${notes} note${notes === 1 ? '' : 's'}` : '', held ? `${held} look${held === 1 ? '' : 's'} held` : ''].filter(Boolean).join(' · ')} for the next turn ›`;
  const state = $('menu-note-state'); if(state) state.textContent = held || notes ? 'waiting' : '';
}
async function refresh(revision) {
  if(!selected) return;
  const id = selected, current = generation;
  try {const data = await api(`/api/sessions/${id}/snapshot${revision ? `?revision=${revision}` : ''}`); if(id === selected && current === generation) {render(data); return data;}}
  catch(error) {if(id === selected && current === generation) {if(error.status === 404) gone(id); else {fail(error); if(!snapshot) $('phase-text').textContent = 'not loaded';}}}
  return null;
}
const refusal = rejected => `provider refused (HTTP ${rejected.status})${rejected.detail ? `: ${String(rejected.detail).slice(0,160)}` : ''} · Retry`;
function scheduleRefresh() {const id = selected, current = generation; clearTimeout(refreshTimer); refreshTimer = setTimeout(() => {if(id === selected && current === generation && !historical) {cursor = ''; connect();}},180);}
function recorded(id) {return Boolean($('history').querySelector(`[data-call="${CSS.escape(id)}"][data-recorded="true"]`));}
function flushBuffered() {if(buffered?.length) {modelText = message($('live'),'assistant','',true); modelText.append(document.createTextNode(buffered.join('')));} buffered = null;}
function preview(event, seq) {
  if(historical) return;
  // During catch-up, text that was already committed is in history; only text of
  // the step still in flight is buffered until the stream reaches the present.
  const catching = seq <= catchup;
  if(!catching) flushBuffered();
  switch(event.type) {
    case 'prompt': if(!catching) message($('live'),'user',event.text); break;
    case 'steered':
      if(catching) break;
      message($('live'),'user',event.text,false,'YOU · WHILE RUNNING');
      // The oldest waiting copy of it has been read.
      if(queued?.messages) {const at = queued.messages.indexOf(event.text); if(at >= 0) queued.messages.splice(at,1); if(!queued.messages.length) queued = null; renderQueued();}
      break;
    // The snapshot already holds what waits; replayed changes are older than it.
    case 'queued': if(!catching) {queued = event.queued || null; renderQueued();} break;
    case 'model': modelText = null; if(catching) buffered = []; setActivity(`Thinking · step ${event.step + 1} · ${event.model}`); break;
    case 'delta': if(catching) {buffered?.push(event.text); break;} if(!modelText) modelText = message($('live'),'assistant','',true); modelText.append(document.createTextNode(event.text)); break;
    case 'withdrawn': if(catching) buffered = null; else if(modelText) modelText.textContent = 'The provider withdrew this preview.'; modelText = null; break;
    case 'model_text': if(catching) buffered = null; else if(event.text && !modelText) message($('live'),'assistant',event.text,true); modelText = null; break;
    case 'usage':
      // The newest request is the live context: the hairline over the composer.
      liveContext = {id: selected, tokens: (event.input || 0) + (event.output || 0)};
      if(!catching) {$('live').append(el('div','meta',`tokens · in ${event.input} · out ${event.output}`)); renderGauge(snapshot);}
      break;
    case 'cost': if(!catching) $('live').append(el('div','meta',`cost · ${event.billing === 'subscription' ? 'subscription' : event.billing === 'unpriced' ? 'unpriced' : money(event.usd)} · turn ${money(event.turn)} · thread ${money(event.total)}`)); break;
    case 'tool':
      modelText = null;
      if(recorded(event.id)) {activeTool = null; break;}
      activeTool = tool($('live'),event,null,true); setActivity(`Running ${event.name}`); break;
    case 'nested': if(activeTool) activeTool.body.append(el('div','nested',`↳ ${event.label}`)); setActivity(event.label); break;
    case 'output': if(activeTool?.terminal) activeTool.terminal.write(decode(event.data),event.stream); break;
    case 'output_end': break;
    case 'result': case 'tool_result':
      if(activeTool) {
        activeTool.body.append(el('div','tool-result-label','RECORDED RECEIPT · adoption pending'),el('pre','result',event.content)); activeTool.card.querySelector('.badge').textContent = event.failed ? 'Failed' : 'Receipt'; if(event.failed) activeTool.card.classList.add('failed');
        // What the call shows appears at once, under its card.
        if(!event.failed && Array.isArray(event.display)) for(const item of event.display) if(item && typeof item.path === 'string') $('live').append(displayCard(item, selected));
      }
      setActivity('Waiting for the model');
      break;
    case 'warning': $('live').append(el('div','notice warning',event.text)); break;
    case 'notice': $('live').append(el('div','notice',event.text)); break;
    case 'compacted': $('live').append(el('div','notice',`Compacted history: ${Math.round(event.before/1024)} → ${Math.round(event.after/1024)} KiB. Older revisions remain inspectable.`)); break;
    case 'done':
    if(event.error) {lastError = event.error; toast(event.error);}
    // Messages sent while the run was busy that it ended without reading come back.
    if(!catching && Array.isArray(event.unsent) && event.unsent.length && selected) {
      const restored = [$('prompt').value, ...event.unsent].filter(text => text.trim()).join('\n\n');
      $('prompt').value = restored; drafts.set(`draft:${selected}`, restored);
      if(!event.error) toast('The run ended before reading your last message; it is back in the composer.');
    }
    if(!catching) {finished(selected, event.state === 'completed' ? 'done' : event.error ? `failed: ${event.error}` : event.state); queued = null; renderQueued();}
    scheduleRefresh();
    break;
    case 'stopped': case 'rewound': scheduleRefresh(); break;
    case 'pack_view': if(!catching) packView($('live'), event.pack, event.title, event.markdown); break;
    case 'pack_ui': packRequest(event); break;
    case 'pack_ui_done': for(const card of $('live').children) if(card.dataset?.packUi === event.id) closePackCard(card, 'Closed'); break;
  }
  scroll();
}
function connect() {
  source?.close(); source = null;
  if(!selected || historical || document.hidden) return;
  const id = selected, current = generation;
  source = new EventSource(`/api/sessions/${id}/events${cursor ? `?after=${encodeURIComponent(cursor)}` : ''}`);
  const stream = source, active = () => source === stream && current === generation && selected === id;
  source.onopen = () => {if(active()) {connection('Connected'); banner('stream');}};
  source.onerror = () => {
    // The host may be restarting (an install, a rollback): its preview links die with it.
    previewLinks.clear();
    if(!active()) return;
    connection(navigator.onLine === false ? 'Offline' : 'Reconnecting…');
    // Closed (not reconnecting): the host refused the stream. Find out why once.
    if(stream.readyState === 2) probe(id, current);
    // Still reconnecting: an expired Access sign-in or a revoked pairing looks the same
    // here, so ask the host now and then (api() raises the right prompt).
    else if(Date.now() - probed > 10000) {probed = Date.now(); api('/api/ping').catch(() => {});}
  };
  source.addEventListener('snapshot',event => {if(!active()) return; cursor = event.lastEventId; const data = JSON.parse(event.data); catchup = data.replay ? Number(cursor.split(':')[1]) : 0; render(data); restarted(cursor.split(':')[0]);});
  source.addEventListener('preview',event => {if(!active()) return; cursor = event.lastEventId; const seq = Number(cursor.split(':')[1]); preview(JSON.parse(event.data), seq); if(seq === catchup) flushBuffered();});
  // A background job that began or ended is an event like the others; one that ended
  // is a knock on the door of whoever left the tab.
  source.addEventListener('job',event => {
    if(!active()) return;
    let job = {}; try {job = JSON.parse(event.data);} catch {return;}
    if(job.running) {if(snapshot && !snapshot.busy) refresh(); return;}
    const name = sessions.find(s => s.id === id)?.name || 'A thread';
    const body = `${job.command || job.id} · ${job.state}`;
    if(away()) showNote(`Spindle · ${name}`, body, id);
    else if(!snapshot?.busy) {toast(body, 'info'); refresh();}
    else refresh();
  });
  source.addEventListener('failure',event => {
    if(!active()) return;
    stream.close(); source = null; connection('Error');
    let error = 'unknown error'; try {error = JSON.parse(event.data).error;} catch {}
    banner('stream',`This thread could not be read: ${error}`,[['Retry',() => {banner('stream'); cursor = ''; connect();}]]);
  });
}
// A stream the browser gave up on: a deleted thread (404) ends here, pairing and
// sign-in problems surface through api(), anything else retries after a pause.
async function probe(id, current) {
  try {await api(`/api/sessions/${id}/snapshot`);}
  catch(error) {
    if(selected !== id || current !== generation) return;
    if(error.status === 404) {gone(id); return;}
    if(error.status === 0 || error.status === 401) return;
  }
  setTimeout(() => {if(selected === id && current === generation && !historical) connect();}, 3000);
}
function gone(id) {
  if(selected !== id) return;
  source?.close(); source = null;
  const created = newDraft()?.session === id;
  const reason = created ? `The new thread could not start${lastError ? `: ${lastError}` : ''}. Your first message is kept.` : 'This thread no longer exists on the host.';
  home().then(() => {toast(reason); if(created) openNew(lastError);}).catch(fail);
}
async function openThread(id) {
  // Back to a thread deleted a moment ago (the browser's Back, say): it stays.
  if(pendingDelete?.id === id) {undoDelete(id); hideToast();}
  menuFor = null;
  source?.close(); cursor = ''; generation++; historical = false; selected = id; snapshot = null; following = true; knownFiles = new Set(); displayNodes.clear(); filesCache.clear();
  $('history').replaceChildren(el('p','note loading','Loading…')); $('live').replaceChildren(); queued = null; renderQueued(); $('send').disabled = true; closeSearch(); attached = []; renderAttachments(); resetBar(); $('jump').hidden = true;
  $('welcome').hidden = true; $('thread').hidden = false; $('dock').hidden = false; $('attach').hidden = false; $('settings').hidden = true; $('title-button').hidden = false; document.body.dataset.view = 'thread';
  const name = sessions.find(s => s.id === id)?.name || 'Thread';
  lastThread = {id, name};
  $('title').textContent = name; $('subtitle').textContent = ''; renderSpend(null); $('title-button').setAttribute('aria-label', `Thread menu: ${name}`);
  markSeen(id);
  $('prompt').placeholder = 'Message';
  $('prompt').value = drafts.get(`draft:${id}`);
  fitPrompt();
  if(location.hash !== `#thread/${id}`) location.hash = `thread/${id}`;
  const current = generation;
  const data = await refresh(); if(id === selected && current === generation && data) connect();
  else if(id === selected && current === generation && !snapshot) loadFailed(id);
}
// The thread could not be read: say so where its messages would be, with a retry.
function loadFailed(id) {
  const retry = el('button','','Try again'); retry.type = 'button'; retry.onclick = () => openThread(id).catch(fail);
  const box = el('div','notice state warning','This thread could not be loaded.'); box.append(retry);
  $('history').replaceChildren(box);
}
const STATES = {running:'RUNNING', stopping:'STOPPING', done:'DONE', stopped:'STOPPED', 'needs attention':'NEEDS ATTENTION', 'reply saved':'REPLY SAVED', empty:'NEW', missing:'NO STORE', unreadable:'UNREADABLE'};
// The thread list: Running, Needs you (a run that stopped and waits on the operator),
// Today and Earlier; each row's state as a shape (a spinner, a warning, a square for a
// reply not yet read here), what it is doing, how long ago, and ⋯ for its actions.
const NEEDS = new Set(['needs attention', 'reply saved', 'missing', 'unreadable']);
const DOING = {running: 'Working', stopping: 'Stopping', done: 'Finished', stopped: 'Stopped', 'needs attention': 'Needs you: open it to see why', 'reply saved': 'Reply saved: Continue records it', empty: 'New', missing: 'No store: delete it', unreadable: 'Could not be read'};
const hasClass = (node, cls) => String(node?.className || '').split(' ').includes(cls);
// How long ago, as the list shows it: now, 13m, 3h, 2d, then the date.
function since(at, now = Date.now()) {
  if(!at) return '';
  const minutes = Math.max(0, Math.floor((now - at) / 60000));
  if(minutes < 1) return 'now';
  if(minutes < 60) return `${minutes}m`;
  if(minutes < 1440) return `${Math.floor(minutes / 60)}h`;
  if(minutes < 10080) return `${Math.floor(minutes / 1440)}d`;
  return new Date(at).toLocaleDateString(undefined, {month: 'short', day: 'numeric'});
}
// When this screen last saw each thread (its status time then): one that finished since
// has a reply not yet read here. A thread never seen before counts as seen.
function threadSeen() {try {return JSON.parse(drafts.get('thread-seen') || '{}') || {};} catch {return {};}}
function markSeen(id) {
  const at = sessions.find(s => s.id === id)?.status?.at; if(!at) return;
  const seen = threadSeen(); if(seen[id] === at) return;
  seen[id] = at; drafts.set('thread-seen', JSON.stringify(seen));
}
function unreadThread(session) {
  const at = session.status?.at;
  if(session.id === selected || session.status?.state !== 'done' || !at) return false;
  const seen = threadSeen();
  if(!(session.id in seen)) {seen[session.id] = at; drafts.set('thread-seen', JSON.stringify(seen)); return false;}
  return seen[session.id] < at;
}
// Threads deleted a moment ago (Undo can still bring them back), and the row whose
// actions are showing.
const hiddenThreads = new Set();
let pendingDelete = null, menuFor = null;
function listSessions() {
  const filter = $('filter').value.trim().toLowerCase();
  $('sessions').replaceChildren();
  const visible = sessions.filter(s => !hiddenThreads.has(s.id));
  const shown = visible.filter(s => !filter || s.name.toLowerCase().includes(filter)).sort((x, y) => (y.status?.at || 0) - (x.status?.at || 0));
  const today = new Date().setHours(0, 0, 0, 0), placed = new Set();
  const groups = [['Running', s => running(s.status?.state)], ['Needs you', s => NEEDS.has(s.status?.state)], ['Today', s => (s.status?.at || 0) >= today], ['Earlier', () => true]];
  for(const [name, member] of groups) {
    const members = shown.filter(s => !placed.has(s.id) && member(s));
    if(!members.length) continue;
    for(const s of members) placed.add(s.id);
    $('sessions').append(el('h2','thread-group',name));
    members.forEach((session, at) => {
      $('sessions').append(sessionRow(session, at === members.length - 1));
      if(menuFor === session.id) $('sessions').append(rowActions(session));
    });
  }
  $('filter-box').hidden = !visible.length && !filter;
  $('intro').hidden = visible.length > 0;
  if(filter && !shown.length) $('sessions').append(el('p','note',`No thread name contains “${$('filter').value.trim()}”.`));
}
function sessionRow(session, last) {
  const state = session.status?.state || '', needs = NEEDS.has(state), fresh = unreadThread(session);
  const row = el('div', `session-row${last ? ' last' : ''}${running(state) ? ' running' : ''}${needs ? ' needs' : ''}${fresh ? ' unread' : ''}`);
  row.dataset.state = state;
  const button = el('button','session'); button.type = 'button';
  const mark = el('span','thread-mark'); mark.setAttribute('aria-hidden','true');
  if(running(state)) mark.append(icon('spinner', 14)); else if(needs) mark.append(icon('warning', 16)); else if(fresh) mark.append(el('span','dot'));
  const text = el('span','thread-text'), top = el('span','thread-top');
  top.append(el('strong','',session.name), el('span','thread-time',since(session.status?.at)));
  const doing = [session.dev || session.self_edit ? 'Self-edit' : '', fresh ? 'Finished · new reply' : DOING[state] || state || 'New', session.usd > 0 ? money(session.usd) : ''].filter(Boolean).join(' · ');
  text.append(top, el('small','',doing));
  button.append(mark, text);
  button.title = [session.name, STATES[state] || state, session.status?.at ? new Date(session.status.at).toLocaleString() : ''].filter(Boolean).join(' · ');
  button.onclick = () => openThread(session.id).catch(fail);
  const more = el('button','thread-more'); more.type = 'button';
  more.setAttribute('aria-label', `More actions for ${session.name}`); more.title = 'More actions';
  more.setAttribute('aria-expanded', String(menuFor === session.id)); more.append(icon('more', 18));
  more.dataset.session = session.id;
  more.onclick = event => {
    event?.stopPropagation?.(); menuFor = menuFor === session.id ? null : session.id; listSessions();
    [...$('sessions').querySelectorAll('.thread-more')].find(node => node.dataset.session === session.id)?.focus?.();
  };
  row.append(button, more);
  return row;
}
// A row's actions, under it while its ⋯ is open.
function rowActions(session) {
  const box = el('div','row-actions');
  const drop = el('button','danger'); drop.type = 'button';
  drop.append(icon('trash', 16), document.createTextNode(`Delete “${session.name}”`));
  drop.onclick = () => deleteThread(session.id, session.name);
  box.append(drop);
  return box;
}
// Run notifications: a phone only shows them through the service worker; nothing is
// pushed by a server, so they come from this page while it is open, or as a summary
// when it is visible again.
const notifiable = () => typeof Notification !== 'undefined' && Notification.permission === 'granted' && drafts.get('notify') === '1';
async function showNote(title, body, id) {
  if(!notifiable()) return false;
  try {
    const registration = await navigator.serviceWorker?.getRegistration?.();
    if(registration?.active) await registration.showNotification(title, {body, tag: id || 'spindle', icon: '/icon.svg', data: {url: id ? `/#thread/${id}` : '/'}});
    else new Notification(title, {body, tag: id || 'spindle', icon: '/icon.svg'});
    return true;
  } catch {return false;}
}
const away = () => document.hidden || (typeof document.hasFocus === 'function' && !document.hasFocus());
function finished(id, outcome) {
  const name = sessions.find(s => s.id === id)?.name || 'Thread';
  remember(id, 'finished');
  if(away()) showNote(`Spindle · ${name}`, `Run finished: ${outcome}`, id);
}
function remember(id, state) {const seen = statuses(); seen[id] = state; drafts.set('thread-status', JSON.stringify(seen));}
function statuses() {try {return JSON.parse(drafts.get('thread-status') || '{}') || {};} catch {return {};}}
const running = state => state === 'running' || state === 'stopping';
async function loadSessions() {
  const data = await api('/api/sessions'); const previous = statuses();
  sessions = data.sessions;
  // Runs that ended since this screen last looked (the open thread reports its own).
  const ended = sessions.filter(s => running(previous[s.id]) && !running(s.status?.state) && s.id !== selected);
  if(ended.length) {
    const text = ended.map(s => `${s.name}: ${(STATES[s.status?.state] || s.status?.state || 'ended').toLowerCase()}`).join(' · ');
    if(!(away() && await showNote('Spindle', `Finished: ${text}`))) note(`Finished: ${text}`);
  }
  drafts.set('thread-status', JSON.stringify(Object.fromEntries(sessions.map(s => [s.id, s.status?.state || '']))));
  return data;
}
async function home(clearHash = true) {
  const leaving = selected;
  source?.close(); source = null; generation++; selected = null; snapshot = null; historical = false; queued = null; renderQueued(); activity = {label:'', since:0}; if(clearHash && location.hash) location.hash = '';
  $('welcome').hidden = false; $('thread').hidden = true; $('dock').hidden = false; $('jump').hidden = true; banner('gone'); banner('stream'); document.body.dataset.view = 'home';
  $('title').textContent = 'Spindle'; $('subtitle').textContent = ''; renderSpend(null); $('title-button').hidden = true; $('settings').hidden = false; $('spend').hidden = true;
  // The dock starts a new thread here: just type. Attachments go with a thread's messages.
  attached = []; renderAttachments(); $('attach').hidden = true; closeSearch();
  $('prompt').disabled = false; $('send').disabled = false; $('prompt').placeholder = 'Start a new thread…';
  $('prompt').value = drafts.get('draft:home'); fitPrompt();
  homeBar();
  const current = generation;
  const data = await loadSessions();
  // What was seen of the thread just left is read.
  if(leaving) markSeen(leaving);
  if(current !== generation || selected) return;
  fallback = providersOf(data.providers);
  listSessions(); homeBar();
  loadCatalog().then(() => {if(!selected) renderChip();}).catch(() => {});
  connection('Connected');
}
// Models: the host's catalog (GET /api/models). A host without it gets one model per
// provider from the thread list, every effort level, and a free model field (search).
let fallback = [];
const providersOf = list => (Array.isArray(list) ? list : []).map(p => ({id: p.id, name: p.id, signed_in: true, subscription: false, default_model: p.model, models: p.model ? [{id: p.model, label: p.model, efforts: ALL_EFFORTS.slice(1), default_effort: null, image: false, context: null, price: null}] : []}));
async function loadCatalog(fresh = false) {
  if(catalog && !catalog.fallback && !fresh) return catalog;
  try {
    const data = await api('/api/models');
    if(Array.isArray(data.providers)) catalog = {providers: data.providers.filter(p => p && typeof p.id === 'string').map(p => ({...p, models: Array.isArray(p.models) ? p.models.filter(m => m && typeof m.id === 'string') : []})), fallback: false};
  } catch(error) {
    if(error.status === 0 || error.status === 401) throw error;
    if(!catalog || catalog.fallback) catalog = {providers: fallback, fallback: true};
  }
  if(!catalog) catalog = {providers: fallback, fallback: true};
  return catalog;
}
const catalogModels = () => (catalog?.providers || []).flatMap(p => p.models.map(m => ({provider: p.id, id: m.id, label: m.label})));
function findModel(provider, model) {
  const p = (catalog?.providers || fallback).find(x => x.id === provider);
  return p ? {provider: p, model: p.models.find(m => m.id === (model || p.default_model)) || null} : null;
}
// "default" (auto) first, then what the model accepts; every level for an unknown one.
function effortsFor(provider, model) {
  const listed = findModel(provider, model)?.model?.efforts;
  const levels = Array.isArray(listed) && listed.length ? listed : ALL_EFFORTS.slice(1);
  return ['default', ...ALL_EFFORTS.slice(1).filter(level => levels.includes(level))];
}
// What the thread sends next: the snapshot's `settings`, or its older top-level fields.
function threadSettings(data) {
  const s = data?.settings && typeof data.settings === 'object' ? data.settings : {};
  return {provider: s.provider ?? data?.provider ?? '', model: s.model ?? data?.model ?? '', effort: s.effort ?? data?.effort ?? 'default', network: Boolean(s.network ?? data?.network), toolchain: Boolean(s.toolchain ?? data?.toolchain), compaction: s.compaction ?? data?.compaction ?? 'model', max_usd: s.max_usd !== undefined ? s.max_usd : data?.budget?.max_usd ?? null};
}
function stored(key) {try {const value = JSON.parse(drafts.get(key) || '{}'); return value && typeof value === 'object' ? value : {};} catch {return {};}}
// A new thread starts with what the last one was given on this screen (GLM by default).
function newSettings() {
  // Until the catalog answers, the thread list's providers say enough for the chip.
  const saved = stored('new-settings'), providers = catalog?.providers || fallback;
  const provider = providers.find(p => p.id === saved.provider) || providers.find(p => p.id === 'glm' && p.signed_in !== false) || providers.find(p => p.signed_in !== false) || providers[0];
  if(!provider) return null;
  const model = provider.id === saved.provider && saved.model ? saved.model : provider.default_model || '';
  const effort = effortsFor(provider.id, model).includes(saved.effort) ? saved.effort : 'default';
  return {provider: provider.id, model, effort, network: Boolean(saved.network), toolchain: Boolean(saved.toolchain)};
}
function saveNewSettings(next) {drafts.set('new-settings', JSON.stringify({provider: next.provider, model: next.model, effort: next.effort, network: next.network, toolchain: next.toolchain}));}
// Name, compaction and budget of the next new thread (folded under "Thread options").
const newExtras = () => stored('new-extras');
function renderChip() {
  const s = selected ? (snapshot ? threadSettings(snapshot) : null) : newSettings(), chip = $('model-chip');
  chip.hidden = !s || (Boolean(selected) && historical);
  if(!s) return;
  const found = findModel(s.provider, s.model);
  const model = s.model || found?.provider.default_model || 'default model';
  const sent = selected && snapshot?.reasoning?.short !== 'default' ? snapshot?.reasoning?.short : '';
  const effort = s.effort && s.effort !== 'default' ? EFFORT_NAMES[s.effort] || s.effort : sent || found?.model?.default_effort || 'auto';
  // The provider shows only when the model's name does not already say it.
  const quiet = s.provider && model.toLowerCase().startsWith(s.provider.toLowerCase().slice(0, 3));
  $('chip-provider').textContent = s.provider && !quiet ? s.provider : '';
  $('chip-model').textContent = model; $('chip-effort').textContent = effort;
  chip.dataset.network = String(s.network); chip.dataset.toolchain = String(s.toolchain);
  chip.setAttribute('aria-label', `Model: ${s.provider} ${model}, effort ${effort}${s.network ? ', network on' : ''}${s.toolchain ? ', toolchain on' : ''}. Change`);
}
// One sheet for model, effort, network and toolchain: a thread's go through `settings`
// (only what changed); a new thread's are this screen's until it starts one.
async function openPicker() {
  const thread = Boolean(selected);
  if(thread && !snapshot) {toast('Wait for the thread to load.'); return;}
  if(!catalog) await loadCatalog().catch(() => {});
  const base = thread ? threadSettings(snapshot) : newSettings() || {provider: '', model: '', effort: 'default', network: false, toolchain: false};
  picking = {thread, session: selected, initial: {...base}, ...base};
  $('model-search').value = '';
  $('model-label').textContent = thread ? 'THIS THREAD' : 'NEW THREAD';
  $('picker-more').hidden = thread;
  if(!thread) {const extra = newExtras(); $('new-name').value = extra.name || ''; $('new-compaction').value = extra.compaction === 'digest' ? 'digest' : 'model'; $('new-budget').value = typeof extra.max_usd === 'number' ? String(extra.max_usd) : '';}
  renderPicker();
  if(!$('model-dialog').open) $('model-dialog').showModal();
  const shown = picking;
  try {await loadCatalog(true);} catch {}
  if(picking === shown && $('model-dialog').open) renderPicker();
}
function pickerChanges() {
  const p = picking, out = {};
  if(!p?.thread) return out;
  if(p.provider !== p.initial.provider || (p.model || '') !== (p.initial.model || '')) {out.provider = p.provider; out.model = p.model;}
  for(const key of ['effort','network','toolchain']) if(p[key] !== p.initial[key]) out[key] = p[key];
  return out;
}
function renderPicker() {
  const p = picking; if(!p) return;
  const raw = $('model-search').value.trim(), query = raw.toLowerCase(), list = $('model-list');
  list.replaceChildren();
  let shown = 0;
  for(const provider of catalog?.providers || []) {
    const models = provider.models.filter(m => !query || [m.id, m.label, provider.id, provider.name].some(v => String(v || '').toLowerCase().includes(query)));
    if(!models.length) continue;
    const group = el('div','provider-group'); group.setAttribute('role','listitem');
    const head = el('div','provider-head'); head.append(el('span','provider-name',provider.name || provider.id));
    if(provider.signed_in === false) {const signin = el('button','provider-signin','Sign in'); signin.type = 'button'; signin.onclick = () => pickerSignIn(provider); head.append(signin);}
    else if(provider.subscription) head.append(el('small','tag','subscription'));
    group.append(head);
    for(const model of models) {
      const on = p.provider === provider.id && (p.model || provider.default_model) === model.id;
      const row = el('button',`model-row${on ? ' active' : ''}${provider.signed_in === false ? ' locked' : ''}`); row.type = 'button';
      row.setAttribute('aria-pressed', String(on)); row.dataset.provider = provider.id; row.dataset.model = model.id;
      const price = provider.subscription ? 'subscription' : priceLine(model.price) ? `${priceLine(model.price)} per Mtok` : 'no price';
      const meta = [model.label && model.label !== model.id ? model.id : '', price, model.context ? `${tokens(model.context)} context` : ''].filter(Boolean).join(' · ');
      const text = el('span','model-text'); text.append(el('span','model-name',model.label || model.id), el('small','model-meta',meta));
      row.append(text);
      if(model.image) row.append(el('span','badge','image'));
      row.append(el('span','model-check',on ? '✓' : ''));
      row.onclick = () => provider.signed_in === false ? pickerSignIn(provider) : pickModel(provider.id, model.id);
      group.append(row); shown++;
    }
    list.append(group);
  }
  // Any model id the provider takes, even one the catalog does not list.
  if(raw && p.provider && !catalogModels().some(m => m.provider === p.provider && m.id.toLowerCase() === query)) {
    const row = el('button','model-row custom'); row.type = 'button';
    row.append(el('span','model-text',`Use “${raw}” with ${p.provider}`));
    row.onclick = () => {if(/[\u0000-\u001f]/.test(raw) || raw.length > 120) {toast('A model name is at most 120 characters, without control characters.'); return;} pickModel(p.provider, raw);};
    list.append(row);
  }
  if(!shown && !raw) list.append(el('p','note',catalog ? 'No models are configured on the host.' : 'Loading models…'));
  // The levels the model takes; none chosen (auto) is the model's own default, outlined,
  // which the line under them names (and a chosen level can go back to).
  const efforts = effortsFor(p.provider, p.model), fallbackEffort = findModel(p.provider, p.model)?.model?.default_effort;
  if(!efforts.includes(p.effort)) p.effort = 'default';
  $('effort-choice').replaceChildren(...efforts.filter(level => level !== 'default').map(level => {
    const auto = p.effort === 'default' && level === fallbackEffort;
    const button = el('button', p.effort === level ? 'active' : auto ? 'auto' : '', EFFORT_NAMES[level] || level); button.type = 'button';
    button.dataset.effort = level; button.setAttribute('aria-pressed', String(p.effort === level));
    if(auto) button.title = "This model's default, used while no level is chosen";
    button.onclick = () => {p.effort = level; commitNew(); renderPicker();};
    return button;
  }));
  $('effort-note').textContent = p.effort === 'default' ? '· auto' : '';
  const words = {off: 'Off', low: 'Low', medium: 'Medium', high: 'High', xhigh: 'X-high', max: 'Max'};
  const named = fallbackEffort ? `${words[fallbackEffort] || fallbackEffort} is this model's default` : "The provider's default applies";
  $('effort-default').replaceChildren(document.createTextNode(p.effort === 'default' ? `${named}; none chosen.` : `${named}.`));
  if(p.effort !== 'default') {
    const back = el('button','link-like','Use the default'); back.type = 'button';
    back.onclick = () => {p.effort = 'default'; commitNew(); renderPicker();};
    $('effort-default').append(back);
  }
  $('picker-network').checked = p.network; $('picker-toolchain').checked = p.toolchain;
  const changed = pickerChanges(), dirty = Object.keys(changed).length > 0;
  $('model-apply').textContent = p.thread && dirty ? (snapshot?.busy ? 'Apply after this turn' : 'Apply') : 'Done';
  $('model-apply').classList.toggle('primary', !p.thread || dirty);
  $('model-note').textContent = p.thread ? (dirty ? `Next turn: ${settingsLine(changed)}` : snapshot?.busy ? 'Changes apply when this turn ends.' : '') : 'Starting a thread just takes a message.';
}
function pickModel(provider, model) {
  const p = picking; if(!p) return;
  p.provider = provider; p.model = model;
  if(!effortsFor(provider, model).includes(p.effort)) p.effort = 'default';
  commitNew(); renderPicker();
}
// A new thread's choices are this screen's own: kept as they are made.
function commitNew() {
  const p = picking; if(!p || p.thread) return;
  saveNewSettings(p);
  const raw = $('new-budget').value.trim(), budget = raw === '' ? null : Number(raw);
  drafts.set('new-extras', JSON.stringify({name: $('new-name').value.trim().slice(0, 80), compaction: $('new-compaction').value, max_usd: budget !== null && budget >= 0 ? budget : null}));
  if(!selected) renderChip();
}
async function applyPicker() {
  const p = picking; if(!p) return;
  if(!p.thread) {commitNew(); $('model-dialog').close(); $('prompt').focus?.(); return;}
  const changed = pickerChanges();
  if(!Object.keys(changed).length) {$('model-dialog').close(); return;}
  if(p.session !== selected || !snapshot) {toast('The thread changed. Open the model picker again.'); return;}
  const busy = snapshot.busy;
  $('model-apply').disabled = true;
  try {
    await command('settings', changed);
    if(picking === p) $('model-dialog').close();
    note(`${busy ? 'Applies next turn' : 'Next turn uses'}: ${settingsLine(changed)}.`);
  } catch {} finally {$('model-apply').disabled = false;}
}
function pickerSignIn(provider) {
  $('model-dialog').close();
  signInTo(provider.id, provider.name || provider.id).catch(fail);
}
// Uncertain submissions are kept per thread ("pending-command:ID", "pending-stop:ID",
// "new" for a thread not yet created), so one stuck request never blocks another thread.
const LEGACY = ['pending-command','pending-stop'];
const pendingKey = (stop, id) => `pending-${stop ? 'stop' : 'command'}:${id || 'new'}`;
const stuck = new Set();
function pendingKeys() {let keys = []; try {keys = JSON.parse(drafts.get('pending-keys') || '[]');} catch {} return Array.isArray(keys) ? keys : [];}
function setPending(key, value) {
  drafts.set(key, value);
  const keys = new Set(pendingKeys()); if(value) keys.add(key); else {keys.delete(key); stuck.delete(key);}
  drafts.set('pending-keys', JSON.stringify([...keys]));
  showStuck();
}
// One unscoped key from an earlier version moves to its thread's key.
function migratePending() {
  for(const legacy of LEGACY) {
    const raw = drafts.get(legacy); if(!raw) continue;
    let session = ''; try {const saved = JSON.parse(raw); session = saved.kind === 'new' ? '' : saved.session || '';} catch {session = 'unknown';}
    const key = pendingKey(legacy === 'pending-stop', session);
    if(!drafts.get(key)) setPending(key, raw);
    drafts.set(legacy, '');
  }
}
function showStuck() {
  const key = [...stuck].find(k => drafts.get(k));
  if(!key) {banner('pending'); return;}
  let pending = {}; try {pending = JSON.parse(drafts.get(key));} catch {}
  const thread = sessions.find(s => s.id === pending.session)?.name || (pending.kind === 'new' ? 'a new thread' : 'a thread');
  banner('pending',`Unresolved ${pending.kind || 'submission'} for ${thread}: the host has not confirmed it.`,[['Check again',() => recover().catch(fail)],['Discard',() => discardPending(key),'danger']]);
}
function discardPending(key) {
  confirmAction('Discard the saved submission?','This screen forgets its copy and its request ID. If the host did admit it, it still runs once; check the thread before sending the same work again. Nothing is sent or cancelled.',async () => {setPending(key,''); note('Saved submission discarded.');});
}
async function command(kind, more = {}) {
  const stop = kind === 'stop', id = more.session || selected || '', current = generation;
  if(stop ? stopping : submitting) throw Error('A submission is already in progress.');
  migratePending();
  const key = pendingKey(stop, kind === 'new' ? '' : id);
  if(!stop && drafts.get(key)) {await recover(); throw Error('The previous submission was checked. Review its outcome before submitting more work.');}
  if(stop && drafts.get(key)) {await recover(); if(drafts.get(key)) throw Error('Resolve the previous Stop submission before sending another.');}
  if(stop ? stopping : submitting) throw Error('A submission is already in progress.');
  if(historical && !['rewind','new','rename','delete','settings','rollback'].includes(kind)) throw Error('Return to live before running this command.');
  if(current !== generation) throw Error('The screen changed. Review the thread before submitting.');
  // Rollback acts on the host, not on a thread.
  const host = kind === 'rollback';
  if(kind !== 'new' && !host && !id) throw Error('Open a thread first.');
  if(kind !== 'new' && !host && kind !== 'rename' && kind !== 'delete' && !snapshot && !('expected_revision' in more)) throw Error('Wait for the thread to load.');
  const payload = host ? {request_id:crypto.randomUUID(),kind,...more} : {request_id:crypto.randomUUID(),session:id,kind,expected_revision:snapshot?.head,...more};
  if(stop) stopping = true; else submitting = true;
  setPending(key,JSON.stringify(payload));
  try {
    const receipt = await api('/api/commands',payload);
    setPending(key,'');
    if(receipt.state === 'failed' || receipt.state === 'interrupted') throw Error(receipt.error || 'The host refused this command.');
    if(kind === 'say' && drafts.get(`draft:${id}`).trim() === more.text) drafts.set(`draft:${id}`,'');
    const sameScreen = current === generation && selected === id;
    if(kind === 'new') {if(current === generation) {await home(); await openThread(receipt.session);} else note('New thread submitted. Open it from the thread list.'); return receipt;}
    if(kind === 'delete') {drafts.set(`draft:${id}`,''); if(sameScreen) await home(); else {await loadSessions(); if(!selected) listSessions();} return receipt;}
    if(kind === 'rename') {await loadSessions(); if(!selected) listSessions(); if(selected === id) $('title').textContent = sessions.find(s => s.id === id)?.name || 'Thread'; return receipt;}
    if(!sameScreen) return receipt;
    if(kind === 'say' && $('prompt').value.trim() === more.text) {$('prompt').value = ''; fitPrompt();}
    if(!stop && kind !== 'settings') {
      historical = false;
      if(snapshot) {snapshot.busy = receipt.state === 'running'; renderStop();}
      // Show the run as started now, not when its first event arrives.
      if(receipt.state === 'running') {remember(id, 'running'); if(snapshot) {renderBar(snapshot); setActivity('Starting'); for(const state of $('live').querySelectorAll('.notice.state')) state.remove();}}
    }
    if(kind === 'settings') await refresh(historical ? snapshot?.revision : undefined);
    else if(!stop && receipt.state !== 'running') {cursor = ''; connect();}
    if(kind === 'rewind' && receipt.state === 'running') {cursor = ''; connect();}
    return receipt;
  } catch(error) {
    // Below 500 the host refused it outright: nothing was admitted, so forget it.
    if(error.status && error.status < 500) setPending(key,'');
    // A refusal admitted nothing; only an unknown outcome is checked again by its ID.
    const said = String(error?.message ?? error).replace(/([^.!?])$/, '$1.');
    toast(kind === 'say' || kind === 'new' ? error.status && error.status < 500 ? `${said} Your message is kept.` : `${said} Your message is retained. Reconnect checks the same request ID, never a fresh automatic send.` : error.message);
    if(error && typeof error === 'object') error.reported = true;
    throw error;
  } finally {if(stop) stopping = false; else submitting = false;}
}
async function recover() {
  migratePending();
  for(const key of pendingKeys()) {
    const raw = drafts.get(key); if(!raw) {setPending(key,''); continue;}
    let pending;
    try {pending = JSON.parse(raw);} catch {stuck.add(key); showStuck(); toast('A saved submission is invalid. Its contents are retained for inspection; Discard removes it.'); continue;}
    const stop = key.startsWith('pending-stop');
    if(!stop && submitting || stop && stopping) continue;
    const reconcile = async receipt => {
      if(drafts.get(key) === raw) setPending(key,'');
      if(receipt.state === 'interrupted' || receipt.state === 'failed') {toast(receipt.error || 'Inspect the interrupted command before continuing.'); return;}
      note(`Previous submission: ${receipt.state}. No duplicate was sent.`);
      if(pending.kind === 'say' && drafts.get(`draft:${pending.session}`).trim() === pending.text) {
        drafts.set(`draft:${pending.session}`,'');
        if(selected === pending.session && $('prompt').value.trim() === pending.text) {$('prompt').value = ''; fitPrompt();}
      }
      if(pending.kind === 'new') {await loadSessions(); if(!selected) listSessions();}
      else if(pending.kind === 'delete') {drafts.set(`draft:${pending.session}`,''); if(selected === pending.session) await home(); else {await loadSessions(); if(!selected) listSessions();}}
      else if(selected === pending.session) {if(!historical) {cursor = ''; connect();} if(pending.kind === 'rename') {await loadSessions(); $('title').textContent = sessions.find(s => s.id === selected)?.name || 'Thread';}}
    };
    try {await reconcile(await api(`/api/receipts/${pending.request_id}`));}
    catch(error) {
      stuck.add(key); showStuck();
      if(error.status === 404) {
        confirmAction('Retry this exact submission?', 'The host has no receipt for this ID. Retry uses the original ID and payload; an admitted command cannot execute twice.', async () => {
          if(drafts.get(key) !== raw) throw Error('The pending submission changed. Reconnect before retrying.');
          try {await reconcile(await api('/api/commands',pending));}
          catch(error) {
            // A definitive refusal (the thread moved on, was deleted, ...) admits nothing.
            if(error.status && error.status < 500 && drafts.get(key) === raw) setPending(key,'');
            throw error;
          }
        });
      } else if(error.status !== 0) toast('A previous submission has an unknown admission. Its original ID is retained; Check again when the host is reachable, or Discard it.');
    }
  }
}
// `steps`: a short numbered list under the text; `yes`: the confirming button's label.
function confirmAction(title,text,action,options = {}) {
  if($('confirm-dialog').open) return;
  $('confirm-title').textContent = title; $('confirm-text').textContent = text;
  const steps = Array.isArray(options.steps) ? options.steps : [];
  $('confirm-steps').hidden = !steps.length; $('confirm-steps').replaceChildren(...steps.map(step => el('li','',step)));
  $('confirm-yes').textContent = options.yes || 'Confirm';
  $('confirm-form').onsubmit = async event => {event.preventDefault(); $('confirm-dialog').close(); try {await action();} catch(error) {fail(error);}};
  $('confirm-dialog').showModal();
}
function confirmCommand(title, text, kind, more = {}, options = {}) {
  const id = selected, head = snapshot?.head;
  confirmAction(title,text,async () => {
    if(id !== selected || head !== snapshot?.head) throw Error('The thread changed. Review it before confirming again.');
    if(kind === 'rewind') $('history-dialog').close();
    await command(kind,more);
  },options);
}
function retryThread() {
  confirmCommand('Retry the model call?','The previous request may have been billed. This creates a fresh request identity; it never silently repeats an unknown tool execution.','retry');
}
function rewindThread(revision) {
  confirmCommand(`Rewind to r${revision}?`,'Restore the conversation and workspace together. The abandoned future stays recorded. External network effects cannot be undone. Rewinding does not automatically resume execution.','rewind',{revision});
}
// Deleting leaves the list at once, with a moment to Undo; the host deletes when the moment
// passes, or as soon as this screen is left (an unknown outcome is checked again by its
// request ID, like any command). A running thread must be stopped first.
const UNDO_MS = 8000;
function deleteThread(id, name) {
  const session = sessions.find(s => s.id === id);
  if(running(session?.status?.state) || (selected === id && snapshot?.busy)) {toast('Stop this thread before deleting it.'); return;}
  finishDelete();
  $('thread-dialog').close();
  menuFor = null; hiddenThreads.add(id);
  if(selected === id) home().catch(fail); else listSessions();
  pendingDelete = {id, timer: setTimeout(finishDelete, UNDO_MS)};
  toast(`Deleted “${name}”`, 'info', {action: ['Undo', () => undoDelete(id)], ms: UNDO_MS});
}
function finishDelete() {
  if(!pendingDelete) return;
  clearTimeout(pendingDelete.timer);
  // One submission at a time: a message on its way goes first.
  if(submitting) {pendingDelete.timer = setTimeout(finishDelete, 500); return;}
  const {id} = pendingDelete; pendingDelete = null;
  // The open thread (another one by now) is not what this deletes.
  command('delete', {session: id, expected_revision: undefined}).then(() => {hiddenThreads.delete(id);}, error => {hiddenThreads.delete(id); if(!selected) listSessions(); fail(error);});
}
function undoDelete(id) {
  if(pendingDelete?.id !== id) return;
  clearTimeout(pendingDelete.timer); pendingDelete = null;
  hiddenThreads.delete(id); if(!selected) listSessions();
}
// What the thread has cost, for the thread menu.
function usageText(data) {
  const usage = data?.busy && data.usage ? {...data.usage, unknown: 0} : data?.usage;
  return usage?.calls ? `Spent ${usageLine(usage)}${usage.priced || usage.subscription ? '' : ' · no prices for this model'}` : 'No model calls yet.';
}
// The thread title opens this: the thread's own settings and everything done to it.
function openThreadMenu() {
  if(!selected) return;
  if(!snapshot) {toast('Wait for the thread to load.'); return;}
  const thread = sessions.find(s => s.id === selected), s = threadSettings(snapshot);
  $('thread-label').textContent = `THIS THREAD · R${snapshot.head}${historical ? ` · VIEWING R${snapshot.revision}` : ''}`;
  $('rename').value = thread?.name || ''; $('rename-save').hidden = true;
  $('settings-usage').textContent = usageText(snapshot);
  const max = s.max_usd;
  $('settings-budget').value = budgetShown = typeof max === 'number' ? String(max) : '';
  $('settings-budget').placeholder = 'Unlimited';
  markCompaction(s.compaction);
  const change = snapshot.self_edit;
  $('menu-change').hidden = !change;
  if(change) $('menu-change-state').textContent = [`${change.changed_count ?? change.changed?.length ?? 0} files`, change.request ? 'install requested' : change.installing ? 'installing' : change.behind ? 'host moved' : ''].filter(Boolean).join(' · ');
  $('menu-files-state').textContent = `${(snapshot.files || []).length} · r${snapshot.revision}`;
  $('menu-history-state').textContent = `${(snapshot.steps || []).length} steps`;
  $('delete').disabled = Boolean(snapshot.busy);
  for(const node of [$('settings-budget'), ...$('compaction-choice').querySelectorAll('button')]) node.disabled = Boolean(snapshot.missing);
  if(!$('thread-dialog').open) $('thread-dialog').showModal();
}
function markCompaction(mode) {for(const button of $('compaction-choice').querySelectorAll('button')) {const on = button.dataset.mode === mode; button.classList.toggle('active', on); button.setAttribute('aria-pressed', String(on));}}
// The host sheet (gear on the thread list): sections fold, each says its state.
async function openHost(section = '') {
  markTheme();
  renderNotify();
  $('skin-thread').textContent = selected ? 'Stage from this thread' : lastThread ? `Stage from “${lastThread.name}”` : 'Stage from thread';
  if(!$('settings-dialog').open) $('settings-dialog').showModal();
  if(section) $(section).open = true;
  loadSkin();
  loadDevices();
  loadLogins(); loadShelf();
  const current = generation;
  $('host-info').replaceChildren(el('dt','','Loading host information…'));
  try {
    const [info, spend] = await Promise.all([api('/api/host'), api(`/api/usage?tz=${-new Date().getTimezoneOffset()}`).catch(() => null)]);
    if(current !== generation || !$('settings-dialog').open) return;
    const disk = info.disk || {}, policy = info.code_policy || {models: [], tools: []};
    facts('host-info', [['Version',info.version],['Build',info.build || 'unknown'],['Schema',info.schema],['Engine',info.engine],['Uptime',info.uptime === undefined ? 'unknown' : duration(info.uptime)],['Threads',`${info.sessions} (${info.running} running)`],['Disk',`threads ${size(disk.sessions)} · trash ${size(disk.trash)}`],['Free space',disk.free === undefined || disk.free === null ? 'unknown' : `${size(disk.free)} of ${size(disk.total)}`],['Self-edit source',info.dev ? 'configured' : 'off'],['Origin',info.origin || 'local only']]);
    $('host-state').textContent = [info.build ? `build ${String(info.build).slice(0, 12)}` : info.version, info.running ? `${info.running} running` : ''].filter(Boolean).join(' · ');
    facts('code-info', [['Nested models',(policy.models || []).map(m => `${m.provider}${m.model ? '/' + m.model : ''}`).join(', ') || 'none granted'],['External tools',(policy.tools || []).map(t => `${t.name} (${t.risk})`).join(', ') || 'none granted'],['Limits',policy.limits ? `${policy.limits.calls} calls · ${policy.limits.model_calls} model · ${policy.limits.parallel} parallel` : 'defaults']]);
    const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;
    $('code-state').textContent = `${plural((policy.models || []).length, 'model')} · ${plural((policy.tools || []).length, 'tool')}`;
    if(spend) {
      facts('usage-info', [['Today',money(spend.today?.usd)],['30 days',money(spend.month?.usd)],['All time',`${money(spend.all?.usd)}${spend.all?.unknown ? ` · ${spend.all.unknown} may have been billed` : ''}`]]);
      $('usage-state').textContent = `today ${money(spend.today?.usd)} · 30 days ${money(spend.month?.usd)}`;
    } else {facts('usage-info', [['Spend','unavailable on this host']]); $('usage-state').textContent = '';}
    $('restart').disabled = !info.supervised || info.running > 0;
    $('restart').title = !info.supervised ? 'Needs an OS supervisor (SPINDLE_SUPERVISED=1)' : info.running ? 'Stop or finish running threads first' : '';
    $('rollback').disabled = !info.supervised || info.running > 0;
    $('host-note').textContent = !info.supervised ? 'Restart and rollback need an OS supervisor (SPINDLE_SUPERVISED=1).' : info.running ? 'Restart and rollback wait until no thread is running.' : 'Rollback swaps in the previous binary and restarts; thread data is not rewound.';
  } catch(error) {fail(error);}
}
function facts(id, rows) {$(id).replaceChildren(...rows.flatMap(([k, v]) => [el('dt','',k), el('dd','',String(v))]));}
async function loadDevices() {
  try {renderDevices(await api('/api/devices'));} catch(error) {$('devices').replaceChildren(el('p','note',error.message));}
}
function renderDevices(data) {
  $('devices').replaceChildren();
  const count = (data.devices || []).length;
  $('devices-state').textContent = `${count} paired`;
  for(const device of data.devices || []) {
    const row = el('div','device-row');
    row.append(el('span','',`${device.label}${device.current ? ' · this screen' : ''}`),el('small','',`paired ${ago(device.created)} · seen ${ago(device.seen)}`));
    const revoke = el('button','danger','Revoke'); revoke.type = 'button';
    revoke.onclick = () => confirmAction(`Revoke “${device.label}”?`, device.current ? 'This screen is signed out of Spindle on the host and needs the pairing key again.' : 'That screen is signed out of Spindle on the host and needs the pairing key again. Other screens stay paired.', async () => {const listed = await api('/api/devices',{action:'revoke',id:device.id}); if(device.current) location.reload(); else renderDevices(listed);});
    row.append(revoke); $('devices').append(row);
  }
}
function renderNotify() {
  const supported = typeof Notification !== 'undefined';
  $('notify').disabled = !supported || Notification.permission === 'denied';
  $('notify').textContent = !supported ? 'Notifications unsupported here' : Notification.permission === 'denied' ? 'Notifications blocked in browser settings' : notifiable() ? 'Notifications: on (turn off)' : 'Notify when runs finish';
  $('notify-state').textContent = !supported ? 'unsupported' : Notification.permission === 'denied' ? 'blocked' : notifiable() ? 'on' : 'off';
}
async function toggleNotify() {
  if(typeof Notification === 'undefined') return;
  if(notifiable()) {drafts.set('notify',''); renderNotify(); return;}
  const permission = await Notification.requestPermission();
  if(permission === 'granted') {drafts.set('notify','1'); await registerWorker(); showNote('Spindle','Notifications are on: finished and stopped runs are announced while this app is open.');}
  else toast('Notifications were not allowed.');
  renderNotify();
}
// A just-registered worker cannot show a notification until it is active.
async function registerWorker() {try {await navigator.serviceWorker?.register('/sw.js'); await Promise.race([navigator.serviceWorker.ready, new Promise(resolve => setTimeout(resolve, 3000))]);} catch {}}
// A tapped run notification asks this tab to open its thread.
if(navigator.serviceWorker) navigator.serviceWorker.onmessage = event => {
  const open = event.data?.open; if(typeof open !== 'string') return;
  const hash = new URL(open, location.href).hash;
  if(/^#thread\/[a-zA-Z0-9_-]+$/.test(hash) && hash !== location.hash) location.hash = hash;
};
// The operator's shelf: notes every thread reads at admission (serve's /api/shelf).
let shelf = [];
function loadShelf() {
  $('shelf-state').textContent = '…';
  api('/api/shelf').then(data => {shelf = Array.isArray(data.files) ? data.files : []; renderShelf();}).catch(error => {$('shelf-state').textContent = 'unavailable'; fail(error);});
}
function renderShelf() {
  $('shelf-state').textContent = shelf.length ? `${shelf.length} note${shelf.length === 1 ? '' : 's'}` : 'empty';
  const list = $('shelf-list'); list.replaceChildren();
  for(const file of shelf) {
    const row = el('div','shelf-row');
    const open = el('button','shelf-name'); open.type = 'button'; open.textContent = file.name;
    open.onclick = async () => {
      try {const note = await api(`/api/shelf/file?name=${encodeURIComponent(file.name)}`); $('shelf-name').value = note.name; $('shelf-text').value = note.content || ''; $('shelf-save').textContent = 'Save'; $('shelf-name').focus?.();}
      catch(error) {fail(error);}
    };
    const drop = el('button','','Delete'); drop.type = 'button';
    drop.onclick = () => confirmAction(`Delete “${file.name}”?`, 'The note leaves the shelf for every thread. This cannot be undone.', async () => {
      try {const data = await api('/api/shelf',{action:'delete',name: file.name}); shelf = data.files || []; renderShelf(); note('Deleted.');} catch(error) {fail(error);}
    });
    row.append(open, el('small','', `${size(file.size)} · ${ago(file.modified)}`), drop);
    list.append(row);
  }
  if(!shelf.length) list.append(el('p','note',"Empty. What every thread should know — your preferences, the project's facts — belongs here."));
}
$('shelf-save').onclick = () => {
  const content = $('shelf-text').value;
  if(!content.trim()) {toast('Write the note first.'); return;}
  const name = $('shelf-name').value.trim() || content.trim().split('\n')[0].toLowerCase().slice(0, 24).replace(/[^\w.-]+/g, '-').replace(/^-+|-+$/g, '') || 'note';
  api('/api/shelf',{name, content}).then(data => {
    shelf = data.files || []; renderShelf();
    $('shelf-name').value = ''; $('shelf-text').value = ''; $('shelf-save').textContent = 'Keep';
    note(`Kept as “${name}”. Every thread reads it.`);
  }).catch(fail);
};
let skinState = null;
async function loadSkin() {
  try {skinState = await api('/api/skin');} catch(error) {fail(error); return;}
  renderSkin();
}
function renderSkin() {
  const s = skinState; if(!s) return;
  const safe = document.documentElement?.dataset.skin === 'safe';
  $('skin-status').textContent = `${s.active ? `Applied ${s.active.digest} (${s.active.bytes} B)` : 'No skin applied'}${s.staged ? ` · staged ${s.staged.digest}` : ''}${safe ? ' · safe mode: no skin loaded' : ''}`;
  if(s.staged && !$('skin-css').value) $('skin-css').value = s.staged.css;
  $('skin-apply').disabled = !s.staged; $('skin-discard').disabled = !s.staged; $('skin-preview').disabled = !s.staged; $('skin-off').disabled = !s.active;
  $('skin-thread').disabled = selected ? !snapshot : !lastThread;
  markTheme();
  $('skin-versions').replaceChildren();
  for(const v of s.versions || []) {
    const button = el('button','',`Restore ${v.digest} · ${new Date(v.at).toLocaleString()}`); button.type = 'button';
    button.onclick = () => confirmAction('Restore this skin?',`Version ${v.digest} becomes the applied skin on every paired screen. /?safe always opens without a skin.`,async () => {await skinAction({action:'restore',id:v.id}); location.reload();});
    $('skin-versions').append(button);
  }
}
async function skinAction(data) {skinState = await api('/api/skin', data); renderSkin(); return skinState;}
function stepLabel(step) {
  const label = step.label || '';
  if(label === 'dext-model-v1') return 'model step';
  if(label.startsWith('commit-prepare')) return 'prepared a model request';
  if(label.startsWith('commit-compact')) return 'compacted history';
  if(label.startsWith('adopt')) return `ran ${label.slice(6)}`;
  if(label.startsWith('commit-')) return label.replace('commit-','').replace(/-r\d+$/,'');
  return label || 'step';
}
function openHistory() {
  if(!snapshot) return;
  $('history-list').replaceChildren();
  const targets = new Set(snapshot.targets || []);
  $('history-label').textContent = `HISTORY · r${snapshot.head}`;
  $('history-note').textContent = '↺ marks a rewind point (the newest 48 steps are checked). Tap a step to view it.';
  for(const step of [...(snapshot.steps || [])].reverse()) {
    const row = el('button','history-row');
    row.append(el('span','',`r${step.revision}`), el('span','', stepLabel(step)), el('span','mark', targets.has(step.revision) ? '↺' : ''));
    if(targets.has(step.revision)) row.classList.add('target');
    if(step.revision === snapshot.revision) row.classList.add('current');
    row.onclick = async () => {$('history-dialog').close(); await inspect(step.revision);};
    $('history-list').append(row);
  }
  $('hist-rewind').hidden = !snapshot.target;
  $('history-dialog').showModal();
}
// Search the committed blocks of the rendered thread; matches are highlighted where
// the browser supports CSS highlights and their blocks are marked either way.
function search(query, jump = true) {
  for(const hit of found) hit.node.classList.remove('find-hit','find-current');
  found = query ? searchable.filter(entry => matches(entry.text, query).length) : [];
  $('findbar').hidden = false; $('find-text').value = query;
  if(!found.length) {$('find-count').textContent = query ? 'no match' : ''; highlight(query); return;}
  foundAt = jump ? 0 : Math.min(foundAt, found.length - 1);
  for(const hit of found) hit.node.classList.add('find-hit');
  highlight(query); show(jump);
}
function show(jump) {
  const hit = found[foundAt]; if(!hit) return;
  for(const other of found) other.node.classList.remove('find-current');
  hit.node.classList.add('find-current');
  $('find-count').textContent = `${foundAt + 1}/${found.length}`;
  if(jump) {
    if(hit.node.tagName === 'DETAILS' || hit.node.tagName === 'details') hit.node.open = true;
    // A card folded into a run of steps: the run opens too.
    const group = hit.node.parentElement; if(group && hasClass(group, 'steps')) group.open = true;
    following = false; hit.node.scrollIntoView?.({block:'center'});
  }
}
function step(by) {if(!found.length) return; foundAt = (foundAt + by + found.length) % found.length; show(true);}
function highlight(query) {
  if(!globalThis.CSS?.highlights || typeof Highlight === 'undefined' || !document.createTreeWalker) return;
  const ranges = [];
  for(const hit of query ? found : []) {
    const walker = document.createTreeWalker(hit.node, NodeFilter.SHOW_TEXT);
    for(let node = walker.nextNode(); node; node = walker.nextNode()) for(const at of matches(node.data, query)) {const range = new Range(); range.setStart(node, at); range.setEnd(node, at + query.length); ranges.push(range);}
  }
  CSS.highlights.set('find', new Highlight(...ranges));
}
function closeSearch() {for(const hit of found) hit.node.classList.remove('find-hit','find-current'); found = []; $('findbar').hidden = true; highlight('');}
async function inspect(revision) {
  source?.close(); source = null; historical = true; generation++;
  await refresh(revision);
}
async function backToLive() {
  $('history-dialog').close();
  historical = false; cursor = ''; generation++;
  await refresh(); connect();
}
async function runIntent(text) {
  const wanted = intent(text, catalogModels());
  if(!wanted) return false;
  const needsThread = ['stop','hurry','continue','retry','rewind','files','delete','rename','find','export','download','history','menu','install','update'].includes(wanted.action);
  if(needsThread && !selected) {toast('Open a thread first.'); return true;}
  switch(wanted.action) {
    case 'stop': if(!snapshot?.busy) {toast('No run to stop.'); break;} await command('stop'); break;
    case 'hurry': await sendNow(); break;
    case 'continue': await command('continue'); break;
    case 'retry': retryThread(); break;
    case 'rewind': rewindThread(wanted.revision); break;
    case 'files': openFiles(); break;
    case 'settings':
      if(wanted.fields) await applySettings(wanted.fields);
      else if(selected) openThreadMenu(); else await openHost();
      break;
    case 'host': await openHost(); break;
    case 'menu': openThreadMenu(); break;
    case 'picker': await openPicker(); break;
    case 'history': openHistory(); break;
    case 'delete': {const t = sessions.find(s => s.id === selected); if(t) deleteThread(t.id, t.name); break;}
    case 'rename': await command('rename',{name:wanted.name}); break;
    case 'effort': await applySettings({effort: wanted.effort}); break;
    case 'model': {
      const current = selected ? threadSettings(snapshot) : newSettings() || {};
      const provider = wanted.provider || current.provider, fields = {provider, model: wanted.model};
      if(wanted.effort) fields.effort = wanted.effort;
      else if(!effortsFor(provider, wanted.model).includes(current.effort)) fields.effort = 'default';
      await applySettings(fields);
      break;
    }
    case 'install': confirmInstall(); break;
    case 'update': confirmUpdate(); break;
    case 'new': await startNew(); break;
    case 'filter': {await home(); $('filter-box').hidden = false; $('filter').value = wanted.query; listSessions(); break;}
    case 'find': search(wanted.query); break;
    case 'export': exportTranscript(); break;
    case 'download': download(); break;
  }
  return true;
}
// Typed settings: a thread's through the `settings` command, a new thread's on this screen.
async function applySettings(fields) {
  if(selected) {
    const busy = snapshot?.busy;
    await command('settings', fields);
    note(`${busy ? 'Applies next turn' : 'Next turn uses'}: ${settingsLine(fields)}.`);
    return;
  }
  const next = {...(newSettings() || {}), ...fields}, extra = newExtras();
  saveNewSettings(next);
  if('compaction' in fields || 'max_usd' in fields) drafts.set('new-extras', JSON.stringify({...extra, ...('compaction' in fields ? {compaction: fields.compaction} : {}), ...('max_usd' in fields ? {max_usd: fields.max_usd} : {})}));
  renderChip();
  note(`New threads use: ${settingsLine(fields)}.`);
}
function newDraft() {try {return JSON.parse(drafts.get('draft:new') || 'null');} catch {return null;}}
// The thread list is where a thread starts: its composer takes the first message.
async function startNew() {
  if(selected) await home();
  $('prompt').focus?.();
}
// A new thread that did not start gives its first message back to the composer.
function openNew(error = '') {
  const draft = newDraft();
  if(draft?.text && !$('prompt').value.trim()) {$('prompt').value = draft.text; drafts.set('draft:home', draft.text); fitPrompt();}
  if(error) toast(`The new thread was refused: ${error}`);
  $('prompt').focus?.();
}
// The first message goes out with the chip's model and the folded thread options.
async function createThread(text) {
  if(!catalog) await loadCatalog().catch(() => {});
  const s = newSettings() || {}, extra = newExtras();
  const form = {name: extra.name || titleFrom(text), text, provider: s.provider, effort: s.effort || 'default', compaction: extra.compaction === 'digest' ? 'digest' : 'model', network: Boolean(s.network), toolchain: Boolean(s.toolchain)};
  if(s.model) form.model = s.model;
  if(typeof extra.max_usd === 'number') form.max_usd = extra.max_usd;
  drafts.set('draft:new', JSON.stringify(form)); lastError = '';
  const receipt = await command('new', form);
  drafts.set('draft:new', JSON.stringify({...form, session: receipt.session}));
  drafts.set('draft:home', ''); drafts.set('new-extras', JSON.stringify({...extra, name: ''}));
  return receipt;
}
function download() {
  if(!snapshot || !selected) return;
  location.href = `/api/sessions/${selected}/archive?revision=${snapshot.revision}`;
}
function exportTranscript() {
  if(!snapshot) {toast('Wait for the thread to load.'); return;}
  const name = sessions.find(s => s.id === selected)?.name || 'Spindle thread';
  const url = URL.createObjectURL(new Blob([transcript(snapshot, name)], {type:'text/markdown'}));
  const link = el('a'); link.href = url; link.download = `${name.replace(/[^\w.-]+/g,'-').slice(0,60) || 'thread'}-r${snapshot.revision}.md`;
  document.body.append(link); link.click(); link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 10000);
}
$('home').onclick = () => home().catch(fail);
$('settings').onclick = () => openHost().catch(fail);
$('title-button').onclick = openThreadMenu;
$('spend').onclick = openThreadMenu;
$('model-chip').onclick = () => openPicker().catch(fail);
$('pending-settings').onclick = () => openPicker().catch(fail);
$('model-search').oninput = renderPicker;
$('model-apply').onclick = () => applyPicker();
$('model-dialog').onclose = () => {picking = null;};
$('picker-network').onchange = () => {if(picking) {picking.network = $('picker-network').checked; commitNew(); renderPicker();}};
$('picker-toolchain').onchange = () => {if(picking) {picking.toolchain = $('picker-toolchain').checked; commitNew(); renderPicker();}};
for(const id of ['new-name','new-compaction','new-budget']) $(id).onchange = commitNew;
// Bottom sheets close on a tap outside them (the backdrop).
for(const id of ['model-dialog','thread-dialog']) $(id).onclick = event => {
  const box = $(id).getBoundingClientRect?.(); if(!box || event.target !== $(id)) return;
  if(event.clientX < box.left || event.clientX > box.right || event.clientY < box.top || event.clientY > box.bottom) $(id).close();
};
$('filter').oninput = listSessions;
for(const button of document.querySelectorAll('[data-close]')) button.onclick = () => $(button.dataset.close).close();
function markTheme() {
  const mode = globalThis.spindleTheme?.get(), texture = globalThis.spindleTheme?.texture?.();
  for(const button of $('theme-choice').querySelectorAll('button')) {const on = button.dataset.mode === mode; button.classList.toggle('active', on); button.setAttribute('aria-pressed', String(on));}
  for(const button of $('texture-choice').querySelectorAll('button')) {const on = button.dataset.texture === texture; button.classList.toggle('active', on); button.setAttribute('aria-pressed', String(on));}
  $('appearance-state').textContent = [mode ? mode[0].toUpperCase() + mode.slice(1) : '', texture === 'plain' ? 'plain' : '', skinState?.active ? 'skin applied' : document.documentElement?.dataset.skin === 'safe' ? 'safe mode' : ''].filter(Boolean).join(' · ');
}
for(const button of $('theme-choice').querySelectorAll('button')) button.onclick = () => {globalThis.spindleTheme?.set(button.dataset.mode); markTheme();};
for(const button of $('texture-choice').querySelectorAll('button')) button.onclick = () => {globalThis.spindleTheme?.setTexture(button.dataset.texture); markTheme();};
// Unpaired, there is nothing behind the pairing dialog: Escape must not dismiss it.
$('pair-dialog').oncancel = event => event.preventDefault();
$('toast').onclick = hideToast;
$('pair-form').onsubmit = async event => {event.preventDefault(); try {await api('/api/pair',{key:$('pair-key').value}); $('pair-key').value=''; $('pair-dialog').close(); reloadSkin(); await init();} catch(error) {$('pair-error').textContent = error.message;}};
// The thread menu's title is its name, edited in place; Rename shows once it differs.
$('rename').oninput = () => {$('rename-save').hidden = !$('rename').value.trim() || $('rename').value.trim() === (sessions.find(s => s.id === selected)?.name || '');};
$('rename-form').onsubmit = event => {event.preventDefault(); if(!$('rename').value.trim()) return; command('rename',{name:$('rename').value}).then(() => {note('Renamed.'); $('rename-save').hidden = true; $('rename').blur?.();}).catch(() => {});};
// An empty budget is unlimited (null clears one); an unchanged field sends nothing.
$('budget-form').onsubmit = event => {
  event.preventDefault();
  const raw = $('settings-budget').value.trim(), max_usd = raw === '' ? null : Number(raw);
  if(max_usd !== null && !(max_usd >= 0)) {toast('The budget must be a dollar amount such as 5 or 0.50, or empty for unlimited.'); return;}
  if(raw === budgetShown.trim()) {note(raw ? `The budget is already ${money(max_usd)}.` : 'No budget is set: spending is unlimited.'); return;}
  command('settings',{max_usd}).then(() => {budgetShown = raw; note(max_usd === null ? 'Budget cleared: spending is unlimited.' : `Budget set to ${money(max_usd)}.`); $('settings-usage').textContent = usageText(snapshot);}).catch(() => {});
};
for(const button of $('compaction-choice').querySelectorAll('button')) button.onclick = () => {
  const compaction = button.dataset.mode;
  if(threadSettings(snapshot).compaction === compaction) return;
  command('settings',{compaction}).then(() => {markCompaction(compaction); note(compaction === 'digest' ? 'History compaction: digest (no extra request).' : 'History compaction: model summary.');}).catch(() => {});
};
$('files').onclick = () => {$('thread-dialog').close(); openFiles();};
$('menu-history').onclick = () => {$('thread-dialog').close(); openHistory();};
$('menu-find').onclick = () => {$('thread-dialog').close(); $('findbar').hidden = false; $('find-text').focus?.();};
$('menu-note').onclick = () => {$('thread-dialog').close(); openAttend();};
$('attention').onclick = openAttend;
// A note for the next turn: written in the operator's words, read once at
// admission, then gone. Held looks are named so the note lands beside them.
function openAttend() {
  if(!selected) return;
  const held = attention?.held || 0;
  $('attend-held').textContent = held ? `Alongside this note, the next turn also learns which files you held open: ${held} look${held === 1 ? '' : 's'}. Both are added to your next message, where you can see them.` : 'The next turn also learns which files you held open in the viewer.';
  if(!$('attend-text').value && !$('attend-dialog').open) $('attend-text').value = (attention?.notes || []).join('\n\n');
  if(!$('attend-dialog').open) $('attend-dialog').showModal();
  $('attend-text').focus?.();
}
$('attend-save').onclick = () => {
  const text = $('attend-text').value.trim(); if(!text) {toast('Write the note first.'); return;}
  api(`/api/sessions/${selected}/attention`, {text}).then(() => {
    renderAttention({held: attention?.held || 0, notes: [text]}); $('attend-dialog').close(); $('attend-text').value = '';
    note('Kept for the next turn. It is read once, then forgotten.');
  }).catch(fail);
};
$('attend-clear').onclick = () => api(`/api/sessions/${selected}/attention`, {clear: true}).then(() => {
  renderAttention({held: attention?.held || 0, notes: []}); $('attend-dialog').close(); $('attend-text').value = ''; note('Note cleared.');
}).catch(fail);
$('menu-change').onclick = () => {$('thread-dialog').close(); const card = $('history').querySelector?.('.change-card'); following = false; card?.scrollIntoView?.({block:'start'});};
$('delete').onclick = () => {const thread = sessions.find(s => s.id === selected); if(thread) deleteThread(thread.id, thread.name);};
$('skin-stage').onclick = () => skinAction({action:'stage',css:$('skin-css').value}).then(() => note('Staged. Preview it, then Apply.')).catch(fail);
// The open thread's viewed revision, or from the list the head of the thread last opened.
$('skin-thread').onclick = async () => {
  try {
    const session = selected || lastThread?.id; if(!session || (selected && !snapshot)) return;
    const revision = selected ? snapshot.revision : (await api(`/api/sessions/${session}/snapshot`)).head;
    const s = await skinAction({action:'stage',session,revision});
    $('skin-css').value = s.staged.css; note(`Staged ${s.workspace_file} from r${revision}. Preview it, then Apply.`);
  } catch(error) {fail(error);}
};
$('skin-preview').onclick = () => window.open('/?preview=1','_blank','noopener');
$('skin-apply').onclick = () => {const s = skinState; if(!s?.staged) return; confirmAction('Apply this skin?',`Staged skin ${s.staged.digest} (${s.staged.bytes} B) becomes the host skin for every paired screen. Dialogs and alerts keep the built-in look. The previous skin stays in versions; /?safe opens without any skin.`,async () => {await skinAction({action:'apply',digest:s.staged.digest}); location.reload();});};
$('skin-discard').onclick = () => skinAction({action:'discard'}).then(() => {$('skin-css').value = ''; note('Staged skin discarded.');}).catch(fail);
$('skin-off').onclick = () => confirmAction('Turn the skin off?','Every screen returns to the built-in look. Versions are kept.',async () => {await skinAction({action:'off'}); location.reload();});
$('unpair').onclick = () => confirmAction('Forget this pairing?','This screen is signed out on the host (its device token is revoked) and needs the pairing key again. Other screens stay paired.',async () => {await api('/api/unpair',{}); location.reload();});
$('notify').onclick = () => toggleNotify().catch(fail);
$('restart').onclick = () => confirmAction('Restart the host?','The host process exits and its supervisor starts it again; screens reconnect by themselves. Only possible while no thread is running.',async () => {await api('/api/restart',{}); $('settings-dialog').close(); note('Restarting. This screen reconnects when the host is back.');});
// While a turn runs, a message goes to that turn: the worker hands it to the model at
// the next tool boundary, before the final answer or before the next request (and makes
// leftovers the next prompt). Until then it waits under the run, where Send now cuts
// the reply being written so it is read at once.
async function steer(id, text) {
  const reply = await api(`/api/sessions/${id}/steer`,{text});
  if(selected === id && $('prompt').value.trim() === text) {$('prompt').value = ''; fitPrompt(); renderStop();}
  if(drafts.get(`draft:${id}`).trim() === text) drafts.set(`draft:${id}`,'');
  // Shown at once, as the host says it waits (its `queued` event may come first).
  if(selected === id) {queued = reply.waiting?.messages ? reply.waiting : {messages:[...(queued?.messages || []), text], now: Boolean(queued?.now)}; renderQueued(); scroll();}
  if(attached.length) note('Sent to the running turn. Attachments wait for the next message.');
}
async function sendNow() {
  const id = selected; if(!id || !queued?.messages?.length) {toast('No message is waiting for the run.'); return;}
  const reply = await api(`/api/sessions/${id}/steer`,{now:true});
  if(selected === id) {queued = reply.waiting?.messages ? reply.waiting : queued && {...queued, now: true}; renderQueued();}
}
// What waits for the running turn: the messages, then how they will be read.
function renderQueued() {
  const area = $('queued'), list = historical ? [] : queued?.messages || [];
  area.replaceChildren(); area.hidden = !list.length;
  if(!list.length) return;
  for(const text of list) message(area,'user',text,false,queued.now ? 'YOU · SENDING NOW' : 'YOU · QUEUED');
  const row = el('div','queued-row');
  if(queued.now) row.append(el('span','meta','Cutting the reply being written so this is read now.'));
  else {
    const now = el('button','','Send now'); now.type = 'button'; now.onclick = () => {now.disabled = true; sendNow().catch(error => {now.disabled = false; fail(error);});};
    row.append(el('span','meta','Read after the current step.'), now);
  }
  area.append(row);
}
const workspacePath = path => Boolean(path) && path.length <= 240 && !path.startsWith('/') && !path.includes('\\') && path.split('/').every(part => part && part !== '.' && part !== '..');
const MAX_UPLOAD = 8 * 1024 * 1024;
async function attach(files) {
  const id = selected; if(!id) return;
  for(const file of files) {
    if(selected !== id) break;
    if(attached.length >= 32) {toast('At most 32 attachments per message.'); break;}
    const entry = {name: file.name, path: file.name.replace(/[\\/]/g,'_'), size: file.size};
    if(!file.size || file.size > MAX_UPLOAD) {toast(`${file.name}: a file must be 1 byte to 8 MiB.`); continue;}
    attached.push(entry); renderAttachments();
    try {const saved = await api(`/api/sessions/${id}/upload`, file, true); if(selected === id) entry.digest = saved.digest;}
    catch(error) {entry.error = error.message; toast(`${file.name}: ${error.message}`);}
    if(selected === id) renderAttachments();
  }
}
function renderAttachments() {
  $('attachments').hidden = !attached.length;
  $('attachments').replaceChildren(...attached.map(entry => {
    const chip = el('div', `chip${entry.error ? ' failed' : entry.digest ? '' : ' uploading'}`);
    const path = el('input'); path.value = entry.path; path.setAttribute('aria-label', `Workspace path for ${entry.name}`); path.spellcheck = false;
    path.oninput = () => {entry.path = path.value;};
    const remove = el('button','','×'); remove.type = 'button'; remove.setAttribute('aria-label', `Remove ${entry.name}`);
    remove.onclick = () => {attached = attached.filter(other => other !== entry); renderAttachments();};
    chip.append(path, el('small','', entry.error ? 'failed' : entry.digest ? size(entry.size) : 'uploading…'), remove);
    return chip;
  }));
}
// Providers: sign-in state and Dext's own sign-in flows. The credential lives only in
// the form field and the one request that uses it.
let signing = null;
async function loadLogins() {
  try {renderLogins((await api('/api/logins')).logins || []);} catch(error) {$('logins').replaceChildren(el('p','note',error.message));}
}
function renderLogins(logins) {
  const needed = logins.filter(login => login.status !== 'not-required');
  $('logins-state').textContent = needed.length ? `${needed.filter(login => login.signed_in).length} of ${needed.length} signed in` : '';
  // A row each: a filled square signed in (a hollow one when not, or when none is needed),
  // its state, and icons to sign in (again) and out.
  $('logins').replaceChildren(...logins.map(login => {
    const name = login.name || login.provider, needed = login.status !== 'not-required';
    const row = el('div', `login-row${login.signed_in && needed ? ' ok' : ''}`);
    const state = login.signed_in ? (!needed ? 'no sign-in needed' : login.status.startsWith('env:') ? `signed in (${login.status.slice(4)})` : 'signed in') : login.status === 'auth(unresolved)' ? 'credential unresolved' : 'not signed in';
    const mark = el('span','login-mark'); mark.setAttribute('aria-hidden','true');
    row.append(mark, el('span','login-name',name), el('small', login.signed_in ? 'ok' : '', state));
    const action = (label, glyph, run, cls = '') => {const button = el('button', cls); button.type = 'button'; button.setAttribute('aria-label', label); button.title = label; button.append(icon(glyph, 17)); button.onclick = run; row.append(button);};
    if(needed && !login.status.startsWith('env:')) {
      if(login.signed_in && login.status === 'auth') {
        action(`Sign in to ${name} again`, 'retry', () => openSignIn(login));
        action(`Sign out of ${name}`, 'signout', () => signOut(login), 'danger');
      } else action(`Sign in to ${name}`, 'signin', () => openSignIn(login));
    }
    return row;
  }));
}
function openSignIn(login) {
  signing = login;
  $('login-title').textContent = `SIGN IN · ${login.name || login.provider}`;
  showLoginMessage('Paste an API key or token, or start the provider\'s web sign-in.');
  $('login-label').textContent = 'API key or token';
  $('login-credential').value = ''; $('login-form').hidden = false; $('login-credential').focus();
}
function closeSignIn() {signing = null; $('login-credential').value = ''; $('login-form').hidden = true;}
function showLoginMessage(text) {
  $('login-message').replaceChildren(...linkParts(text).map(part => {
    if(!part.url) return document.createTextNode(part.text);
    const link = el('a','',part.url); link.href = part.url; link.target = '_blank'; link.rel = 'noopener noreferrer'; return link;
  }));
}
async function signIn(credential) {
  const login = signing; if(!login) return;
  try {
    const result = await api('/api/login', credential ? {provider: login.provider, credential} : {provider: login.provider});
    if(signing !== login) return;
    $('login-credential').value = '';
    if(result.awaiting) {showLoginMessage(result.message); $('login-label').textContent = 'Paste the callback URL or code'; $('login-credential').focus(); return;}
    closeSignIn(); note(result.message || 'Signed in.');
    // The catalog's sign-in states (and a fresh provider's models) follow at once.
    await loadLogins(); fallback = providersOf((await api('/api/sessions')).providers); await loadCatalog(true); renderChip();
  } catch(error) {if(signing === login) showLoginMessage(error.message);}
}
function signOut(login) {
  confirmAction(`Sign out of ${login.name || login.provider}?`,'The host removes the stored credential from Dext\'s credential store. Threads using this provider cannot send model requests until it is signed in again.',async () => {
    const result = await api('/api/logout',{provider: login.provider}); note(result.message || 'Signed out.'); await loadLogins();
  });
}
$('login-form').onsubmit = event => {event.preventDefault(); const credential = $('login-credential').value.trim(); if(!credential) {showLoginMessage('Paste a key, token, or the callback URL/code first.'); return;} signIn(credential);};
$('login-start').onclick = () => signIn('');
$('login-cancel').onclick = closeSignIn;
$('settings-dialog').onclose = closeSignIn;
$('attach').onclick = () => {if(!selected) {toast('Open a thread first.'); return;} $('attach-input').click();};
$('attach-input').onchange = () => {const files = [...($('attach-input').files || [])]; $('attach-input').value = ''; attach(files).catch(fail);};
let inflight = '';
$('composer').onsubmit = async event => {
  event.preventDefault(); const text = $('prompt').value.trim(); if(!text) return;
  // A second tap while the first is on its way would steer twice; commands stay free
  // (typing stop must work while a message is outstanding).
  if(inflight === text) return;
  try {
    const id = selected, current = generation;
    // On the thread list the composer starts a thread (typed commands still work there).
    if(!id) {
      if(await runIntent(text)) {if(!selected && $('prompt').value.trim() === text) {$('prompt').value = ''; drafts.set('draft:home',''); fitPrompt();}}
      else {inflight = text; await createThread(text);}
      return;
    }
    if(!snapshot) throw Error('Wait for the thread to load.');
    if(await runIntent(text)) {if(id === selected && current === generation && $('prompt').value.trim() === text) {$('prompt').value = ''; drafts.set(`draft:${id}`,''); fitPrompt();}}
    else if(snapshot.busy && !historical) {inflight = text; await steer(id, text);}
    else if(!snapshot.complete || historical) toast('This thread is not ready for a new message; the note at its end says why. Your draft is kept.');
    else {
      if(attached.some(file => !file.digest)) throw Error(attached.some(file => file.error) ? 'Remove the attachments that failed to upload first.' : 'Wait for the attachments to finish uploading.');
      const files = attached.map(file => ({path: file.path.trim(), digest: file.digest}));
      const wrong = files.find(file => !workspacePath(file.path));
      if(wrong) throw Error(`“${wrong.path}” is not a workspace path: relative, no “..”, no leading “/”.`);
      const twice = files.find((file, i) => files.findIndex(other => other.path === file.path) !== i);
      if(twice) throw Error(`Two attachments would both be written to “${twice.path}”; rename one.`);
      inflight = text;
      await command('say',files.length ? {text, attachments: files} : {text});
      if(selected === id) {attached = []; renderAttachments();}
    }
  } catch(error) {fail(error);}
  finally {inflight = '';}
};
$('prompt').oninput = () => {drafts.set(selected ? `draft:${selected}` : 'draft:home',$('prompt').value); fitPrompt(); renderStop();};
// Ctrl/Cmd+Enter sends from a keyboard; Enter alone is a new line.
$('prompt').onkeydown = event => {if(event.key === 'Enter' && (event.ctrlKey || event.metaKey)) {event.preventDefault(); $('composer').requestSubmit?.();}};
$('stop').onclick = () => command('stop').catch(() => {});
$('continue').onclick = () => command('continue').catch(() => {});
$('retry').onclick = retryThread;
$('recover').onclick = confirmRecover;
$('phase').onclick = explainStatus;
$('bar-signin').onclick = () => signinProvider().catch(fail);
$('bar-rewind').onclick = () => rewindThread(snapshot.revision);
$('bar-live').onclick = () => backToLive().catch(fail);
$('hist-prev').onclick = async () => {const target = historical ? snapshot.revision - 1 : snapshot.head - 1; if(target >= 1) await inspect(target);};
$('hist-next').onclick = async () => {if(historical && snapshot.revision < snapshot.head) await inspect(snapshot.revision + 1);};
$('hist-chip').onclick = openHistory;
$('hist-live').onclick = () => backToLive().catch(fail);
$('hist-rewind').onclick = () => rewindThread(snapshot.revision);
$('find-form').onsubmit = event => {event.preventDefault(); const query = $('find-query').value.trim(); if(!query) return; $('history-dialog').close(); search(query);};
$('findbar').onsubmit = event => {event.preventDefault(); search($('find-text').value.trim());};
$('find-prev').onclick = () => step(-1);
$('find-next').onclick = () => step(1);
$('find-close').onclick = closeSearch;
// Only the reader scrolling up leaves the live end: content that grows after a jump to the
// end (a preview loading) moves the end away without anyone scrolling.
let lastTop = 0;
$('main').onscroll = () => {
  const main = $('main'), top = main.scrollTop;
  if(main.scrollHeight - top - main.clientHeight < 110) following = true;
  else if(top < lastTop - 2) following = false;
  lastTop = top;
  $('jump').hidden = following || !selected;
};
$('jump').onclick = () => {following = true; $('jump').hidden = true; scroll();};
// Cards that load later (previews, images, Markdown) grow the thread after the jump to
// its end; a reader at the end stays there.
// A card the reader opens grows the thread too: that is read from its top, not followed.
let toggledAt = 0;
$('thread').addEventListener?.('toggle', () => {toggledAt = Date.now();}, true);
if(typeof ResizeObserver === 'function') new ResizeObserver(() => {
  if(!following || !selected) return;
  if(Date.now() - toggledAt < 600) {following = $('main').scrollHeight - $('main').scrollTop - $('main').clientHeight < 110; $('jump').hidden = following; return;}
  $('main').scrollTop = $('main').scrollHeight;
}).observe($('thread'));
$('download').onclick = () => {$('thread-dialog').close(); download();};
$('export').onclick = () => {$('thread-dialog').close(); exportTranscript();};
// A file of a revision as text (null when the revision has no such file).
async function fileText(id, revision, path) {
  const response = await fetch(`/api/sessions/${id}/file?revision=${revision}&path=${encodeURIComponent(path)}`, {redirect:'manual'});
  if(response.type === 'opaqueredirect') {signin(); throw Error('Sign in again to continue.');}
  return response.ok ? response.text() : null;
}
// Preview links of thread revisions (`/api/preview/TOKEN/`): a page's relative CSS,
// scripts and images load through one. Each is reused until a minute before it lapses.
// Cards of one revision that ask at once share the request (the host keeps at most 32).
const previewLinks = new Map();
async function previewUrl(id, revision, path, network = 'none', scripts = false) {
  // A link that runs scripts runs them on its own page only (GitHub access runs them),
  // or, where previews are contained (`'all'`), on every page of the revision, and only
  // where the browser keeps them offline (the host stops them anywhere else).
  const runs = scripts === 'all' && network === 'none' ? true : scripts || network === 'github' ? path : null;
  const key = `${id}:${revision}:${network}:${runs === true ? '*' : runs ?? ''}`;
  let link = previewLinks.get(key);
  if(!link || link.until <= Date.now()) {
    const minting = api(`/api/sessions/${id}/preview`, {revision, ...(network !== 'none' && {network}), ...(runs !== null && {scripts: runs}), ...(runs === true && {contained: true})}).then(grant => ({url: grant.url, until: Date.now() + Math.max(0, grant.expires_in - 60) * 1000}));
    link = {pending: minting, until: Infinity};
    previewLinks.set(key, link);
    try {link = await minting; if(previewLinks.get(key)?.pending === minting) previewLinks.set(key, link);}
    catch(error) {if(previewLinks.get(key)?.pending === minting) previewLinks.delete(key); throw error;}
  } else if(link.pending) link = await link.pending;
  return link.url + path.split('/').map(encodeURIComponent).join('/');
}
const renderUrl = (id, revision, path) => `/api/sessions/${id}/file?revision=${revision}&path=${encodeURIComponent(path)}&render=1`;
// Whether this browser keeps a running preview offline. A preview carries a
// Connection-Allowlist naming only its own files (preview.rs), and Chrome (152 and later)
// holds every connection of the page to it, WebRTC, hints and navigations included, which
// no page policy can; the host's check page tells whether it does here (asked as the app
// starts, and again after a while when it only timed out: a slow answer is no answer).
let containing = null;
let containingUntil = Infinity;
function previewsContained() {
  if(containing && Date.now() > containingUntil) containing = null;
  // One check at a time: until it answers, every caller waits for it.
  if(!containing) containingUntil = Infinity;
  containing ??= new Promise(resolve => {
    // Without WebRTC there is no telling.
    if(typeof RTCPeerConnection !== 'function') {resolve({contained: false, sure: true}); return;}
    const frame = el('iframe'); frame.hidden = true; frame.title = 'Preview check'; frame.setAttribute('sandbox', 'allow-scripts');
    let timer = setTimeout(() => done(false, false), 5000);
    const done = (contained, sure) => {window.removeEventListener('message', heard); clearTimeout(timer); frame.remove(); resolve({contained, sure});};
    const heard = ({source, data}) => {if(source === frame.contentWindow && typeof data?.spindleContained === 'boolean') done(data.spindleContained, true);};
    // Loaded without a word (an error page): a no, once its word would have come.
    frame.onload = () => {clearTimeout(timer); timer = setTimeout(() => done(false, false), 1000);};
    window.addEventListener('message', heard);
    frame.src = '/api/preview/check';
    document.body.append(frame);
  }).then(({contained, sure}) => {
    containingUntil = contained || sure ? Infinity : Date.now() + 30_000;
    return contained;
  });
  return containing;
}
// Whether a page's scripts run in its preview: where previews are contained, always (and
// so every page of its revision's, `'all'`); else as the user let this version of it.
async function scriptsRun(id, revision, path) {return await previewsContained() ? 'all' : scriptsLet(id, revision, path);}
// Where previews are not contained, a page's scripts run in its preview only once let, for
// that version of it: the page, the scripts it loads, and the files it or they name (what
// they fetch is meant to be among them); any of those changing asks again, as a script
// could send what the page shows out of the browser. Kept on this device; the link the
// host gives runs that page's scripts alone.
const versions = new Map();
const PASSIVE = /\.(png|jpe?g|gif|webp|avif|ico|bmp|woff2?|ttf|otf|mp3|mp4|webm|wav|ogg)$/i;
function pageVersion(id, revision, path) {
  const key = `${id}:${revision}:${path}`;
  if(!versions.has(key)) versions.set(key, (async () => {
    const files = await filesAt(id, revision);
    const listed = new Map(files.map(file => [file.path, file]));
    const text = listed.has(path) ? await fileText(id, revision, path) : null;
    if(text === null) return null;
    const dir = path.includes('/') ? path.slice(0, path.lastIndexOf('/') + 1) : '';
    const local = ref => {
      if(!ref || /^([a-z][a-z0-9+.-]*:|\/\/|#)/i.test(ref.trim())) return null;
      try {const name = decodeURIComponent(new URL(ref.trim(), `https://workspace.invalid/${dir}`).pathname.slice(1)); return listed.has(name) ? name : null;}
      catch {return null;}
    };
    const doc = new DOMParser().parseFromString(text, 'text/html');
    const scripts = [...new Set([...doc.querySelectorAll('script[src]')].map(script => local(script.getAttribute('src'))).filter(Boolean))].sort();
    const said = [text, ...await Promise.all(scripts.map(name => fileText(id, revision, name).then(code => code ?? '')))].join('\n');
    const named = files.filter(file => file.path !== path && !scripts.includes(file.path) && !PASSIVE.test(file.path) && said.includes(file.path.split('/').pop())).map(file => file.path).sort();
    const parts = [listed.get(path).digest, ...[...scripts, ...named].map(name => `${name}:${listed.get(name).digest}`)].join('|');
    try {return [...new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(parts)))].map(byte => byte.toString(16).padStart(2,'0')).join('');}
    catch {return parts;}
  })().catch(error => {versions.delete(key); throw error;}));
  return versions.get(key);
}
async function scriptsLet(id, revision, path) {
  try {
    const kept = JSON.parse(localStorage.getItem(`spindle-preview:${id}:${path}`) || 'null');
    if(!kept?.scripts) return false;
    const version = await pageVersion(id, revision, path);
    return Boolean(version) && kept.version === version;
  } catch {return false;}
}
async function letScripts(id, revision, path, scripts) {
  try {localStorage.setItem(`spindle-preview:${id}:${path}`, JSON.stringify({version: await pageVersion(id, revision, path), scripts}));} catch {}
}
// The file list of a revision never changes once committed: fetched once per thread.
const filesCache = new Map();
async function filesAt(id, revision) {
  const key = `${id}:${revision}`;
  if(!filesCache.has(key)) filesCache.set(key, api(`/api/sessions/${id}/files?revision=${revision}`).then(data => data.files || []).catch(error => {filesCache.delete(key); throw error;}));
  return filesCache.get(key);
}
// Work done when a card first nears the screen: preview tokens and file reads are spent
// only on what is looked at (a long thread could otherwise mint more than the host keeps).
const waitingToShow = new WeakMap();
const nearby = typeof IntersectionObserver === 'function' ? new IntersectionObserver(entries => {
  for(const entry of entries) if(entry.isIntersecting) {nearby.unobserve(entry.target); const work = waitingToShow.get(entry.target); waitingToShow.delete(entry.target); work?.();}
}, {root: $('main'), rootMargin: '600px 0px'}) : null;
function whenVisible(node, work) {if(!nearby) {work(); return;} waitingToShow.set(node, work); nearby.observe(node);}
const KIND_LABELS = {html: 'page', svg: 'svg', image: 'image', markdown: 'markdown', pdf: 'pdf', text: 'text', file: 'file'};
function kindOf(path, kind = '') {
  if(KIND_LABELS[kind] && kind !== 'file') return kind;
  const ext = String(path).split('.').pop().toLowerCase();
  if(ext === 'html' || ext === 'htm') return 'html';
  if(ext === 'svg') return 'svg';
  if(['png','jpg','jpeg','gif','webp','avif','ico','bmp'].includes(ext)) return 'image';
  if(ext === 'md' || ext === 'markdown') return 'markdown';
  if(ext === 'pdf') return 'pdf';
  return kind === 'file' ? 'file' : 'text';
}
// What a tool result shows, as a card in the conversation. The same card is reused when
// the thread re-renders, so a loaded preview does not reload.
const displayNodes = new Map();
function displayCard(item, id) {
  const revision = Number(item.revision) || snapshot?.revision || 0, path = String(item.path), kind = kindOf(path, item.kind);
  const key = `${id}:${revision}:${path}:${kind}`;
  if(displayNodes.has(key)) return displayNodes.get(key);
  const title = String(item.title || path.split('/').pop());
  const card = el('figure', `display-card display-${kind}`); card.dataset.path = path; card.dataset.kind = kind;
  const head = el('figcaption', 'display-head'), label = el('span', 'display-label');
  label.append(el('strong', 'display-title', title), el('small', 'display-meta', [title !== path ? path : '', typeof item.size === 'number' ? size(item.size) : '', `r${revision}`].filter(Boolean).join(' · ')));
  const view = () => openViewer(id, revision, path, {kind}).catch(fail);
  const expand = el('button', 'display-expand', '⤢'); expand.type = 'button'; expand.setAttribute('aria-label', `Open ${title} full screen`); expand.onclick = view;
  head.append(el('span', 'badge', KIND_LABELS[kind]), label, expand);
  const body = el('div', 'display-body');
  const unavailable = error => body.replaceChildren(el('p', 'note', `Preview unavailable${error?.message ? `: ${error.message}` : ''}.`));
  // Inline previews are pictures of the file: a tap opens the full, interactive view.
  const cover = () => {const button = el('button', 'display-open'); button.type = 'button'; button.setAttribute('aria-label', `Open ${title}`); button.onclick = view; return button;};
  if(kind === 'html') {
    const frame = el('iframe'); frame.title = `Preview of ${title}`; frame.tabIndex = -1;
    frame.setAttribute('sandbox', 'allow-scripts allow-forms allow-modals'); frame.setAttribute('referrerpolicy', 'no-referrer'); frame.setAttribute('loading', 'lazy');
    body.append(frame, cover());
    whenVisible(card, () => scriptsRun(id, revision, path).then(scripts => previewUrl(id, revision, path, 'none', scripts)).then(url => {frame.src = url;}, () => {frame.src = renderUrl(id, revision, path);}));
  } else if(kind === 'image' || kind === 'svg') {
    const image = el('img'); image.alt = title; image.decoding = 'async';
    body.append(image, cover());
    whenVisible(card, () => previewUrl(id, revision, path).then(url => {image.src = url;}).catch(unavailable));
  } else if(kind === 'markdown' || kind === 'text') {
    body.append(el('p', 'note', 'Loading…'));
    whenVisible(card, () => fileText(id, revision, path).then(text => {
      if(text === null) throw Error('the file is not in this revision');
      const long = text.split('\n').length > 14 || text.length > 1200;
      if(kind === 'markdown') body.replaceChildren(mdNodes(el('div', 'md'), markdown(text)));
      else body.replaceChildren(el('pre', 'display-pre', text.split('\n').slice(0, 14).join('\n')));
      if(long) {body.classList.add('clipped'); const more = el('button', 'display-more', kind === 'markdown' ? 'Read all' : 'Show all'); more.type = 'button'; more.onclick = view; body.append(more);}
    }).catch(unavailable));
  } else if(kind === 'pdf') {
    const link = el('a', 'display-link', 'Open PDF ↗'); link.target = '_blank'; link.rel = 'noopener noreferrer'; link.setAttribute('aria-disabled', 'true');
    body.append(link);
    whenVisible(card, () => previewUrl(id, revision, path).then(url => {link.href = url; link.removeAttribute('aria-disabled');}).catch(unavailable));
  } else {
    const open = el('button', 'display-more', 'View file'); open.type = 'button'; open.onclick = view; body.append(open);
  }
  card.append(head, body);
  if(displayNodes.size >= 80) displayNodes.delete(displayNodes.keys().next().value);
  displayNodes.set(key, card);
  return card;
}
// One viewer for every file: over the conversation (a tapped path or card) or over
// Files (with a way back to the list). It fills the screen; the preview takes what the
// header leaves.
let viewing = null;
const viewed = token => viewing === token && $('viewer-dialog').open && selected === token.id;
function clearViewer() {
  for(const id of ['file-html','file-image','file-md','file-content']) $(id).hidden = true;
  $('file-html').removeAttribute('src'); $('file-image').removeAttribute('src');
  $('file-md').replaceChildren(); $('file-content').replaceChildren();
  $('file-view-toggle').hidden = true; $('file-view-network').hidden = true; $('file-view-network').onclick = null; $('file-view-scripts').hidden = true; $('file-view-scripts').onclick = null; $('file-open').hidden = true; $('file-open').removeAttribute('href');
}
async function openViewer(id, revision, path, options = {}) {
  if(viewing && viewing.id === id && viewing.path !== path) attend('file', viewing.path, viewing.at);
  const token = viewing = {id, revision, path, kind: kindOf(path, options.kind), network: 'none', scripts: undefined, from: options.from || 'chat', base: options.base, at: Date.now()};
  attend('file', path);
  // The shelf keeps this head's bytes for every thread; an old revision is past its reach.
  $('file-view-shelf').hidden = historical || !snapshot || revision !== snapshot.head;
  $('file-view-shelf').onclick = () => api(`/api/sessions/${id}/shelf`, {path}).then(kept => note(`Kept on the shelf: ${kept.kept}. Every thread reads it.`)).catch(fail);
  $('viewer-back').hidden = token.from !== 'files';
  const folder = path.includes('/') ? path.slice(0, path.lastIndexOf('/') + 1) : '';
  $('file-view-name').textContent = path.split('/').pop(); $('file-view-meta').textContent = `${folder}${folder ? ' · ' : ''}r${revision}`;
  $('viewer-dialog').dataset.kind = token.kind;
  clearViewer();
  if(!$('viewer-dialog').open) $('viewer-dialog').showModal();
  await (options.diff ? showDiff(token) : showFile(token));
}
async function showFile(token) {
  const {id, revision, path, kind} = token;
  clearViewer();
  $('file-view-diff').hidden = revision <= 1; $('file-view-diff').textContent = 'Diff'; $('file-view-diff').onclick = () => showDiff(token);
  try {
    if(kind === 'html' || kind === 'svg' || kind === 'image' || kind === 'pdf') {
      // A page renders as a site of this revision (sandboxed, with its sibling files);
      // without a preview link (an older host) a page or SVG still renders on its own.
      let src = '', site = false;
      // Whether this version runs, asked once the viewer shows it (another file opened
      // meanwhile takes over): always where previews are contained.
      if(kind === 'html' && token.scripts === undefined) {
        token.scripts = await scriptsRun(id, revision, path);
        if(!viewed(token)) return;
      }
      try {src = await previewUrl(id, revision, path, token.network, token.scripts === 'all' ? 'all' : Boolean(token.scripts)); site = true;}
      catch(error) {if(error.status === 0 || error.status === 401) throw error; if(kind === 'html' || kind === 'svg') src = renderUrl(id, revision, path);}
      if(!viewed(token)) return;
      if(site) {$('file-open').href = src; $('file-open').hidden = false;}
      if(site && kind === 'html') {
        const scripts = $('file-view-scripts'); scripts.hidden = token.network === 'github' || token.scripts === 'all';
        scripts.textContent = token.scripts ? 'Stop scripts' : 'Run scripts';
        scripts.title = token.scripts ? 'Show this page without running its scripts' : "Let this version of the page run its scripts (only its own files; this browser cannot keep a running page offline, so a script could send what the page shows out of it)";
        scripts.onclick = () => {
          if(!viewed(token)) return;
          token.scripts = !token.scripts;
          letScripts(id, revision, path, token.scripts).then(() => showFile(token)).catch(fail);
        };
        const control = $('file-view-network'); control.hidden = false;
        control.textContent = token.network === 'github' ? 'Use isolated preview' : 'GitHub access';
        control.onclick = () => {
          const enable = token.network !== 'github';
          const apply = async () => {
            if(!viewed(token)) return;
            token.network = enable ? 'github' : 'none';
            await showFile(token);
          };
          if(enable) confirmAction('Allow this preview to contact GitHub?', 'This page can send its contents and anything you type to api.github.com, and load avatars from avatars.githubusercontent.com. This does not grant host cookies or host API access. Only this preview link gets access, for up to 30 minutes; other previews stay isolated.', apply, {yes:'Allow GitHub'});
          else apply().catch(fail);
        };
      }
      if(!src) throw Error('No preview link for this file');
      if(kind === 'pdf') {$('file-content').textContent = 'A PDF opens in its own tab: Open ↗.'; $('file-content').hidden = false;}
      else if(kind === 'image' || (kind === 'svg' && site)) {$('file-image').alt = path; $('file-image').src = src; $('file-image').hidden = false;}
      else {$('file-html').src = src; $('file-html').hidden = false;}
      if(kind === 'html' || kind === 'svg') sourceToggle(token, () => {$('file-html').hidden = $('file-image').hidden = true;}, () => {$(site && kind === 'svg' ? 'file-image' : 'file-html').hidden = false;});
      return;
    }
    const text = await fileText(id, revision, path);
    if(!viewed(token)) return;
    if(text === null) throw Error('File unavailable');
    if(kind === 'markdown') {
      mdNodes($('file-md'), markdown(text)); $('file-md').hidden = false;
      sourceToggle(token, () => {$('file-md').hidden = true;}, () => {$('file-md').hidden = false;}, text);
    } else {$('file-content').textContent = text; $('file-content').hidden = false; $('file-content').focus?.();}
  } catch(error) {fail(error);}
}
// Rendered ⇄ source for pages, SVG and Markdown; the source is read when first asked for.
function sourceToggle(token, hide, show, known = null) {
  let rendered = true, text = known;
  $('file-view-toggle').hidden = false; $('file-view-toggle').textContent = 'Source';
  $('file-view-toggle').onclick = async () => {
    rendered = !rendered;
    $('file-view-toggle').textContent = rendered ? 'Source' : 'Rendered';
    if(rendered) {$('file-content').hidden = true; show(); return;}
    try {text ??= await fileText(token.id, token.revision, token.path);} catch(error) {fail(error); return;}
    if(!viewed(token) || rendered) return;
    hide(); $('file-content').textContent = text ?? 'File unavailable'; $('file-content').hidden = false;
  };
}
// The revision before this file's last change (up to 24 revisions back, read from the
// revision file lists); the one before when its digest is unknown.
async function previousVersion(id, revision, path) {
  const digest = snapshot?.revision === revision ? (snapshot.files || []).find(f => f.path === path)?.digest : undefined;
  if(!digest) return revision - 1;
  for(let at = revision - 1; at >= 1 && at >= revision - 24; at--) {
    const file = (await filesAt(id, at)).find(f => f.path === path);
    if(!file || file.digest !== digest) return at;
  }
  return Math.max(1, revision - 24);
}
async function showDiff(token) {
  const {id, revision, path} = token;
  $('file-view-diff').disabled = true;
  try {
    const base = token.base && token.base < revision ? token.base : await previousVersion(id, revision, path);
    const [before, after] = await Promise.all([fileText(id, base, path), fileText(id, revision, path)]);
    if(!viewed(token)) return;
    clearViewer();
    $('file-view-meta').textContent = `${path} · r${base} → r${revision}`;
    $('file-view-diff').textContent = 'View'; $('file-view-diff').onclick = () => {$('file-view-meta').textContent = `r${revision}`; showFile(token);};
    $('file-content').hidden = false;
    const rows = unified(before ?? '', after ?? '');
    if(rows === null) {$('file-content').textContent = 'This change is too large to show as a diff.'; return;}
    if(!rows.length) {$('file-content').textContent = `No change between r${base} and r${revision}.`; return;}
    const note = before === null ? `New file at r${revision}.\n` : after === null ? `Deleted at r${revision}.\n` : '';
    $('file-content').replaceChildren(...(note ? [el('span','diff-hunk',note)] : []), ...rows.map(([kind, line]) => el('span', kind === '+' ? 'diff-add' : kind === '-' ? 'diff-del' : kind === '@' ? 'diff-hunk' : '', `${kind === '@' ? '' : kind}${line}\n`)));
  } catch(error) {fail(error);}
  finally {$('file-view-diff').disabled = false;}
}
$('viewer-back').onclick = () => $('viewer-dialog').close();
$('viewer-close').onclick = () => {const from = viewing?.from; $('viewer-dialog').close(); if(from === 'files') $('files-dialog').close();};
$('viewer-dialog').onclose = () => {if(viewing) attend('file', viewing.path, viewing.at); viewing = null; clearViewer();};
// Files: the whole sheet is the list, a folder at a time (each shut until tapped, a lone
// folder chain on one row); a long list filters to the paths that match; a file opens the
// viewer. What the revision changed is marked, on files and on the folders holding them.
let fileSheet = null; const openFolders = new Map(), ROWS_AT_ONCE = 400;
async function openFiles() {
  if(!snapshot) {toast('Wait for the thread to load.'); return;}
  const id = selected, revision = snapshot.revision, listed = snapshot.files || [];
  $('file-revision').textContent = `WORKSPACE · r${revision}`;
  $('file-filter').value = ''; $('file-filter').hidden = listed.length < 12;
  fileSheet = {id, revision, files: listed, marks: new Map()}; drawFileList();
  if(!$('files-dialog').open) $('files-dialog').showModal();
  // Mark what this revision changed, and list what it deleted, against the one before.
  if(revision <= 1) return;
  try {
    const before = await filesAt(id, revision - 1);
    if(!(selected === id && snapshot?.revision === revision && $('files-dialog').open && fileSheet?.id === id && fileSheet.revision === revision)) return;
    const old = new Map(before.map(f => [f.path, f])), now = new Set(listed.map(f => f.path)), marks = new Map();
    for(const file of listed) {const was = old.get(file.path); if(!was || was.digest !== file.digest) marks.set(file.path, was ? 'changed' : 'new');}
    const gone = before.filter(f => !now.has(f.path)).map(f => ({path: f.path, deleted: true}));
    for(const file of gone) marks.set(file.path, 'deleted');
    fileSheet = {id, revision, files: [...listed, ...gone], marks}; drawFileList();
  } catch {}
}
function drawFileList() {
  const {id, revision, files, marks} = fileSheet, query = $('file-filter').value.trim().toLowerCase();
  if(!openFolders.has(id)) openFolders.set(id, new Set());
  const open = openFolders.get(id), tree = fileTree(query ? files.filter(f => f.path.toLowerCase().includes(query)) : files, f => marks.get(f.path) || '');
  const fileRow = (file, label) => {
    const button = el('button'); button.type = 'button'; button.dataset.path = file.path;
    const mark = marks.get(file.path);
    button.append(el('span','',label), el('small', mark ? 'changed' : '', file.deleted ? 'deleted' : mark ? `${mark} · ${size(file.size)}` : size(file.size)));
    button.onclick = () => openViewer(id, revision, file.path, file.deleted ? {from: 'files', diff: true} : {from: 'files'}).catch(fail);
    return button;
  };
  const folderRow = node => {
    const group = el('div','tree-folder'), head = el('button','tree-folder-head'), inside = el('div','tree-folder-files'), present = node.count - (node.marks.deleted || 0);
    head.type = 'button'; head.dataset.folder = node.path; head.title = node.path;
    const notes = Object.entries(node.marks).map(([kind, n]) => `${n} ${kind}`);
    head.append(el('span','tree-folder-name',node.name), el('small', notes.length ? 'changed' : '', [`${present} file${present === 1 ? '' : 's'}`, ...notes].join(' · ')));
    let drawn = false;
    // Open state by every folder the row stands for: a turn that joins or splits the chain keeps it.
    const show = on => {if(on && !drawn) {drawn = true; entries(inside, node);} inside.hidden = !on; head.setAttribute('aria-expanded', String(on)); for(const path of node.paths) if(on) open.add(path); else open.delete(path);};
    head.onclick = () => show(inside.hidden); show(open.has(node.path));
    group.append(head, inside); return group;
  };
  // A few hundred rows at a time: "Show more" draws the next ones, the focus moving to the first.
  const rows = (into, items, draw, from = 0) => {
    const first = into.children.length;
    for(const item of items.slice(from, from + ROWS_AT_ONCE)) into.append(draw(item));
    const left = items.length - from - ROWS_AT_ONCE;
    if(left > 0) {
      const more = el('button','tree-more',`Show ${Math.min(left, ROWS_AT_ONCE)} more of ${left}`); more.type = 'button';
      more.onclick = () => {more.remove(); rows(into, items, draw, from + ROWS_AT_ONCE)?.focus();}; into.append(more);
    }
    const drawn = into.children[first]; return drawn?.tagName?.toLowerCase() === 'button' ? drawn : drawn?.querySelector('button');
  };
  const entries = (into, node) => rows(into, [...node.files, ...node.folders], item => Array.isArray(item.folders) ? folderRow(item) : fileRow(item, item.path.split('/').pop()));
  $('file-list').replaceChildren();
  if(query) rows($('file-list'), tree.order, file => fileRow(file, file.path)); else entries($('file-list'), tree);
  if(!tree.count) $('file-list').append(el('p','note', query ? 'No file matches.' : 'No files at this revision.'));
}
$('file-filter').oninput = () => {if(fileSheet) drawFileList();};
// Update from host source: merges the host's current source into a self-edit thread.
function confirmUpdate() {
  confirmCommand('Update from host source?','Spindle merges the host\'s current source into this thread\'s workspace (a three-way merge against the source the thread last synced with) and starts a turn: the model is told what changed and resolves any conflicts. Nothing is built or installed; install afterwards. The turn\'s model calls may be billed.','update',{},{yes:'Update'});
}
// Shown while the last promotion stands refused because the host source moved (hosts
// without the inline change card).
function sourceMoved(paths) {
  const listed = paths.slice(0, 6).join(', ') + (paths.length > 6 ? ` and ${paths.length - 6} more` : '');
  const node = el('div', 'notice state warning', `Not promoted: since this thread last synced with the host source, both the host and this thread changed ${listed || 'the same files'}. Nothing was built or installed. Update from host source merges the current source into this thread and starts a turn that resolves the conflicts; promote again after it.`);
  const action = el('button', 'primary notice-action', 'Update from host source'); action.type = 'button'; action.onclick = confirmUpdate;
  node.append(action);
  return node;
}
// A thread that changed Spindle's own source: what changed, whether the host moved, a
// request from the model to install it, and Install / Update / Dismiss, at the end of
// the conversation. Promotion progress follows it as live notices.
// A checkout with nothing changed, requested, running or refused needs no card.
function pendingChange(change) {return change.changed_count > 0 || Boolean(change.request) || Boolean(change.installing) || (change.conflicts || []).length > 0;}
function changeCard(data, opened) {
  const change = data.self_edit, changed = Array.isArray(change.changed) ? change.changed : [], conflicts = Array.isArray(change.conflicts) ? change.conflicts : [];
  const count = change.changed_count ?? changed.length, prefix = change.prefix || '';
  const card = el('section', `tool-card change-card${change.request ? ' requested' : ''}${change.installing ? ' installing' : ''}`); card.setAttribute('aria-label', 'Spindle change');
  const head = el('div', 'change-head');
  head.append(el('span', 'tool-icon', '◆'), el('strong', 'change-title', 'Spindle change'), el('span', 'badge', change.installing ? 'installing' : change.request ? 'install requested' : conflicts.length ? 'conflicts' : change.behind ? 'host moved' : 'ready'));
  card.append(head);
  if(change.installing) card.append(el('p', 'change-line', 'Installing: checks and the build run offline; the host restarts when no thread is running. Progress follows below.'));
  else if(change.request) {
    const request = el('div', 'change-request');
    request.append(el('p', '', change.request.summary || 'The model asks to install this change.'), el('small', '', ['asked', typeof change.request.revision === 'number' ? `at r${change.request.revision}` : '', ago(change.request.at)].filter(Boolean).join(' ')));
    card.append(request);
  }
  const files = el('details', 'change-files'); files.dataset.key = 'self-edit-files'; files.open = opened.has(files.dataset.key);
  files.append(el('summary', '', `${count} changed file${count === 1 ? '' : 's'}${prefix ? ` in ${prefix}` : ''}`));
  const list = el('div', 'change-list');
  for(const path of changed.slice(0, 200)) {
    const full = !prefix || path.startsWith(prefix) ? path : `${prefix}${path}`;
    list.append(fileLink(full, full.slice(prefix.length) || full, {base: change.base_revision}));
  }
  if(count > Math.min(changed.length, 200)) list.append(el('small', 'note', `and ${count - Math.min(changed.length, 200)} more`));
  files.append(list); card.append(files);
  const live = !data.busy && !historical && data.complete && !change.installing;
  if(change.installing) {}
  else if(conflicts.length) card.append(el('p', 'change-line warning', `Conflicts with the host source: ${conflicts.slice(0, 4).join(', ')}${conflicts.length > 4 ? ` and ${conflicts.length - 4} more` : ''}. Update brings the host's changes in and starts a turn that resolves them.`));
  else if(change.behind) card.append(el('p', 'change-line', 'The host source moved since this thread last synced. Install merges both; Update brings the host\'s changes in here first.'));
  const actions = el('div', 'change-actions'); actions.hidden = Boolean(change.installing);
  const button = (label, cls, action) => {const b = el('button', cls, label); b.type = 'button'; b.disabled = !live; b.onclick = action; actions.append(b); return b;};
  // A gate stopped the last install of this head: what it said, and a way to hand it
  // to the model, which never sees the gate's output otherwise.
  const failure = !change.installing && change.failure && typeof change.failure === 'object' ? change.failure : null;
  if(failure) {
    const box = el('details','change-failure'); box.dataset.key = 'self-edit-failure'; box.open = opened.has(box.dataset.key);
    box.append(el('summary','',`Install stopped at: ${failure.command || 'a gate'}`), el('pre','display-pre', failure.output || 'The gate printed nothing.'));
    card.append(box);
  }
  if(change.request) button('Dismiss', '', () => command('dismiss_install').then(() => note('Install request dismissed. The change stays in this thread.')).catch(() => {}));
  if(change.behind || conflicts.length) button('Update', conflicts.length ? 'primary' : '', confirmUpdate);
  if(failure) button('Ask the thread to fix it', conflicts.length ? '' : 'primary', () => askFix(failure));
  const install = button('Install…', conflicts.length || failure ? '' : 'primary', confirmInstall);
  if(conflicts.length) install.disabled = true;
  card.append(actions);
  if(change.host_build) card.append(el('p', 'meta change-foot', `host build ${String(change.host_build).slice(0, 12)}`));
  return card;
}
// The gate's own words go to the model as the next message (its end, where the
// errors are), with what an install runs, so the fix is checked the same way.
function askFix(failure) {
  const output = String(failure.output || '').slice(-5000);
  const text = `Installing your Spindle change stopped at \`${failure.command || 'a gate'}\`. Its output ends:\n\n\`\`\`\n${output}\n\`\`\`\n\nFix every error and warning (any warning fails the install), run that command and the other install gates in spindle/ until they pass, then call spindle_install again.`;
  command('say', {text}).catch(() => {});
}
function confirmInstall() {
  const change = snapshot?.self_edit;
  if(!change) {toast('This thread has no Spindle change to install.'); return;}
  if(snapshot.busy || !snapshot.complete) {toast('Install after the current turn finishes (or Stop it).'); return;}
  if(change.conflicts?.length) {toast('Resolve the conflicts first: Update brings the host\'s changes in.'); return;}
  const count = change.changed_count ?? change.changed?.length ?? 0;
  confirmCommand('Install this Spindle change?', `${count} changed file${count === 1 ? '' : 's'}${change.prefix ? ` in ${change.prefix}` : ''} at r${snapshot.head}${change.behind ? ', merged with the host source that moved since this thread synced' : ''}. Installing:`, 'promote', {}, {yes: 'Install', steps: [
    'Merges this thread\'s changes into the host\'s current source.',
    'Checks formatting and lint, and runs the sandboxed unit tests (library and binary).',
    'Builds a candidate offline, verifies copies of threads and that it opens this host.',
    'Installs it and keeps the current binary for rollback (restored automatically if the new one cannot start).',
    'Restarts the host as soon as no thread is running; screens reconnect by themselves.',
  ]});
}
$('rollback').onclick = () => {
  // A host-level command: no thread is involved.
  confirmAction('Restore the previous binary?','The host must be idle. This swaps the current and previous binaries and restarts the host. It does not rewind thread data.',async () => {
    await command('rollback');
    $('settings-dialog').close(); note('Rolling back. This screen reconnects when the host is back.');
  },{yes:'Roll back'});
};
// While visible, the thread list refreshes now and then, so ended runs are noticed.
function poll() {if(document.hidden) return; loadSessions().then(() => {if(!selected) listSessions();}).catch(() => {});}
setInterval(poll, 20000);
document.addEventListener('visibilitychange',() => {if(document.hidden) {finishDelete(); source?.close(); source = null;} else {recover(); poll(); if(selected && !historical) {cursor = ''; connect();}}});
// Leaving the page deletes what waits for its Undo.
window.addEventListener('pagehide', finishDelete);
window.addEventListener('online',() => {recover(); cursor = ''; connect();});
window.addEventListener('offline',() => {$('connection').textContent = 'Offline';});
window.addEventListener('hashchange',async () => {
  const match = location.hash.match(/^#thread\/([a-zA-Z0-9_-]+)$/);
  try {if(match && match[1] !== selected) await openThread(match[1]); else if(!match && selected) await home();} catch(error) {fail(error);}
});
if(window.visualViewport) {
  // The tallest visual viewport seen at this width is the keyboard-closed height; a much shorter one means the keyboard is up.
  let full = {width: 0, height: 0};
  const fit = () => {
    const vv = window.visualViewport;
    if(vv.width !== full.width || vv.height > full.height) full = {width: vv.width, height: Math.max(vv.height, vv.width === full.width ? full.height : 0)};
    document.body.style.height = `${vv.height}px`;
    document.body.classList.toggle('keyboard', vv.height < full.height * 0.8);
    // What the keyboard covers of the layout viewport (iOS keeps that viewport; Chrome with
    // resizes-content shrinks it, leaving 0): bottom sheets rise above the keyboard.
    document.documentElement.style.setProperty('--keyboard', `${Math.max(0, Math.round(window.innerHeight - vv.height - vv.offsetTop))}px`);
    window.scrollTo(0,0);
  };
  window.visualViewport.addEventListener('resize',fit); fit();
}
// The skin link loaded before pairing was refused (401); fetch it again with the new cookie.
function reloadSkin() {const link = document.getElementById('skin'); if(!link) return; const url = new URL(link.href); url.searchParams.set('v', String(Date.now())); link.href = url.href;}
const skinMode = document.documentElement?.dataset.skin;
if(skinMode === 'preview') note('Previewing the staged skin on this tab only. Apply it from Settings to keep it.');
if(skinMode === 'safe') note('Safe mode: no host skin is loaded. Turn off or restore a skin from Settings.');
// A host restarted onto another build (a promotion, an upgrade) serves a newer page than
// this one: say so once. Needs the build in /api/ping; without it nothing is compared.
let loadedBuild = null, seenBoot = '';
async function checkBuild() {
  try {
    const {build} = await api('/api/ping');
    if(!build) return;
    if(loadedBuild === null) loadedBuild = build;
    else if(build !== loadedBuild) banner('update','Spindle was updated on the host. Reload to use the new version.',[['Reload',() => location.reload(),'primary']]);
  } catch {}
}
function restarted(boot) {if(seenBoot && boot !== seenBoot) checkBuild(); seenBoot = boot;}
// Session packs: each pack's latest view under the history, and the UI request a pack is
// waiting on (answered here, then the pack hears the answer).
function packCards(data) {
  for(const pack of data.packs || []) if(pack.view) packView($('history'), pack.pack, pack.view.title, pack.view.markdown);
  if(data.pack_ui && !historical) packRequest(data.pack_ui);
}
function packView(parent, pack, title, text) {
  const card = el('details','tool-card pack-card'); card.open = true;
  const summary = el('summary'); summary.append(el('span','tool-icon','▣'), el('span','tool-title',String(title)), el('span','badge',pack));
  const body = mdNodes(el('div','tool-body md'), markdown(String(text ?? '')));
  card.append(summary, body); parent.append(card);
  searchable.push({node:card, text:`${title}\n${text}`});
  return card;
}
function packRequest(event) {
  const params = event.params || {};
  // Progress updates replace the card with the same progress id; nothing is answered.
  if(event.method === 'progress') {
    const key = `progress:${event.pack}:${params.id || event.request_id}`;
    let card = [...$('live').children].find(c => c.dataset?.progress === key);
    if(!card) {card = el('div','notice pack-progress'); card.dataset.progress = key; $('live').append(card);}
    card.textContent = `${event.pack} · ${params.title || 'progress'}${params.total ? ` · ${params.current ?? 0}/${params.total}` : ''}${params.message ? ` · ${params.message}` : ''}`;
    scroll(); return;
  }
  if([...$('live').children].some(c => c.dataset?.packUi === event.id)) return;
  const card = el('div','tool-card pack-card pack-request'); card.dataset.packUi = event.id;
  card.append(el('div','tool-result-label',`${event.pack} ASKS`));
  if(params.title) card.append(el('strong','',String(params.title)));
  if(params.message || params.description) card.append(el('div','prose',String(params.message || params.description)));
  const actions = el('div','tool-footer'), id = selected;
  const send = async (answer, button) => {
    if(button) button.disabled = true;
    try {await api(`/api/sessions/${id}/pack-ui`, {id: event.id, ...answer}); closePackCard(card, 'Answered');}
    catch(error) {if(button) button.disabled = false; fail(error);}
  };
  const button = (label, answer, cls = '') => {const b = el('button', cls, label); b.type = 'button'; b.onclick = () => send(typeof answer === 'function' ? answer() : answer, b); actions.append(b); return b;};
  const options = list => (Array.isArray(list) ? list : []).map(o => typeof o === 'object' && o !== null ? {value: o.value, label: String(o.label ?? o.value)} : {value: o, label: String(o)});
  const control = field => {
    const kind = field.type || 'text';
    if(kind === 'textarea') {const t = el('textarea'); t.value = field.default ?? ''; return [t, () => t.value];}
    if(kind === 'boolean') {const c = el('input'); c.type = 'checkbox'; c.checked = Boolean(field.default); return [c, () => c.checked];}
    if(kind === 'select' || kind === 'multiselect') {
      const list = options(field.options), select = el('select'); select.multiple = kind === 'multiselect';
      for(const [index, option] of list.entries()) {const o = el('option','',option.label); o.value = String(index); select.append(o);}
      return [select, () => kind === 'select' ? list[Number(select.value)]?.value : [...select.querySelectorAll('option')].filter(o => o.selected).map(o => list[Number(o.value)]?.value)];
    }
    const input = el('input'); input.type = kind === 'number' ? 'number' : 'text'; input.value = field.default ?? ''; if(field.placeholder) input.placeholder = field.placeholder;
    return [input, () => kind === 'number' ? (input.value === '' ? null : Number(input.value)) : input.value];
  };
  if(event.method === 'confirm') {
    button(params.confirm_label || 'Yes', {status:'ok', value:true}, 'primary'); button(params.cancel_label || 'No', {status:'ok', value:false});
  } else if(event.method === 'select') {
    for(const option of options(params.options)) button(option.label, {status:'ok', value: option.value});
  } else if(event.method === 'input') {
    const [node, read] = control({type: params.multiline ? 'textarea' : 'text', default: params.default, placeholder: params.placeholder});
    card.append(node); button(params.submit_label || 'Send', () => ({status:'ok', value: read()}), 'primary');
  } else if(event.method === 'form') {
    const reads = [];
    for(const field of Array.isArray(params.fields) ? params.fields : []) {
      const label = el('label','field'); label.append(el('span','', `${field.label || field.id}${field.required ? ' *' : ''}`));
      const [node, read] = control(field); label.append(node); card.append(label); reads.push([field.id, read]);
    }
    button(params.submit_label || 'Submit', () => ({status:'ok', value: Object.fromEntries(reads.map(([key, read]) => [key, read()]).filter(([, v]) => v !== null && v !== undefined))}), 'primary');
  }
  button('Dismiss', {status:'cancelled'});
  card.append(actions); $('live').append(card); scroll();
  if(document.hidden || !following) toast(`${event.pack} is waiting for your answer.`, 'info');
}
// A request answered (here or elsewhere), withdrawn or timed out: its fields stay readable,
// nothing on the card answers any more.
function closePackCard(card, label) {
  card.dataset.packUi = ''; card.classList.add('answered');
  for(const node of card.querySelectorAll('input,select,textarea,button')) node.disabled = true;
  const footer = [...card.children].find(node => node.className === 'tool-footer');
  if(footer) footer.replaceChildren(el('span','meta',label));
}
async function init() {const hash = location.hash; await home(false); checkBuild(); previewsContained(); const match = hash.match(/^#thread\/([a-zA-Z0-9_-]+)$/); if(match && sessions.some(s => s.id === match[1])) await openThread(match[1]); await recover(); if(notifiable()) registerWorker();}
init().catch(error => {if(!$('pair-dialog').open) fail(error);});
