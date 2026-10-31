import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {createContext, runInContext} from 'node:vm';
import test from 'node:test';
import {intent} from '../ui/commands.js';
import * as text from '../ui/text.js';

const html = await readFile(new URL('../ui/index.html', import.meta.url), 'utf8');
const source = (await readFile(new URL('../ui/app.js', import.meta.url), 'utf8')).replace(/^import .*;\n/gm, '');
class Node {
  constructor(tag = 'div') {
    this.tagName = tag; this.children = []; this.dataset = {}; this.style = {}; this.value = ''; this.textContent = ''; this.hidden = false; this.open = false;
    this.scrollHeight = 45; this.scrollTop = 0; this.clientHeight = 45;
    const classes = new Set();
    this.classList = {add: (...xs) => xs.forEach(x => classes.add(x)), remove: (...xs) => xs.forEach(x => classes.delete(x)), contains: x => classes.has(x), toggle: (x, on) => {on ??= !classes.has(x); on ? classes.add(x) : classes.delete(x);}};
  }
  append(...nodes) {this.children.push(...nodes);}
  replaceChildren(...nodes) {this.children = [...nodes];}
  setAttribute(k, v) {this[k] = v;}
  removeAttribute(k) {delete this[k];}
  remove() {this.removed = true;}
  focus() {}
  click() {if(!this.disabled) return this.onclick?.({preventDefault() {}});}
  showModal() {if(this.open) throw Error('dialog already open'); this.open = true;}
  close() {this.open = false; this.onclose?.();}
  querySelectorAll(selector) {return this.children.flatMap(node => node instanceof Node ? [...(selector.split(',').includes(node.tagName) || selector === 'button' && node.tagName === 'button' ? [node] : []), ...node.querySelectorAll(selector)] : []);}
  querySelector(selector) {
    const descendants = this.children.filter(n => n instanceof Node).flatMap(n => [n, ...n.descendants()]);
    if(selector === '.badge') return descendants.find(n => n.className === 'badge') || null;
    const call = selector.match(/^\[data-call="([^"]+)"\]\[data-recorded="true"\]$/);
    if(call) return descendants.find(n => n.dataset.call === call[1] && n.dataset.recorded === 'true') || null;
    return null;
  }
  descendants() {return this.children.filter(n => n instanceof Node).flatMap(n => [n,...n.descendants()]);}
}
const textOf = (node) => [node.textContent || '', ...(node.children || []).map(n => n instanceof Node ? textOf(n) : n?.textContent || '')].join(' ');
// The Files sheet: folders start shut; these open every one, then find a row by its path.
const openFolders = list => {for(let shut; (shut = list.descendants().filter(n => n.className === 'tree-folder-head' && n['aria-expanded'] === 'false')).length;) for(const head of shut) head.onclick();};
const fileRow = (a, path) => {openFolders(a.nodes.get('file-list')); return a.nodes.get('file-list').descendants().find(n => n.dataset.path === path);};
const state = (more = {}) => ({head: 10, revision: 10, busy: false, complete: true, phase: 'Stopped(Complete)', stop: null, messages: [], files: [], steps: [{revision: 1}, {revision: 10}], ...more});
const response = (value, status = 200) => ({ok: status >= 200 && status < 300, status, text: async () => JSON.stringify(value)});
async function app(custom = () => undefined, hash = '', globals = {}) {
  const nodes = new Map([...html.matchAll(/id="([^"]+)"/g)].map(m => [m[1], new Node()]));
  for(const id of ['theme-choice']) for(const mode of ['paper','light','dark','system']) {const button = new Node('button'); button.dataset.mode = mode; nodes.get(id).append(button);}
  for(const mode of ['model','digest']) {const button = new Node('button'); button.dataset.mode = mode; nodes.get('compaction-choice').append(button);}
  const storage = new Map(), calls = [], streams = [], events = {}, timers = [];
  const context = createContext({
    ...globals, console, intent, ...text, Terminal: class {write() {}}, TextEncoder, TextDecoder, Uint8Array, URL, CSS: {escape: x => x}, crypto: {randomUUID: (() => {let n = 0; return () => `req-${++n}`;})()},
    // Enough of a parser for a page's script sources.
    DOMParser: class {parseFromString(html) {const sources = [...String(html).matchAll(/<script[^>]*\ssrc="([^"]+)"/g)].map(found => found[1]); return {querySelectorAll: () => sources.map(src => ({getAttribute: () => src}))};}},
    atob: s => Buffer.from(s, 'base64').toString('binary'),
    document: {body: new Node('body'), getElementById: id => nodes.get(id), createElement: tag => new Node(tag), createElementNS: (ns, tag) => new Node(tag), createTextNode: text => ({textContent: text}), createDocumentFragment: () => new Node(), querySelectorAll: () => [], addEventListener: (name, fn) => events[name] = fn, hidden: false},
    location: {hash, reload() {}}, navigator: {clipboard: {writeText: async () => {}}},
    localStorage: {getItem: k => storage.get(k), setItem: (k,v) => storage.set(k,v)},
    requestAnimationFrame: () => 1, clearTimeout() {}, setTimeout: fn => {timers.push(fn); return timers.length;}, setInterval: () => 1,
    EventSource: class {constructor(url) {this.url = url; this.listeners = {}; streams.push(this);} close() {this.closed = true;} addEventListener(name, fn) {this.listeners[name] = fn;}},
    fetch: async (path, options = {}) => {
      const raw = options.headers?.['Content-Type'] === 'application/octet-stream';
      const data = options.body ? raw ? {upload: options.body} : JSON.parse(options.body) : undefined; calls.push({path, data});
      const override = await custom(path, data); if(override !== undefined) return override;
      if(path === '/api/sessions') return response({sessions: [{id:'a',name:'Alpha'},{id:'b',name:'Beta'}],providers: [{id:'glm',model:'demo'}]});
      if(path.endsWith('/snapshot')) return response(state());
      if(path.includes('/snapshot?')) return response(state({revision: Number(path.split('=').at(-1))}));
      if(path === '/api/commands') return response({id:data.request_id,session:data.session,state:'completed'});
      if(path === '/api/host') return response({code_policy:{models:[],tools:[]}});
      if(path === '/api/devices') return response({devices:[]});
      if(path === '/api/ping') return response({boot:'boot'});
      if(path === '/api/logins') return response({logins:[{provider:'glm',name:'GLM',status:'missing',signed_in:false}]});
      if(path.endsWith('/steer')) return response({queued:true},202);
      if(path.endsWith('/upload')) return response({digest:`${data.upload.name.length}`.padStart(64,'0'),size:data.upload.size});
      if(path === '/api/skin') return response(data?.action === 'apply' ? {active:{digest:data.digest,bytes:20},staged:null,versions:[{id:'00000000000000000001-abcdef0123456789',digest:'abcdef0123456789',at:1}]} : {active:null,staged:{digest:'abcdef0123456789',bytes:20,css:'body { --accent: red }'},versions:[]});
      throw Error(`unexpected request ${path}`);
    },
  });
  context.window = {addEventListener: (name, fn) => events[name] = fn, removeEventListener: (name, fn) => {if(events[name] === fn) delete events[name];}};
  runInContext(source + '\nglobalThis.subject = {openThread, home, command, recoverThread, runIntent, render, backToLive, connect, recover, loadSessions, openPicker, openViewer, finishDelete, get snapshot(){return snapshot;}, get selected(){return selected;}, get historical(){return historical;}, get catalog(){return catalog;}};', context);
  const flush = async () => {for(let i = 0; i < 25; i++) await Promise.resolve();};
  await flush();
  return {subject: context.subject, nodes, storage, calls, streams, events, timers, context, flush, confirm: async () => {await nodes.get('confirm-form').onsubmit({preventDefault() {}}); await flush();}};
}

test('application boots, pairs on 401 and binds actual controls', async () => {
  const a = await app(path => path === '/api/sessions' ? response({error:'Pair first'},401) : undefined);
  assert(a.nodes.get('pair-dialog').open);
  assert.equal(typeof a.nodes.get('model-chip').onclick, 'function');
  assert.equal(typeof a.nodes.get('title-button').onclick, 'function');
  assert.equal(typeof a.nodes.get('composer').onsubmit, 'function');
});

test('typed retry and rewind use the same confirmation as buttons', async () => {
  for(const text of ['retry', 'rewind to r1']) {
    const a = await app(); await a.subject.openThread('a'); await a.subject.runIntent(text);
    assert(a.nodes.get('confirm-dialog').open, text);
    assert.equal(a.calls.filter(c => c.data).length, 0);
    await a.confirm(); assert.equal(a.calls.filter(c => c.data).length, 1);
  }
});

test('composer allows stop while busy, and an ordinary message steers the running turn', async () => {
  let ended = false;
  const a = await app(path => ended && path.endsWith('/steer') ? response({error:'no turn is running; send it as a message'},409) : undefined);
  await a.subject.openThread('a'); a.subject.render(state({busy:true,complete:false}));
  assert.equal(a.nodes.get('send').disabled, false);
  a.nodes.get('prompt').value = 'stop'; await a.nodes.get('composer').onsubmit({preventDefault() {}});
  assert.equal(a.calls.filter(c => c.data).at(-1).data.kind, 'stop');
  a.nodes.get('prompt').value = 'also check the tests'; a.storage.set('draft:a','also check the tests'); a.subject.render(state({busy:true,complete:false}));
  await a.nodes.get('composer').onsubmit({preventDefault() {}});
  const sent = a.calls.filter(c => c.data).at(-1);
  assert.equal(sent.path,'/api/sessions/a/steer'); assert.deepEqual(sent.data,{text:'also check the tests'});
  assert.equal(a.calls.filter(c => c.path === '/api/commands').length,1,'steering is not a command');
  assert.equal(a.nodes.get('prompt').value,''); assert.equal(a.storage.get('draft:a'),'');
  const waiting = a.nodes.get('queued');
  assert.equal(waiting.hidden,false,'the message shows at once, waiting');
  assert.equal(waiting.children[0].children[0].textContent,'YOU · QUEUED');
  assert.equal(waiting.children[0].children[1].textContent,'also check the tests');
  ended = true; a.nodes.get('prompt').value = 'too late';
  await a.nodes.get('composer').onsubmit({preventDefault() {}});
  assert.equal(a.nodes.get('prompt').value,'too late','a refused steer keeps the draft');
  assert.match(a.nodes.get('toast').textContent,/no turn is running/);
});

test('delayed command responses cannot clear a different thread draft or change its state', async () => {
  let finish;
  const a = await app((path,data) => path === '/api/commands' ? new Promise(resolve => finish = () => resolve(response({id:data.request_id,session:data.session,state:'running'}))) : undefined);
  await a.subject.openThread('a'); a.nodes.get('prompt').value = 'message A';
  const pending = a.subject.command('say',{text:'message A'}); await a.flush();
  a.storage.set('draft:b','message B'); await a.subject.openThread('b');
  finish(); await pending;
  assert.equal(a.nodes.get('prompt').value,'message B'); assert.equal(a.storage.get('draft:b'),'message B'); assert.equal(a.subject.snapshot.busy,false);
});

test('unknown administrative admission retains its exact request id', async () => {
  const a = await app(path => {if(path === '/api/commands') throw Error('connection lost');});
  await a.subject.openThread('a'); await assert.rejects(a.subject.command('settings',{network:true}));
  const pending = JSON.parse(a.storage.get('pending-command:a')); assert.equal(pending.kind,'settings'); assert.equal(pending.session,'a');
});

test('recovery waits for rewind completion before continuing', async () => {
  let polls = 0;
  const a = await app((path,data) => {
    if(path === '/api/commands' && data.kind === 'rewind') return response({id:data.request_id,session:'a',state:'running'});
    if(path.startsWith('/api/receipts/')) {polls++; return response({id:path.split('/').at(-1),session:'a',state:'completed'});}
    if(path.endsWith('/snapshot')) return response(state({head:11,revision:11,complete:false}));
  });
  await a.subject.openThread('a'); a.subject.render(state({stop:'stopped: too many incomplete responses',complete:false,recover:1}));
  await a.subject.recoverThread();
  assert.equal(polls,1); const commands = a.calls.filter(c => c.data); assert.deepEqual(commands.map(c => c.data.kind),['rewind','continue']); assert.equal(commands[1].data.expected_revision,11);
});

test('return to live closes the history sheet and re-enables ordinary messages', async () => {
  const a = await app(); await a.subject.openThread('a'); a.nodes.get('hist-chip').click(); assert(a.nodes.get('history-dialog').open);
  await a.subject.backToLive(); assert.equal(a.nodes.get('history-dialog').open,false);
});

test('browser hash navigation opens the requested thread', async () => {
  const a = await app(); await a.subject.openThread('a'); a.context.location.hash = '#thread/b';
  assert.equal(typeof a.events.hashchange,'function'); await a.events.hashchange(); await a.flush(); assert.equal(a.subject.selected,'b');
});

test('a deep link survives startup without a spurious home navigation', async () => {
  const a = await app(undefined,'#thread/b');
  assert.equal(a.subject.selected,'b'); assert.equal(a.context.location.hash,'#thread/b');
});

test('stale stream callbacks cannot overwrite a new stream or connection state', async () => {
  const a = await app(); await a.subject.openThread('a'); const old = a.streams.at(-1);
  a.subject.connect(); const live = a.streams.at(-1); live.onopen(); old.onerror();
  old.listeners.snapshot({lastEventId:'boot:9',data:JSON.stringify(state({head:999}))});
  assert.equal(a.nodes.get('connection').textContent,'Connected'); assert.equal(a.subject.snapshot.head,10);
});

test('mid-run replay shows pending tool output and flushes thinking at the catch-up boundary', async () => {
  const a = await app(); await a.subject.openThread('a'); const stream = a.streams.at(-1);
  const emit = (type,seq,event) => stream.listeners[type]({lastEventId:`boot:${seq}`,data:JSON.stringify(event)});
  emit('snapshot',3,state({busy:true,complete:false,replay:true}));
  emit('preview',1,{type:'model',model:'demo',step:0}); emit('preview',2,{type:'delta',text:'partial reply'}); emit('preview',3,{type:'nested',label:'waiting'});
  assert(a.nodes.get('live').descendants().some(n => n.children.some(c => c.textContent === 'partial reply')));
  const call = {id:'call-1',name:'bash',input:{command:'echo ok'}};
  emit('snapshot',8,state({busy:true,complete:false,replay:true,messages:[{role:'assistant',content:[{type:'tool_use',...call}]}],pending:[call]}));
  emit('preview',5,{type:'tool',...call}); emit('preview',6,{type:'output',stream:1,data:'b2s='});
  assert.equal(a.nodes.get('history').children.length,0,'unrecorded calls are not rendered twice');
  assert(a.nodes.get('live').descendants().some(n => n.className === 'terminal'));
});

test('Stop is admissible while the normal submission response is outstanding', async () => {
  let finish;
  const a = await app((path,data) => path === '/api/commands' && data.kind === 'say' ? new Promise(resolve => finish = () => resolve(response({id:data.request_id,session:'a',state:'running'}))) : undefined);
  await a.subject.openThread('a'); const message = a.subject.command('say',{text:'task'}); await a.flush();
  await a.subject.command('stop'); finish(); await message;
  assert.deepEqual(a.calls.filter(c => c.data).map(c => c.data.kind),['say','stop']);
});

test('uncertain Stop is reconciled by receipt without a new dispatch', async () => {
  const a = await app(path => path.startsWith('/api/receipts/') ? response({state:'completed',session:'a'}) : undefined);
  a.storage.set('pending-stop',JSON.stringify({request_id:'saved-stop',kind:'stop',session:'a'}));
  await a.subject.recover(); assert.equal(a.storage.get('pending-stop'),''); assert.equal(a.calls.filter(c => c.data).length,0);
});

test('confirmation refuses to act on a different thread or changed head', async () => {
  const a = await app(); await a.subject.openThread('a'); await a.subject.runIntent('retry'); await a.subject.openThread('b'); await a.confirm();
  assert.equal(a.calls.filter(c => c.data).length,0);
});

test('failed rewind receipt never triggers Continue', async () => {
  const a = await app((path,data) => {
    if(path === '/api/commands') return response({id:data.request_id,session:'a',state:'running'});
    if(path.startsWith('/api/receipts/')) return response({state:'failed',error:'bad target'});
  });
  await a.subject.openThread('a'); a.subject.render(state({complete:false,recover:1,stop:'halted'}));
  await assert.rejects(a.subject.recoverThread(),/bad target/);
  assert.deepEqual(a.calls.filter(c => c.data).map(c => c.data.kind),['rewind']);
});

test('a 401 is a known refusal, not an unknown command admission', async () => {
  const a = await app(path => path === '/api/commands' ? response({error:'Pair first'},401) : undefined);
  await a.subject.openThread('a'); await assert.rejects(a.subject.command('say',{text:'hello'}));
  assert.equal(a.storage.get('pending-command:a'),''); assert(a.nodes.get('pair-dialog').open);
});

test('checking an uncertain acknowledged message never sends a fresh duplicate', async () => {
  const a = await app(path => path.startsWith('/api/receipts/') ? response({state:'completed',session:'a'}) : undefined);
  await a.subject.openThread('a'); a.nodes.get('prompt').value = 'hello'; a.storage.set('draft:a','hello');
  a.storage.set('pending-command',JSON.stringify({request_id:'old-message',kind:'say',session:'a',text:'hello'}));
  await assert.rejects(a.subject.command('say',{text:'hello'}),/previous submission/);
  assert.equal(a.calls.filter(c => c.data).length,0); assert.equal(a.storage.get('pending-command'),'');
  assert.equal(a.storage.get('draft:a'),''); assert.equal(a.nodes.get('prompt').value,'');
});

test('malformed tool arguments remain inspectable instead of crashing the thread renderer', async () => {
  const a = await app(); await a.subject.openThread('a');
  for(const [name,input] of [['bash',{command:42}],['read_file',{path:{invalid:true}}],['read_output',{artifact:123}]]) {
    assert.doesNotThrow(() => a.subject.render(state({messages:[{role:'assistant',content:[{type:'tool_use',id:'bad-call',name,input}]}]})));
    assert.equal(a.nodes.get('history').children.length,1);
  }
});

test('newer drafts and historical inspection do not get silently discarded', async () => {
  const a = await app(); await a.subject.openThread('a');
  a.nodes.get('prompt').value='newer draft'; a.storage.set('draft:a','newer draft');
  await a.subject.command('say',{text:'older submission'});
  assert.equal(a.nodes.get('prompt').value,'newer draft'); assert.equal(a.storage.get('draft:a'),'newer draft');
  a.nodes.get('hist-prev').click(); await a.flush(); assert.equal(a.subject.historical,true);
  await a.subject.command('settings',{network:true}); assert.equal(a.subject.historical,true);
});

test('host skin is staged, reviewed through the shared confirmation, then applied by digest', async () => {
  const a = await app(); await a.subject.openThread('a');
  await a.nodes.get('settings').onclick(); await a.flush();
  assert.match(a.nodes.get('skin-status').textContent, /staged abcdef0123456789/);
  assert.equal(a.nodes.get('skin-css').value, 'body { --accent: red }');
  assert.equal(a.nodes.get('skin-apply').disabled, false);
  a.nodes.get('settings-dialog').close();
  a.nodes.get('skin-apply').onclick();
  assert(a.nodes.get('confirm-dialog').open, 'apply needs confirmation');
  assert.equal(a.calls.filter(c => c.path === '/api/skin' && c.data).length, 0);
  await a.confirm();
  const posts = a.calls.filter(c => c.path === '/api/skin' && c.data);
  assert.deepEqual(posts.map(c => c.data), [{action:'apply',digest:'abcdef0123456789'}]);
  assert.match(a.nodes.get('skin-status').textContent, /Applied abcdef0123456789/);
  assert.equal(a.nodes.get('skin-versions').children.length, 1);
  a.nodes.get('skin-thread').onclick(); await a.flush();
  assert.deepEqual(a.calls.filter(c => c.path === '/api/skin' && c.data).at(-1).data, {action:'stage',session:'a',revision:10});
});

test('typed reasoning command saves effort; the chip shows model and what is sent, the header what it cost', async () => {
  const a = await app(path => path.endsWith('/snapshot') ? response(state({provider:'glm',model:'glm-5.3',effort:'default',reasoning:{label:'extended thinking, 16384-token budget (high)',short:'think 16K'},usage:{calls:2,input:10,output:5,usd:0.0123,priced:2}})) : undefined);
  await a.subject.openThread('a');
  // The model under the title; what it spent, a button at the right (to the thread menu).
  assert.equal(a.nodes.get('subtitle').textContent, 'glm · glm-5.3');
  assert.match(a.nodes.get('subtitle').title, /16384-token budget/);
  assert.equal(a.nodes.get('spend').textContent, '$0.012'); assert.equal(a.nodes.get('spend').hidden, false);
  assert.equal(a.nodes.get('spend').onclick, a.nodes.get('title-button').onclick, 'the spend opens the thread menu, where the limit is');
  assert.equal(a.nodes.get('chip-model').textContent, 'glm-5.3');
  assert.equal(a.nodes.get('chip-effort').textContent, 'think 16K', 'auto effort shows what the route sends');
  assert.equal(a.nodes.get('chip-provider').textContent, '', 'the model name already says glm');
  await a.subject.runIntent('reasoning high'); await a.flush();
  const sent = a.calls.filter(c => c.path === '/api/commands' && c.data).at(-1).data;
  assert.equal(sent.kind, 'settings'); assert.equal(sent.effort, 'high');
  assert.equal(Object.keys(sent).filter(k => ['provider','model','network','toolchain','compaction','max_usd'].includes(k)).length, 0, 'only the effort is sent');
  assert.match(a.nodes.get('toast').textContent, /Next turn uses: high/);
});

test('a definitive refusal of a retried submission forgets it, and only that thread was blocked', async () => {
  let mode = 'drop';
  const a = await app(path => {
    if(path === '/api/commands' && mode === 'drop') throw Error('connection lost');
    if(path === '/api/commands' && mode === 'refuse') return response({error:'session changed; refresh before submitting'}, 409);
    if(path.startsWith('/api/receipts/')) return response({error:'request ID not admitted'}, 404);
  });
  await a.subject.openThread('a');
  await assert.rejects(a.subject.command('continue'));
  assert(a.storage.get('pending-command:a'), 'an unknown admission is kept');
  mode = 'ok';
  await a.subject.openThread('b');
  await a.subject.command('continue');
  assert.equal(a.calls.filter(c => c.data).at(-1).data.session, 'b', 'thread b is not blocked by thread a');
  await a.subject.openThread('a'); mode = 'refuse';
  await assert.rejects(a.subject.command('continue'), /previous submission/);
  assert(a.nodes.get('confirm-dialog').open, 'the exact retry is offered');
  assert.equal(a.nodes.get('banner').hidden, false);
  await a.confirm();
  assert.equal(a.storage.get('pending-command:a'), '', 'a 409 on the retry admits nothing');
  assert.equal(a.nodes.get('banner').hidden, true);
  mode = 'ok'; await a.subject.command('continue');
  assert.equal(a.calls.filter(c => c.data).at(-1).data.kind, 'continue');
});

test('an unresolved submission can be discarded from the banner', async () => {
  const a = await app(path => {if(path.startsWith('/api/receipts/')) throw Error('offline');});
  a.storage.set('pending-command', JSON.stringify({request_id:'old',kind:'say',session:'a',text:'x'}));
  await a.subject.recover();
  assert.equal(a.storage.get('pending-command'), '', 'the unscoped key moved to its thread');
  assert(a.storage.get('pending-command:a'));
  assert.match(a.nodes.get('banner-text').textContent, /Unresolved say/);
  const discard = a.nodes.get('banner-actions').children.find(b => b.textContent === 'Discard');
  discard.onclick(); assert(a.nodes.get('confirm-dialog').open); await a.confirm();
  assert.equal(a.storage.get('pending-command:a'), '');
  assert.equal(a.nodes.get('banner').hidden, true);
  assert.equal(a.calls.filter(c => c.data).length, 0, 'discarding sends nothing');
});

test('a refused new thread keeps the first message in the home composer with the real reason', async () => {
  const a = await app(path => path === '/api/commands' ? response({error:'reasoning off is not available on this route'}, 400) : undefined);
  assert.equal(a.nodes.get('dock').hidden, false, 'the thread list has a composer');
  assert.equal(a.nodes.get('attach').hidden, true, 'attachments go with a thread message');
  assert.equal(a.nodes.get('prompt').placeholder, 'Start a new thread…');
  a.nodes.get('prompt').value = 'build it'; a.nodes.get('prompt').oninput();
  await a.nodes.get('composer').onsubmit({preventDefault() {}}); await a.flush();
  const sent = a.calls.filter(c => c.path === '/api/commands').at(-1).data;
  assert.equal(sent.kind, 'new'); assert.equal(sent.text, 'build it'); assert.equal(sent.provider, 'glm'); assert.equal(sent.model, 'demo');
  assert.equal('dev' in sent, false, 'self-edit is not a checkbox any more');
  assert.match(a.nodes.get('toast').textContent, /reasoning off is not available on this route\. Your message is kept\./);
  assert.equal(a.nodes.get('prompt').value, 'build it');
  assert.equal(a.storage.get('draft:home'), 'build it');
  assert.equal(a.storage.get('pending-command:new'), '');
  assert.equal(JSON.parse(a.storage.get('draft:new')).text, 'build it');
  assert.equal(a.subject.selected, null);
});

test('a deleted thread ends its stream and says so instead of reconnecting', async () => {
  let deleted = false;
  const a = await app(path => deleted && path.startsWith('/api/sessions/a/') ? response({error:'thread not found'}, 404) : undefined);
  await a.subject.openThread('a'); const stream = a.streams.at(-1);
  deleted = true; stream.readyState = 2; stream.onerror(); await a.flush();
  assert.equal(a.subject.selected, null);
  assert.match(a.nodes.get('toast').textContent, /no longer exists/);
  assert.equal(a.streams.length, 1, 'no new EventSource');
});

test('an expired Cloudflare sign-in offers Sign in again instead of Reconnecting', async () => {
  const a = await app(path => path === '/api/sessions' ? {ok:false, status:0, type:'opaqueredirect', text: async () => ''} : undefined);
  assert.equal(a.nodes.get('banner').hidden, false);
  assert.match(a.nodes.get('banner-text').textContent, /sign-in expired/);
  assert.equal(a.nodes.get('banner-actions').children[0].textContent, 'Sign in again');
  assert.equal(a.nodes.get('pair-dialog').open, false);
});

test('the thread list shows status and reports runs that ended since it last looked', async () => {
  const sessions = [{id:'a',name:'Alpha',status:{state:'needs attention',at:Date.now() - 120000}},{id:'b',name:'Beta',status:{state:'done',at:Date.now()}}];
  const a = await app(path => path === '/api/sessions' ? response({sessions,providers:[]}) : undefined);
  // By what they need: a heading, then its rows (a shape for the state, what it is doing,
  // how long ago, and ⋯ for its actions).
  const list = a.nodes.get('sessions').children;
  assert.deepEqual(list.map(n => n.tagName === 'h2' ? `# ${n.textContent}` : n.dataset.state), ['# Needs you', 'needs attention', '# Today', 'done']);
  const rows = list.filter(n => n.tagName !== 'h2');
  assert.match(textOf(rows[0]), /Alpha 2m .*Needs you: open it to see why/);
  assert.equal(rows[0].className, 'session-row last needs');
  assert.equal(rows[0].children[1]['aria-label'], 'More actions for Alpha');
  a.storage.set('thread-status', JSON.stringify({b:'running'}));
  await a.subject.home(); await a.flush();
  assert.match(a.nodes.get('toast').textContent, /Finished: Beta: done/);
});

test('search marks matching blocks and steps through them', async () => {
  const a = await app(); await a.subject.openThread('a');
  a.subject.render(state({messages:[{role:'user',content:[{type:'text',text:'say hello'}]},{role:'assistant',content:[{type:'text',text:'Hello back'},{type:'tool_use',id:'t',name:'bash',input:{command:'echo nothing'}}]}]}));
  await a.subject.runIntent('/find hello');
  assert.equal(a.nodes.get('findbar').hidden, false);
  assert.equal(a.nodes.get('find-count').textContent, '1/2');
  a.nodes.get('find-next').onclick(); assert.equal(a.nodes.get('find-count').textContent, '2/2');
  a.nodes.get('find-next').onclick(); assert.equal(a.nodes.get('find-count').textContent, '1/2');
  await a.subject.runIntent('/find nothing');
  assert.equal(a.nodes.get('find-count').textContent, '1/1', 'tool cards are searchable');
  a.nodes.get('find-close').onclick(); assert.equal(a.nodes.get('findbar').hidden, true);
});

test('files mark what a revision changed and show a unified diff against the one before', async () => {
  const files = [{path:'a.txt',digest:'new',size:4},{path:'same.txt',digest:'s',size:1}];
  const a = await app(path => {
    if(path.endsWith('/snapshot')) return response(state({files}));
    if(path.startsWith('/api/sessions/a/files?revision=9')) return response({revision:9,files:[{path:'a.txt',digest:'old',size:4},{path:'same.txt',digest:'s',size:1},{path:'gone.txt',digest:'g',size:1}]});
    if(path.startsWith('/api/sessions/a/file?revision=9&path=a.txt')) return {ok:true,status:200,text: async () => 'one\ntwo\n'};
    if(path.startsWith('/api/sessions/a/file?revision=10&path=a.txt')) return {ok:true,status:200,text: async () => 'one\n2\n'};
    if(path.startsWith('/api/sessions/a/file?revision=9&path=gone.txt')) return {ok:true,status:200,text: async () => 'bye\n'};
    if(path.startsWith('/api/sessions/a/file?revision=10&path=gone.txt')) return {ok:false,status:404,text: async () => 'missing'};
  });
  await a.subject.openThread('a'); await a.nodes.get('files').click(); await a.flush();
  const list = a.nodes.get('file-list').children.map(textOf);
  assert.match(list[0], /a\.txt.*changed/); assert.match(list[1], /gone\.txt.*deleted/, 'a deleted file stays where it was'); assert.doesNotMatch(list[2], /changed/);
  a.nodes.get('file-list').children[0].onclick(); await a.flush();
  a.nodes.get('file-view-diff').onclick(); await a.flush();
  const rows = a.nodes.get('file-content').children;
  assert.deepEqual(rows.map(r => [r.className || '', r.textContent]), [['diff-hunk','@@ -1,2 +1,2 @@\n'],['',' one\n'],['diff-del','-two\n'],['diff-add','+2\n']]);
  assert.match(a.nodes.get('file-view-meta').textContent, /a\.txt · r9 → r10/);
  // A deleted file opens straight on what it lost; nothing complains it is missing.
  a.nodes.get('viewer-back').onclick(); a.nodes.get('toast').textContent = '';
  await a.nodes.get('file-list').children[1].onclick(); await a.flush();
  assert.deepEqual(a.nodes.get('file-content').children.map(r => r.textContent), ['Deleted at r10.\n','@@ -1,1 +0,0 @@\n','-bye\n']);
  assert.equal(a.nodes.get('toast').textContent, '');
});

test('folders start shut and say what they hold and what changed; they stay as the user left them', async () => {
  const files = [{path:'src/a.rs',digest:'a2',size:4},{path:'src/lib/b.rs',digest:'b',size:4},{path:'docs/x.md',digest:'x',size:4},
    ...Array.from({length: 3}, (_, i) => ({path:`.venv/lib/python3.14/site-packages/pip-26.2.2.dist-info/F${i}`,digest:`v${i}`,size:1}))];
  const a = await app(path => {
    if(path.endsWith('/snapshot')) return response(state({files}));
    if(path.startsWith('/api/sessions/a/files?revision=9')) return response({revision:9,files:[{path:'src/a.rs',digest:'a1',size:4},{path:'docs/x.md',digest:'x',size:4},{path:'src/old.rs',digest:'o',size:4}]});
  });
  await a.subject.openThread('a'); await a.nodes.get('files').click(); await a.flush();
  const list = a.nodes.get('file-list'), heads = () => list.descendants().filter(n => n.className === 'tree-folder-head');
  assert.deepEqual(heads().map(h => [h.dataset.folder, h['aria-expanded']]), [['.venv/lib/python3.14/site-packages/pip-26.2.2.dist-info','false'],['docs','false'],['src','false']], 'shut, never opened for the user; a lone folder chain is one row');
  assert.equal(heads()[0].children[0].textContent, '.venv/lib/python3.14/site-packages/pip-26.2.2.dist-info');
  const src = heads()[2].children[1];
  assert.equal(src.className, 'changed'); assert.equal(src.textContent, '2 files · 1 new · 1 changed · 1 deleted', 'changes are findable without opening, always in that order');
  assert.equal(heads()[1].children[1].textContent, '1 file'); assert(!heads()[1].children[1].className, 'a folder without changes is quiet');
  assert.equal(list.descendants().filter(n => n.dataset.path).length, 0, 'nothing inside a shut folder is drawn');
  heads()[2].onclick();
  const inside = list.children[2].children[1];
  assert.deepEqual(inside.children.map(n => n.dataset.path ?? n.children[0].dataset.folder), ['src/a.rs','src/old.rs','src/lib']);
  assert.match(textOf(inside.children[1]), /old\.rs.*deleted/);
  a.nodes.get('files-dialog').close(); await a.nodes.get('files').click(); await a.flush();
  assert.deepEqual(heads().map(h => [h.dataset.folder, h['aria-expanded']]).slice(1), [['docs','false'],['src','true'],['src/lib','false']], 'opened again, the sheet keeps what was open (and only that)');
});

test('a page renders as a site through a preview link of its revision; SVG and images as pictures, text as text', async () => {
  const files = [{path:'site/my page.html',digest:'h',size:10},{path:'logo.svg',digest:'s',size:5},{path:'notes.txt',digest:'n',size:3}];
  const token = 't'.repeat(64), base = `/api/preview/${token}/`;
  let mints = 0, refuse = false;
  const a = await app((path, data) => {
    if(path.endsWith('/snapshot')) return response(state({files}));
    if(path.startsWith('/api/sessions/a/files?')) return response({revision:9,files});
    if(path === '/api/sessions/a/preview') {mints++; return refuse ? response({error:'not found'},404) : response({token,url:base,revision:data.revision,expires_in:1800});}
    if(path.startsWith('/api/sessions/a/file?')) return {ok:true,status:200,text: async () => '<p>x</p>'};
  });
  await a.subject.openThread('a'); await a.nodes.get('files').click(); await a.flush();
  const open = index => fileRow(a, ['site/my page.html','logo.svg','notes.txt'][index]).onclick();
  await open(0); await a.flush();
  assert(a.nodes.get('viewer-dialog').open && a.nodes.get('files-dialog').open, 'the viewer opens over the list');
  assert.equal(a.nodes.get('file-html').src, `${base}site/my%20page.html`, 'path segments are encoded');
  assert.equal(a.nodes.get('file-html').hidden, false);
  assert.equal(a.nodes.get('file-open').hidden, false);
  assert.equal(a.nodes.get('file-open').href, `${base}site/my%20page.html`);
  assert.deepEqual(a.calls.filter(c => c.path === '/api/sessions/a/preview').map(c => c.data), [{revision:10}], 'minted as a POST for the viewed revision');
  assert.equal(a.calls.filter(c => c.path.startsWith('/api/sessions/a/file?')).length, 0, 'the source is read only when asked for');
  a.nodes.get('file-view-toggle').onclick(); await a.flush();
  assert.equal(a.nodes.get('file-content').textContent, '<p>x</p>'); assert.equal(a.nodes.get('file-html').hidden, true);
  a.nodes.get('viewer-back').onclick();
  assert.equal(a.nodes.get('viewer-dialog').open, false); assert(a.nodes.get('files-dialog').open, 'back returns to the list');
  await open(1); await a.flush();
  assert.equal(a.nodes.get('file-image').src, `${base}logo.svg`, 'an SVG is a picture of the revision');
  assert.equal(a.nodes.get('file-image').hidden, false); assert.equal(a.nodes.get('file-html').hidden, true);
  a.nodes.get('viewer-back').onclick(); await open(0); await a.flush();
  assert.equal(mints, 1, 'a live link of the revision is reused');
  a.nodes.get('viewer-back').onclick(); await open(2); await a.flush();
  assert.equal(a.nodes.get('file-html').hidden, true); assert.equal(a.nodes.get('file-open').hidden, true);
  assert.equal(a.nodes.get('file-content').textContent, '<p>x</p>');
  // Without a link (an older host, say) a page or SVG still renders alone.
  refuse = true; a.nodes.get('viewer-close').onclick();
  assert.equal(a.nodes.get('files-dialog').open, false, 'close leaves the files too');
  a.subject.render(state({files, head:11, revision:11}));
  await a.nodes.get('files').click(); await a.flush(); await open(0); await a.flush();
  assert.equal(a.nodes.get('file-html').src, '/api/sessions/a/file?revision=11&path=site%2Fmy%20page.html&render=1');
  assert.equal(a.nodes.get('file-open').hidden, true);
  a.nodes.get('viewer-back').onclick(); await open(1); await a.flush();
  assert.equal(a.nodes.get('file-html').src, '/api/sessions/a/file?revision=11&path=logo.svg&render=1');
});

test('GitHub preview access needs confirmation, is isolated from normal links and resets when reopened', async () => {
  const a = await app((path,data) => {
    if(path === '/api/sessions/a/preview') return response({url:data.network === 'github' ? '/github/' : '/isolated/',expires_in:1800});
  });
  await a.subject.openThread('a');
  await a.subject.openViewer('a',10,'site/index.html');
  assert.equal(a.nodes.get('file-html').src,'/isolated/site/index.html');
  const control = a.nodes.get('file-view-network');
  assert.equal(control.hidden,false);
  control.onclick();
  assert(a.nodes.get('confirm-dialog').open);
  assert.match(a.nodes.get('confirm-text').textContent,/send its contents and anything you type/);
  assert.equal(a.calls.filter(c => c.data?.network === 'github').length,0);
  await a.confirm();
  assert.equal(a.nodes.get('file-html').src,'/github/site/index.html');
  assert.equal(a.nodes.get('file-open').href,'/github/site/index.html');
  assert.deepEqual(a.calls.filter(c => c.path.endsWith('/preview')).map(c => c.data),[{revision:10},{revision:10,network:'github',scripts:'site/index.html'}]);
  control.onclick(); await a.flush();
  assert.equal(a.nodes.get('file-html').src,'/isolated/site/index.html');
  control.onclick();
  a.nodes.get('viewer-close').onclick();
  await a.subject.openViewer('a',10,'another.html');
  await a.confirm();
  assert.equal(a.nodes.get('file-html').src,'/isolated/another.html','an old confirmation cannot widen a different viewer');
  a.nodes.get('viewer-close').onclick();
  await a.subject.openViewer('a',10,'site/index.html');
  assert.equal(a.nodes.get('file-html').src,'/isolated/site/index.html');
});

test('a page previews without its scripts until they are let run, for that version of it', async () => {
  let digest = 'js1', page = 'page1', fetched = 'd1', notes = 'n1';
  const a = await app((path,data) => {
    if(path.startsWith('/api/sessions/a/files?')) return response({revision:10,files:[{path:'site/index.html',digest:page,size:1},{path:'site/app.js',digest,size:1},{path:'site/data.json',digest:fetched,size:1},{path:'site/notes.md',digest:notes,size:1}]});
    if(path.startsWith('/api/sessions/a/file?')) return {ok:true,status:200,text: async () => path.includes('app.js') ? "fetch('data.json')" : '<script src="app.js"></script>'};
    if(path === '/api/sessions/a/preview') return response({url:data.scripts ? '/scripted/' : '/quiet/',expires_in:1800});
  });
  await a.subject.openThread('a');
  await a.subject.openViewer('a',10,'site/index.html'); await a.flush();
  assert.equal(a.nodes.get('file-html').src,'/quiet/site/index.html','scripts off at first');
  const scripts = a.nodes.get('file-view-scripts');
  assert.equal(scripts.hidden,false); assert.equal(scripts.textContent,'Run scripts');
  scripts.onclick(); for(let n = 0; n < 6; n++) await a.flush();
  assert.equal(a.nodes.get('file-html').src,'/scripted/site/index.html');
  assert.equal(scripts.textContent,'Stop scripts');
  assert.deepEqual(a.calls.filter(c => c.path.endsWith('/preview')).map(c => c.data).at(-1),{revision:10,scripts:'site/index.html'},'scripts for this page only');
  // Opened again: this version runs without asking.
  a.nodes.get('viewer-close').onclick();
  await a.subject.openViewer('a',10,'site/index.html'); await a.flush();
  assert.equal(a.nodes.get('file-html').src,'/scripted/site/index.html','kept for this version');
  // A changed script file, or a changed page, is another version: it asks again.
  a.nodes.get('viewer-close').onclick(); digest = 'js2';
  await a.subject.openViewer('a',11,'site/index.html'); for(let n = 0; n < 6; n++) await a.flush();
  assert.equal(a.nodes.get('file-html').src,'/quiet/site/index.html','a new script waits for the user again');
  a.nodes.get('viewer-close').onclick(); digest = 'js1'; page = 'page2';
  await a.subject.openViewer('a',12,'site/index.html'); for(let n = 0; n < 6; n++) await a.flush();
  assert.equal(a.nodes.get('file-html').src,'/quiet/site/index.html','a new page waits for the user again');
  // A file its script names (what it fetches) is part of the version; one it never names is not.
  page = 'page1'; notes = 'n2';
  a.nodes.get('viewer-close').onclick();
  await a.subject.openViewer('a',13,'site/index.html'); for(let n = 0; n < 6; n++) await a.flush();
  assert.equal(a.nodes.get('file-html').src,'/scripted/site/index.html','an unnamed file changing asks nothing');
  fetched = 'd2';
  a.nodes.get('viewer-close').onclick();
  await a.subject.openViewer('a',14,'site/index.html'); for(let n = 0; n < 6; n++) await a.flush();
  assert.equal(a.nodes.get('file-html').src,'/quiet/site/index.html','the data its script fetches changing asks again');
  // Stop: back to the page without them, kept so.
  digest = 'js1';
  a.nodes.get('viewer-close').onclick();
  await a.subject.openViewer('a',10,'site/index.html'); await a.flush();
  a.nodes.get('file-view-scripts').onclick(); for(let n = 0; n < 6; n++) await a.flush();
  assert.equal(a.nodes.get('file-html').src,'/quiet/site/index.html');
});

test('where previews are contained, a page runs as it shows, under a grant that holds it to that', async () => {
  const a = await app((path,data) => {
    if(path.startsWith('/api/sessions/a/files?')) return response({revision:10,files:[{path:'site/index.html',digest:'p',size:1}]});
    if(path === '/api/sessions/a/preview') return response({url:data.contained ? '/held/' : '/quiet/',expires_in:1800});
  }, '', {RTCPeerConnection: function() {}});
  await a.subject.openThread('a');
  const opening = a.subject.openViewer('a',10,'site/index.html'); await a.flush();
  // The host's check page says whether this browser keeps a running page offline.
  const check = a.context.document.body.children.find(n => n.tagName === 'iframe');
  assert.equal(check.src,'/api/preview/check'); assert.equal(check.hidden,true);
  a.events.message({source: check.contentWindow, data: {spindleContained: true}});
  await opening; await a.flush();
  assert.equal(check.removed,true);
  assert.equal(a.nodes.get('file-html').src,'/held/site/index.html','it runs, unasked');
  assert.equal(a.nodes.get('file-view-scripts').hidden,true,'nothing to let');
  assert.deepEqual(a.calls.filter(c => c.path.endsWith('/preview')).map(c => c.data),[{revision:10,scripts:true,contained:true}]);
  // Asked once; one grant runs every page of the revision.
  a.nodes.get('viewer-close').onclick();
  await a.subject.openViewer('a',10,'other.html'); await a.flush();
  assert.equal(a.nodes.get('file-html').src,'/held/other.html');
  assert.equal(a.calls.filter(c => c.path.endsWith('/preview')).length,1);
  assert.equal(a.context.document.body.children.filter(n => n.tagName === 'iframe').length,1);
});

test('Files is a full-height list until a file opens; the viewer then fills the screen', async () => {
  const files = Array.from({length: 14}, (_, i) => ({path: `src/file${i}.rs`, digest: `d${i}`, size: 100 + i}));
  const a = await app(path => {
    if(path.endsWith('/snapshot')) return response(state({files}));
    if(path.startsWith('/api/sessions/a/files?')) return response({revision:9,files});
    if(path.startsWith('/api/sessions/a/file?')) return {ok:true,status:200,text: async () => 'fn main() {}\n'};
  });
  await a.subject.openThread('a');
  a.nodes.get('title-button').onclick(); assert(a.nodes.get('thread-dialog').open, 'the title opens the thread menu');
  await a.nodes.get('files').click(); await a.flush();
  assert.equal(a.nodes.get('thread-dialog').open, false); assert(a.nodes.get('files-dialog').open);
  assert.equal(a.nodes.get('viewer-dialog').open, false, 'only the list until a file is chosen');
  const list = a.nodes.get('file-list'), head = () => list.children[0].children[0];
  assert.equal(list.children.length, 1); assert.equal(head()['aria-expanded'], 'false', 'a folder starts shut');
  assert.match(textOf(head()), /src.*14 files/);
  head().onclick(); assert.equal(head()['aria-expanded'], 'true'); assert.equal(list.children[0].children[1].children.length, 14, 'opened, it lists its files');
  assert.equal(a.nodes.get('file-filter').hidden, false, 'a long list can be filtered');
  a.nodes.get('file-filter').value = 'file1'; a.nodes.get('file-filter').oninput();
  assert.deepEqual(list.children.map(row => row.dataset.path), ['src/file1.rs','src/file10.rs','src/file11.rs','src/file12.rs','src/file13.rs'], 'a filter lists the matching paths');
  a.nodes.get('file-filter').value = ''; a.nodes.get('file-filter').oninput();
  assert.equal(head()['aria-expanded'], 'true', 'cleared, the folders are as they were');
  a.nodes.get('file-filter').value = 'file1'; a.nodes.get('file-filter').oninput();
  await list.children[0].onclick(); await a.flush();
  assert(a.nodes.get('viewer-dialog').open); assert.equal(a.nodes.get('viewer-back').hidden, false, 'a way back to all files');
  assert.equal(a.nodes.get('file-view-name').textContent, 'file1.rs'); assert.match(a.nodes.get('file-view-meta').textContent, /^src\/ · r10$/);
  assert.equal(a.nodes.get('file-content').textContent, 'fn main() {}\n');
  // From the conversation the same viewer opens without the way back.
  a.nodes.get('viewer-close').onclick(); await a.subject.openViewer('a', 10, 'src/file2.rs'); await a.flush();
  assert(a.nodes.get('viewer-dialog').open); assert.equal(a.nodes.get('viewer-back').hidden, true); assert.equal(a.nodes.get('files-dialog').open, false);
  // No fixed split: the list and the viewer size to the space they have.
  const css = await readFile(new URL('../ui/style.css', import.meta.url), 'utf8');
  const rules = selector => [...css.replace(/\/\*[\s\S]*?\*\//g, '').matchAll(/([^{}]+)\{([^}]*)\}/g)].filter(m => m[1].split(',').some(part => part.trim() === selector)).map(m => m[2]).join(';');
  assert.doesNotMatch(rules('#file-list') + rules('#files-dialog #file-list'), /max-height/, 'the list is never held to part of the sheet');
  assert.match(rules('dialog.viewer[open]'), /display: flex; flex-direction: column/);
  assert.match(rules('#viewer-body'), /flex: 1 1 auto; min-height: 0/, 'the viewer body takes what the header leaves');
  assert.match(rules('#viewer-body > *'), /flex: 1 1 auto; min-height: 0/);
  assert.doesNotMatch(rules('#file-html') + rules('#file-image') + rules('#viewer-body > *'), /min-height: [1-9]|height: \d+dvh/, 'previews have no fixed height');
  assert.match(rules('#file-html'), /height: 100%/);
});

test('a promotion refused because the host source moved offers Update from host source', async () => {
  let moved = true;
  const a = await app(path => {
    if(path === '/api/sessions') return response({sessions:[{id:'a',name:'Alpha',dev:true}],providers:[{id:'glm',model:'demo'}],dev:true});
    if(path.endsWith('/snapshot')) return response(state(moved ? {source_moved:{paths:['store/ui/app.js','store/src/serve/mod.rs']}} : {}));
  });
  await a.subject.openThread('a');
  const notice = a.nodes.get('live').children.find(n => n.textContent?.startsWith('Not promoted'));
  assert(notice, 'the refusal is shown on the thread');
  assert.match(notice.textContent, /changed store\/ui\/app\.js, store\/src\/serve\/mod\.rs\. Nothing was built or installed/);
  const action = notice.children.find(n => n.tagName === 'button');
  assert.equal(action.textContent, 'Update from host source');
  action.onclick(); assert(a.nodes.get('confirm-dialog').open);
  assert.equal(a.calls.filter(c => c.path === '/api/commands').length, 0, 'nothing is sent before confirming');
  await a.confirm();
  const sent = a.calls.filter(c => c.path === '/api/commands').at(-1).data;
  assert.deepEqual([sent.kind, sent.session, sent.expected_revision], ['update', 'a', 10]);
  moved = false; a.subject.render(state());
  assert(!a.nodes.get('live').children.some(n => n.textContent?.startsWith('Not promoted')));
});

test('a stream that keeps reconnecting asks the host why, so an expired sign-in surfaces', async () => {
  let expired = false;
  const a = await app(path => expired && path === '/api/ping' ? {ok:false,status:0,type:'opaqueredirect',text: async () => ''} : undefined);
  await a.subject.openThread('a'); const stream = a.streams.at(-1);
  expired = true; stream.readyState = 0; stream.onerror(); await a.flush();
  assert.equal(a.nodes.get('connection').textContent, 'Signed out');
  assert.match(a.nodes.get('banner-text').textContent, /sign-in expired/);
});

test('a steered message shows in the live area as sent while running', async () => {
  const a = await app(); await a.subject.openThread('a'); const stream = a.streams.at(-1);
  stream.listeners.snapshot({lastEventId:'boot:1',data:JSON.stringify(state({busy:true,complete:false}))});
  stream.listeners.preview({lastEventId:'boot:2',data:JSON.stringify({type:'steered',text:'use the faster test'})});
  stream.listeners.preview({lastEventId:'boot:3',data:JSON.stringify({type:'notice',text:'Provider refused (HTTP 503); retrying in 4s (1/6)'})});
  const live = a.nodes.get('live').children;
  assert.equal(live[0].children[0].textContent,'YOU · WHILE RUNNING');
  assert.equal(live[0].children[1].textContent,'use the faster test');
  assert.match(live[1].textContent,/retrying in 4s \(1\/6\)/);
});

test('a waiting message can be sent now, and leaves the queue when the model reads it', async () => {
  const a = await app(); await a.subject.openThread('a'); const stream = a.streams.at(-1);
  stream.listeners.snapshot({lastEventId:'boot:1',data:JSON.stringify(state({busy:true,complete:false,queued:{messages:['also check the tests'],now:false}}))});
  const waiting = a.nodes.get('queued');
  assert.equal(waiting.hidden,false,'a reload shows what still waits');
  const row = waiting.children.at(-1), now = row.children.find(node => node.tagName === 'button');
  assert.equal(now.textContent,'Send now');
  await now.click(); await a.flush();
  const sent = a.calls.filter(c => c.data).at(-1);
  assert.equal(sent.path,'/api/sessions/a/steer'); assert.deepEqual(sent.data,{now:true});
  assert.equal(waiting.children[0].children[0].textContent,'YOU · SENDING NOW');
  assert(!waiting.children.at(-1).children.some(node => node.tagName === 'button'),'nothing more to tap');
  // The host's event can reach the screen before the reply: no copy is added twice.
  const b = await app(path => path.endsWith('/steer') ? response({queued:true,now:false,waiting:{messages:['first'],now:false}},202) : undefined);
  await b.subject.openThread('a'); const bs = b.streams.at(-1);
  bs.listeners.snapshot({lastEventId:'boot:1',data:JSON.stringify(state({busy:true,complete:false}))});
  bs.listeners.preview({lastEventId:'boot:2',data:JSON.stringify({type:'queued',queued:{messages:['first'],now:false}})});
  b.nodes.get('prompt').value = 'first'; await b.nodes.get('composer').onsubmit({preventDefault() {}});
  assert.equal(b.nodes.get('queued').children.filter(node => node.className === 'message user').length,1);
  // Another screen's message arrives through the host's event.
  stream.listeners.preview({lastEventId:'boot:2',data:JSON.stringify({type:'queued',queued:{messages:['also check the tests','and the docs'],now:true}})});
  assert.equal(waiting.children.filter(node => node.className === 'message user').length,2);
  stream.listeners.preview({lastEventId:'boot:3',data:JSON.stringify({type:'steered',text:'also check the tests'})});
  assert.equal(waiting.children.filter(node => node.className === 'message user').length,1,'the read one moves to the conversation');
  assert.equal(a.nodes.get('live').children.at(-1).children[1].textContent,'also check the tests');
  stream.listeners.preview({lastEventId:'boot:4',data:JSON.stringify({type:'steered',text:'and the docs'})});
  assert.equal(waiting.hidden,true);
  // Typed, the same request.
  stream.listeners.preview({lastEventId:'boot:5',data:JSON.stringify({type:'queued',queued:{messages:['one more'],now:false}})});
  await a.subject.runIntent('send now');
  assert.deepEqual(a.calls.filter(c => c.data).at(-1).data,{now:true});
});

test('history shows the words of a message sent while running, not the guidance around them', async () => {
  const a = await app(); await a.subject.openThread('a');
  const wrapped = '[queued-user-update] The user sent this while you were working. Guidance.\n\nProgress: completed 2 iterations, 1 tool calls so far. Phase: probe. Injection point: before final response.\nUser update:\nNeed a report for today';
  a.subject.render(state({messages:[
    {role:'user',content:[{type:'text',text:'first task'}]},
    {role:'assistant',content:[{type:'text',text:'working'}]},
    {role:'user',content:[{type:'text',text:wrapped,steer:'Need a report for today'}]},
  ]}));
  const shown = a.nodes.get('history').children.filter(node => node.className === 'message user');
  assert.equal(shown.length,2);
  assert.equal(shown[1].children[0].textContent,'YOU · WHILE RUNNING');
  assert.equal(shown[1].children[1].textContent,'Need a report for today');
  assert(!textOf(a.nodes.get('history')).includes('[queued-user-update]'),'the guidance is not shown');
});

test('attachments upload as files, keep an editable path and go with the next message', async () => {
  const a = await app(); await a.subject.openThread('a');
  const file = (name, size) => ({name, size});
  a.nodes.get('attach-input').files = [file('notes.md',12), file('big.bin',9 * 1024 * 1024), file('a.png',3)];
  a.nodes.get('attach-input').onchange(); await a.flush();
  const uploads = a.calls.filter(c => c.path === '/api/sessions/a/upload');
  assert.deepEqual(uploads.map(c => c.data.upload.name),['notes.md','a.png'],'files over 8 MiB are refused before upload');
  const chips = a.nodes.get('attachments').children;
  assert.equal(a.nodes.get('attachments').hidden,false); assert.equal(chips.length,2);
  chips[0].children[0].value = 'docs/notes.md'; chips[0].children[0].oninput();
  chips[1].children[2].onclick();
  assert.equal(a.nodes.get('attachments').children.length,1,'removed chip');
  a.nodes.get('prompt').value = 'read the notes'; await a.nodes.get('composer').onsubmit({preventDefault() {}});
  const say = a.calls.filter(c => c.path === '/api/commands').at(-1).data;
  assert.equal(say.kind,'say'); assert.deepEqual(say.attachments,[{path:'docs/notes.md',digest:'8'.padStart(64,'0')}]);
  assert.equal(a.nodes.get('attachments').hidden,true,'sent attachments are cleared');
  a.nodes.get('attach-input').files = [file('x.txt',1)]; a.nodes.get('attach-input').onchange(); await a.flush();
  a.nodes.get('attachments').children[0].children[0].value = '../escape.txt'; a.nodes.get('attachments').children[0].children[0].oninput();
  a.nodes.get('prompt').value = 'again'; await a.nodes.get('composer').onsubmit({preventDefault() {}});
  assert.match(a.nodes.get('toast').textContent,/not a workspace path/);
  assert.equal(a.calls.filter(c => c.path === '/api/commands').length,1,'an invalid path sends nothing');
});

test('providers list sign-in state; web sign-in links the URL and the pasted code completes it', async () => {
  let step = 0;
  const a = await app((path,data) => {
    if(path === '/api/login' && !data.credential) {step = 1; return response({message:'Open https://auth.example.com/authorize?x=1 and paste the callback URL or code here.',awaiting:true});}
    if(path === '/api/login') {step = 2; return response({message:'Signed in to ChatGPT.',awaiting:false});}
    if(path === '/api/logout') return response({message:'Signed out of GLM.'});
    if(path === '/api/logins') return response({logins:[{provider:'chatgpt',name:'ChatGPT',status:'missing',signed_in:false},{provider:'glm',name:'GLM',status:'auth',signed_in:true},{provider:'local',name:'Local',status:'not-required',signed_in:true}]});
  });
  await a.nodes.get('settings').onclick(); await a.flush();
  const rows = a.nodes.get('logins').children;
  // A row: its mark, name and state, then icons (each named) to sign in, again, or out.
  assert.deepEqual(rows.map(r => r.children[2].textContent),['not signed in','signed in','no sign-in needed']);
  assert.deepEqual(rows.map(r => r.className),['login-row','login-row ok','login-row'],'a filled square for a sign-in held; none needed is not one');
  assert.equal(rows[2].children.length,3,'nothing to sign in to');
  assert.deepEqual(rows[1].children.slice(3).map(b => b['aria-label']),['Sign in to GLM again','Sign out of GLM']);
  assert.equal(rows[0].children[3]['aria-label'],'Sign in to ChatGPT');
  rows[0].children[3].onclick(); assert.equal(a.nodes.get('login-form').hidden,false);
  await a.nodes.get('login-start').onclick(); await a.flush();
  const link = a.nodes.get('login-message').children.find(n => n.tagName === 'a');
  assert.equal(link.href,'https://auth.example.com/authorize?x=1'); assert.equal(link.rel,'noopener noreferrer');
  assert.equal(a.nodes.get('login-label').textContent,'Paste the callback URL or code');
  a.nodes.get('login-credential').value = 'https://localhost/callback?code=SECRET-CODE';
  a.nodes.get('login-form').onsubmit({preventDefault() {}}); await a.flush();
  assert.equal(a.calls.filter(c => c.path === '/api/login').at(-1).data.credential,'https://localhost/callback?code=SECRET-CODE');
  assert.equal(step,2); assert.equal(a.nodes.get('login-form').hidden,true); assert.equal(a.nodes.get('login-credential').value,'');
  assert(![...a.storage.values()].some(v => String(v).includes('SECRET-CODE')),'the credential never reaches local storage');
  a.nodes.get('logins').children[1].children[4].onclick(); assert(a.nodes.get('confirm-dialog').open); await a.confirm();
  assert.deepEqual(a.calls.filter(c => c.path === '/api/logout').at(-1).data,{provider:'glm'});
});

test('usage, compaction and a provider refusal are shown where they matter', async () => {
  const usage = {calls:3,input:120000,output:4500,cache_read:80000,cache_create:0,usd:0.4213,priced:3};
  const a = await app(path => path.endsWith('/snapshot') ? response(state({provider:'glm',model:'glm-5.3',usage,compaction:'digest'})) : undefined);
  await a.subject.openThread('a');
  assert.equal(a.nodes.get('spend').textContent,'$0.421');
  a.nodes.get('title-button').onclick();
  assert.equal(a.nodes.get('settings-usage').textContent,'Spent 3 calls · 120K in · 4.5K out · 80K cached · $0.421');
  const [model, digest] = a.nodes.get('compaction-choice').children;
  assert.equal(digest.getAttribute?.('aria-pressed') ?? digest['aria-pressed'], 'true');
  model.onclick(); await a.flush();
  assert.equal(a.calls.filter(c => c.path === '/api/commands').at(-1).data.compaction,'model');
  assert.deepEqual(Object.keys(a.calls.filter(c => c.path === '/api/commands').at(-1).data).filter(k => !['request_id','session','kind','expected_revision'].includes(k)), ['compaction'], 'only compaction is sent');
  a.nodes.get('thread-dialog').close();
  a.subject.render(state({complete:false,phase:'ModelWait',rejected:{status:400,detail:'model not found'}}));
  assert.equal(a.nodes.get('phase-text').textContent,'provider refused (HTTP 400): model not found · Retry');
  assert.equal(a.nodes.get('retry').hidden,false); assert.equal(a.nodes.get('continue').hidden,true);
  // A new thread's compaction is one of its folded options.
  await a.subject.home(); await a.subject.openPicker(); await a.flush();
  assert.equal(a.nodes.get('picker-more').hidden, false);
  a.nodes.get('new-compaction').value = 'digest'; a.nodes.get('new-compaction').onchange();
  a.nodes.get('model-apply').onclick();
  a.nodes.get('prompt').value = 'go'; await a.nodes.get('composer').onsubmit({preventDefault() {}}); await a.flush();
  assert.equal(a.calls.filter(c => c.path === '/api/commands').at(-1).data.compaction,'digest');
});

test('each turn, the thread, the thread list and the host show spend; the budget is a thread setting', async () => {
  const usage = {calls:4,input:1000,output:500,cache_read:0,cache_create:0,usd:0.0523,priced:4,subscription:0,unpriced:0,refused:0,unknown:1};
  const messages = [{role:'user',content:[{type:'text',text:'first'}]},{role:'assistant',content:[{type:'text',text:'done'}]},{role:'user',content:[{type:'text',text:'second'}]}];
  const turn_costs = [{message:0,calls:1,priced:1,usd:0.0123},{message:2,calls:3,priced:3,usd:0.04,unknown:1}];
  let snapshot = state({provider:'glm',model:'glm-5.3',usage,messages,turn_costs,budget:{max_usd:5,spent:0.0523,blocked:null}});
  const a = await app(path => {
    if(path === '/api/sessions') return response({sessions:[{id:'a',name:'Alpha',status:{state:'done',at:Date.now()},usd:0.0523},{id:'b',name:'Beta'}],providers:[{id:'glm',model:'demo'}]});
    if(path.endsWith('/snapshot')) return response(snapshot);
    if(path.startsWith('/api/usage?tz=')) return response({today:{usd:0.05},month:{usd:1.5},all:{usd:12.25,unknown:2},threads:[]});
  });
  assert.match(textOf(a.nodes.get('sessions').children.find(n => n.className?.startsWith('session-row'))), /Alpha .* Finished · \$0\.052/);
  await a.subject.openThread('a');
  assert.equal(a.nodes.get('spend').textContent, '$0.052', 'the header carries what the thread cost');
  assert.equal(a.nodes.get('spend')['aria-label'], 'Spent $0.052 of $5.00. Usage and limit');
  const labels = a.nodes.get('history').children.filter(n => n.className === 'meta turn-cost').map(n => n.textContent);
  assert.deepEqual(labels, ['$0.012 · 1 call', '$0.040 · 3 calls · 1 may have been billed'], 'each prompt carries its turn\'s cost');
  await a.subject.home(); await a.nodes.get('settings').onclick(); await a.flush();
  assert(a.nodes.get('settings-dialog').open, 'the host sheet opens from the thread list');
  const host = a.nodes.get('usage-info').children.map(n => n.textContent);
  assert.deepEqual(host, ['Today','$0.050','30 days','$1.50','All time','$12.25 · 2 may have been billed']);
  assert.equal(a.nodes.get('usage-state').textContent, 'today $0.050 · 30 days $1.50', 'a folded section says its state');
  a.nodes.get('settings-dialog').close();
  await a.subject.openThread('a'); a.nodes.get('title-button').onclick();
  assert.equal(a.nodes.get('settings-budget').value, '5');
  assert.match(a.nodes.get('settings-usage').textContent, /\$0\.052 · 1 may have been billed$/);
  const budget = () => a.nodes.get('budget-form').onsubmit({preventDefault() {}});
  const sent = () => a.calls.filter(c => c.path === '/api/commands');
  let count = sent().length;
  budget(); await a.flush();
  assert.equal(sent().length, count, 'an untouched budget is not sent');
  a.nodes.get('settings-budget').value = '12.5'; budget(); await a.flush();
  assert.equal(sent().at(-1).data.max_usd, 12.5);
  assert.deepEqual(Object.keys(sent().at(-1).data).filter(k => !['request_id','session','kind','expected_revision'].includes(k)), ['max_usd'], 'the budget alone is sent');
  a.nodes.get('settings-budget').value = ''; budget(); await a.flush();
  assert.equal(sent().at(-1).data.max_usd, null, 'an empty budget is unlimited');
  count = sent().length;
  a.nodes.get('settings-budget').value = 'lots'; budget(); await a.flush();
  assert.equal(sent().length, count, 'a bad amount is refused on the screen');
  assert.match(a.nodes.get('toast').textContent, /dollar amount/);
  a.nodes.get('thread-dialog').close();
  // A snapshot whose spend could not be read still carries the budget; one without a
  // budget leaves an empty field that saving does not turn into a clear.
  snapshot = state({provider:'glm',model:'glm-5.3',messages,budget:{max_usd:5,spent:null,uncertain:null,blocked:null}});
  await a.subject.openThread('a'); a.nodes.get('title-button').onclick();
  assert.equal(a.nodes.get('settings-budget').value, '5');
  a.nodes.get('thread-dialog').close();
  snapshot = state({provider:'glm',model:'glm-5.3',messages});
  await a.subject.openThread('a'); a.nodes.get('title-button').onclick();
  assert.equal(a.nodes.get('settings-budget').value, '');
  count = sent().length; budget(); await a.flush();
  assert.equal(sent().length, count, 'an empty field the screen filled is not a clear');
  a.nodes.get('thread-dialog').close();
  // A request the budget holds back: Continue (after raising it) sends it; Retry is not offered.
  const blocked = 'spending limit: this thread has spent $4.9900 of its $5.00 budget and the next model request could cost up to $0.0500, so nothing was sent';
  snapshot = state({complete:false,phase:'ModelWait',usage,messages,budget:{max_usd:5,spent:4.99,blocked}});
  a.subject.render(snapshot);
  assert.equal(a.nodes.get('phase-text').textContent, 'budget reached · raise it in the thread menu, then Continue');
  assert.equal(a.nodes.get('continue').hidden, false); assert.equal(a.nodes.get('retry').hidden, true);
  assert(a.nodes.get('live').children.some(n => n.textContent === `${blocked}.`), 'the end of the thread says why');
  // Live cost events show this call, the turn and the thread.
  const stream = a.streams.at(-1);
  stream.listeners.snapshot({lastEventId:'boot:1',data:JSON.stringify(state({busy:true,complete:false}))});
  stream.listeners.preview({lastEventId:'boot:2',data:JSON.stringify({type:'cost',usd:0.0042,turn:0.04,total:0.0565,billing:'priced'})});
  stream.listeners.preview({lastEventId:'boot:3',data:JSON.stringify({type:'cost',usd:0,turn:0,total:0.0565,billing:'subscription'})});
  const live = a.nodes.get('live').children.map(n => n.textContent);
  assert(live.includes('cost · $0.0042 · turn $0.040 · thread $0.057'), live.join(' | '));
  assert(live.includes('cost · subscription · turn $0.000 · thread $0.057'), live.join(' | '));
});

test('opening a thread shows none of the previous thread\'s actions until its own snapshot arrives', async () => {
  let release;
  const a = await app(path => path === '/api/sessions/b/snapshot' ? new Promise(resolve => release = () => resolve(response(state()))) : undefined);
  await a.subject.openThread('a'); a.subject.render(state({busy:true,complete:false}));
  assert.equal(a.nodes.get('stop').hidden,false);
  const opening = a.subject.openThread('b'); await a.flush();
  assert.equal(a.nodes.get('stop').hidden,true,'no Stop for a thread that is not loaded');
  assert.equal(a.nodes.get('phase-text').textContent,'loading');
  release(); await opening;
  assert.match(a.nodes.get('phase-text').textContent,/^ready/);
});

test('a double tap steers once; typed stop still goes through while a message is on its way', async () => {
  let finish, held = true;
  const a = await app(path => path.endsWith('/steer') && held ? new Promise(resolve => finish = () => {held = false; resolve(response({queued:true},202));}) : undefined);
  await a.subject.openThread('a'); a.subject.render(state({busy:true,complete:false}));
  a.nodes.get('prompt').value = 'also run clippy';
  const first = a.nodes.get('composer').onsubmit({preventDefault() {}}); await a.flush();
  await a.nodes.get('composer').onsubmit({preventDefault() {}}); await a.flush();
  assert.equal(a.calls.filter(c => c.path.endsWith('/steer')).length,1,'the second tap sends nothing');
  a.nodes.get('prompt').value = 'stop'; await a.nodes.get('composer').onsubmit({preventDefault() {}});
  assert.equal(a.calls.filter(c => c.path === '/api/commands').at(-1).data.kind,'stop');
  finish(); await first;
  a.nodes.get('prompt').value = 'also run clippy'; await a.nodes.get('composer').onsubmit({preventDefault() {}});
  assert.equal(a.calls.filter(c => c.path.endsWith('/steer')).length,2,'once settled, the same text can be sent again');
});

test('files picked for one thread never become attachments of another', async () => {
  const waits = [];
  const a = await app(path => path.endsWith('/upload') ? new Promise(resolve => waits.push(() => resolve(response({digest:'1'.padStart(64,'0'),size:1})))) : undefined);
  await a.subject.openThread('a');
  a.nodes.get('attach-input').files = [{name:'one.txt',size:1},{name:'two.txt',size:1},{name:'three.txt',size:1}];
  a.nodes.get('attach-input').onchange(); await a.flush();
  await a.subject.openThread('b'); waits.shift()(); await a.flush();
  assert.equal(a.calls.filter(c => c.path.endsWith('/upload')).length,1,'the rest of the batch is not uploaded');
  assert.equal(a.nodes.get('attachments').children.length,0);
  assert.equal(a.nodes.get('attachments').hidden,true);
});

test('two attachments for the same path are refused before sending', async () => {
  const a = await app(); await a.subject.openThread('a');
  a.nodes.get('attach-input').files = [{name:'notes.md',size:3},{name:'notes.md',size:4}];
  a.nodes.get('attach-input').onchange(); await a.flush();
  a.nodes.get('prompt').value = 'read both'; await a.nodes.get('composer').onsubmit({preventDefault() {}});
  assert.match(a.nodes.get('toast').textContent,/both be written to “notes.md”/);
  assert.equal(a.calls.filter(c => c.path === '/api/commands').length,0);
});

test('a refused credential says needs sign-in, explains itself at the end of the thread and opens the sign-in', async () => {
  const a = await app(); await a.subject.openThread('a');
  a.subject.render(state({complete:false,phase:'ModelWait',provider:'chatgpt',rejected:{status:401,detail:'token expired'}}));
  assert.equal(a.nodes.get('phase-text').textContent,'needs sign-in · chatgpt');
  assert.equal(a.nodes.get('bar-signin').hidden,false); assert.equal(a.nodes.get('retry').hidden,false);
  assert(!a.nodes.get('retry').classList.contains('primary'),'Sign in is the next step, not Retry');
  const note = a.nodes.get('live').children.at(-1);
  assert.match(note.textContent,/HTTP 401\): token expired\. Sign in below, then Retry/);
  assert.equal(a.nodes.get('prompt').placeholder,'Not ready for a message');
  await a.nodes.get('bar-signin').onclick(); await a.flush();
  assert(a.nodes.get('settings-dialog').open); assert.equal(a.nodes.get('login-form').hidden,false);
  assert.match(a.nodes.get('login-title').textContent,/chatgpt/);
  a.nodes.get('settings-dialog').close();
  a.subject.render(state());
  assert.equal(a.nodes.get('bar-signin').hidden,true); assert.equal(a.nodes.get('live').children.length,0,'a ready thread has no note');
  assert.equal(a.nodes.get('prompt').placeholder,'Message');
});

test('an expired sign-in shows its banner without a toast covering it', async () => {
  let expired = false;
  const a = await app(path => expired && path.endsWith('/snapshot') ? {ok:false,status:0,type:'opaqueredirect',text: async () => ''} : undefined);
  expired = true; await a.subject.openThread('a');
  assert.match(a.nodes.get('banner-text').textContent,/sign-in expired/);
  assert.notEqual(a.nodes.get('toast').textContent,'Sign in again to continue.');
  assert.equal(a.nodes.get('phase-text').textContent,'not loaded');
});

test('an unnamed new thread is named after its first message; success notices are not errors', async () => {
  const a = await app();
  a.nodes.get('prompt').value = '\n  Port the parser to a streaming API and keep the old function as a thin wrapper';
  await a.nodes.get('composer').onsubmit({preventDefault() {}}); await a.flush();
  const sent = a.calls.filter(c => c.path === '/api/commands').at(-1).data;
  assert.equal(sent.name,'Port the parser to a streaming API and keep the old…');
  assert.equal(a.storage.get('draft:home'), '', 'a started thread clears the home draft');
  await a.subject.openThread('a');
  a.nodes.get('title-button').onclick();
  assert.equal(a.nodes.get('rename').value, 'Alpha'); assert.equal(a.nodes.get('rename-save').hidden, true);
  a.nodes.get('rename').value = 'Better'; a.nodes.get('rename').oninput();
  assert.equal(a.nodes.get('rename-save').hidden, false, 'Rename shows once the name differs');
  a.nodes.get('rename-form').onsubmit({preventDefault() {}}); await a.flush();
  assert.equal(a.calls.filter(c => c.path === '/api/commands').at(-1).data.name, 'Better');
  assert.equal(a.nodes.get('toast').textContent,'Renamed.'); assert.equal(a.nodes.get('toast').dataset.kind,'info');
  a.nodes.get('toast').onclick(); assert.equal(a.nodes.get('toast').hidden,true,'a tap dismisses it');
});

test('the pairing dialog cannot be dismissed with Escape', async () => {
  const a = await app(path => path === '/api/sessions' ? response({error:'Pair first'},401) : undefined);
  let prevented = false; a.nodes.get('pair-dialog').oncancel({preventDefault() {prevented = true;}});
  assert(prevented);
});

test('a host restarted onto another build asks this screen to reload', async () => {
  let build = 'one';
  const a = await app(path => path === '/api/ping' ? response({boot:'boot', build}) : undefined);
  await a.flush(); await a.subject.openThread('a');
  const emit = (id, stream = a.streams.at(-1)) => stream.listeners.snapshot({lastEventId:`${id}:1`,data:JSON.stringify(state())});
  emit('boot'); emit('boot'); await a.flush();
  assert.equal(a.nodes.get('banner-text').textContent,'','same boot, nothing to compare');
  emit('boot2'); await a.flush();
  assert.equal(a.nodes.get('banner-text').textContent,'','a plain restart onto the same build');
  build = 'two'; emit('boot3'); await a.flush();
  assert.match(a.nodes.get('banner-text').textContent,/updated on the host/);
  assert.equal(a.nodes.get('banner-actions').children[0].textContent,'Reload');
});

test('a pack request is answered from its card, views stay under the history and progress updates in place', async () => {
  const a = await app(path => path.endsWith('/pack-ui') ? response({answered:true}) : undefined);
  await a.subject.openThread('a'); const stream = a.streams.at(-1);
  const emit = (type,seq,event) => stream.listeners[type]({lastEventId:`boot:${seq}`,data:JSON.stringify(event)});
  emit('snapshot',1,state({busy:true,complete:false,packs:[{pack:'counter',view:{title:'Runs',markdown:'# 2 runs\n```\nok\n```'},tools:[]}]}));
  const history = a.nodes.get('history').descendants();
  assert(history.some(n => n.textContent === 'Runs'), 'the latest view is a card under the history');
  assert(history.some(n => n.className === 'code' && n.textContent === 'ok'));
  emit('preview',2,{type:'pack_ui',id:'ui-1-1',pack:'counter',request_id:'q1',method:'form',params:{title:'Profile',fields:[{id:'name',label:'Name',type:'text',required:true},{id:'age',label:'Age',type:'number'},{id:'kind',label:'Kind',type:'select',options:['a',{value:'b',label:'Bee'}]}]}});
  const card = a.nodes.get('live').children.find(c => c.dataset?.packUi === 'ui-1-1');
  assert(card, 'the request shows as a card');
  const fields = card.descendants().filter(n => n.tagName === 'input' || n.tagName === 'select');
  fields[0].value = 'Ada'; fields[1].value = '36'; fields[2].value = '1';
  const submit = card.descendants().find(n => n.tagName === 'button' && n.textContent === 'Submit');
  await submit.onclick(); await a.flush();
  const sent = a.calls.find(c => c.path === '/api/sessions/a/pack-ui');
  assert.deepEqual(sent.data, {id:'ui-1-1',status:'ok',value:{name:'Ada',age:36,kind:'b'}});
  assert.equal(card.dataset.packUi, '');
  // A replayed request is not shown twice; done marks the card answered.
  emit('preview',3,{type:'pack_ui',id:'ui-1-2',pack:'counter',request_id:'q2',method:'confirm',params:{message:'Delete it?'}});
  emit('preview',4,{type:'pack_ui',id:'ui-1-2',pack:'counter',request_id:'q2',method:'confirm',params:{message:'Delete it?'}});
  assert.equal(a.nodes.get('live').children.filter(c => c.dataset?.packUi === 'ui-1-2').length, 1);
  const no = a.nodes.get('live').children.find(c => c.dataset?.packUi === 'ui-1-2').descendants().find(n => n.textContent === 'No');
  await no.onclick(); await a.flush();
  assert.deepEqual(a.calls.filter(c => c.path === '/api/sessions/a/pack-ui').at(-1).data, {id:'ui-1-2',status:'ok',value:false});
  emit('preview',5,{type:'pack_ui',id:'ui-1-3',pack:'counter',request_id:'q3',method:'select',params:{options:['x','y']}});
  const pick = a.nodes.get('live').children.find(c => c.dataset?.packUi === 'ui-1-3');
  assert.deepEqual(pick.descendants().filter(n => n.tagName === 'button').map(n => n.textContent), ['x','y','Dismiss']);
  emit('preview',6,{type:'pack_ui_done',id:'ui-1-3'});
  assert(pick.classList.contains('answered') && pick.dataset.packUi === '', 'a request answered elsewhere or withdrawn is marked done');
  assert.equal(pick.descendants().filter(n => n.tagName === 'button' && !n.disabled).length, 0, 'a closed request offers no live buttons');
  assert(pick.descendants().some(n => n.textContent === 'Closed'));
  // A form closed elsewhere keeps its fields readable but no longer editable.
  emit('preview',6,{type:'pack_ui',id:'ui-1-6',pack:'counter',request_id:'q6',method:'form',params:{fields:[{id:'name',label:'Name',type:'text'}]}});
  const form = a.nodes.get('live').children.find(c => c.dataset?.packUi === 'ui-1-6');
  emit('preview',6,{type:'pack_ui_done',id:'ui-1-6'});
  assert(form.descendants().filter(n => n.tagName === 'input').every(n => n.disabled));
  assert.equal(form.descendants().filter(n => n.tagName === 'button' && !n.disabled).length, 0);
  emit('preview',7,{type:'pack_ui',id:'ui-1-4',pack:'counter',request_id:'p',method:'progress',params:{id:'build',title:'Build',current:1,total:3}});
  emit('preview',8,{type:'pack_ui',id:'ui-1-5',pack:'counter',request_id:'p',method:'progress',params:{id:'build',title:'Build',current:2,total:3}});
  const progress = a.nodes.get('live').children.filter(c => c.dataset?.progress);
  assert.equal(progress.length, 1); assert.match(progress[0].textContent, /Build · 2\/3/);
  emit('preview',9,{type:'pack_view',pack:'counter',title:'Live view',markdown:'fresh'});
  assert(a.nodes.get('live').descendants().some(n => n.textContent === 'Live view'));
});

test('a pack follow-up the host never delivered is offered as Continue', async () => {
  const a = await app(); await a.subject.openThread('a');
  a.subject.render(state({follow_up:true}));
  assert.equal(a.nodes.get('continue').hidden, false);
  assert(a.nodes.get('continue').classList.contains('primary'));
  assert.match(a.nodes.get('phase-text').textContent, /follow-up waiting · Continue/);
  assert.equal(a.nodes.get('prompt').placeholder, 'Message', 'a new message is still taken');
  assert(a.nodes.get('live').children.some(n => /Continue delivers it/.test(n.textContent)), 'the end of the thread says why');
  a.subject.render(state());
  assert.equal(a.nodes.get('continue').hidden, true);
});

test('a pending pack request survives a reload through the snapshot', async () => {
  const a = await app(); await a.subject.openThread('a'); const stream = a.streams.at(-1);
  stream.listeners.snapshot({lastEventId:'boot:1',data:JSON.stringify(state({busy:true,complete:false,pack_ui:{type:'pack_ui',id:'ui-9-1',pack:'counter',request_id:'q',method:'input',params:{title:'Name?',default:'Ada'}}}))});
  const card = a.nodes.get('live').children.find(c => c.dataset?.packUi === 'ui-9-1');
  assert(card); assert(card.descendants().some(n => n.tagName === 'input' && n.value === 'Ada'));
});

// ---- Chip and picker, title menu, host sheet, self-edit card,
// inline display, tappable paths, pending settings. The /api/models catalog, self_edit,
// pending_settings and display fields follow the backend contract (mocked here).
const catalog = {providers: [
  {id:'glm', name:'GLM', signed_in:true, subscription:false, default_model:'glm-5.3', models:[
    {id:'glm-5.3', label:'GLM-5.3', efforts:['off','low','medium','high','max'], default_effort:'high', image:false, context:200000, price:{input:1.4,output:4.4,cached:0.26}},
    {id:'glm-5.2', label:'GLM-5.2', efforts:['off','high'], default_effort:'high', image:false, context:200000, price:{input:1.4,output:4.4,cached:0.26}}]},
  {id:'chatgpt', name:'ChatGPT', signed_in:true, subscription:true, default_model:'gpt-5.6', models:[
    {id:'gpt-5.6', label:'GPT-5.6', efforts:['low','medium','high','xhigh'], default_effort:'medium', image:true, context:1050000, price:null}]},
  {id:'anthropic', name:'Anthropic', signed_in:false, subscription:false, default_model:'claude-opus', models:[
    {id:'claude-opus', label:'Claude Opus', efforts:['high'], default_effort:'high', image:true, context:1000000, price:{input:5,output:25,cached:0.5}}]},
]};
const rowsOf = a => a.nodes.get('model-list').descendants().filter(n => n.dataset?.model);
const effortsShown = a => a.nodes.get('effort-choice').children.map(b => b.dataset.effort);
const commands = a => a.calls.filter(c => c.path === '/api/commands').map(c => c.data);
const fields = data => Object.fromEntries(Object.entries(data).filter(([k]) => !['request_id','session','kind','expected_revision'].includes(k)));

test('the model chip opens one sheet: providers with their models, only the efforts a model takes, and what changed is sent', async () => {
  let busy = false;
  const a = await app(path => {
    if(path === '/api/models') return response(catalog);
    if(path.endsWith('/snapshot')) return response(state({busy, complete: !busy, settings:{provider:'glm',model:'glm-5.3',effort:'max',network:false,toolchain:false,compaction:'model',max_usd:null}}));
  });
  await a.subject.openThread('a'); await a.flush();
  assert.equal(a.nodes.get('chip-model').textContent, 'glm-5.3'); assert.equal(a.nodes.get('chip-effort').textContent, 'max');
  await a.nodes.get('model-chip').onclick(); await a.flush();
  assert(a.nodes.get('model-dialog').open);
  assert.equal(a.nodes.get('model-label').textContent, 'THIS THREAD');
  assert.equal(a.nodes.get('picker-more').hidden, true, 'thread options live in the thread menu');
  const groups = a.nodes.get('model-list').children.map(g => g.children[0].children[0].textContent);
  assert.deepEqual(groups, ['GLM','ChatGPT','Anthropic']);
  const active = rowsOf(a).find(r => r['aria-pressed'] === 'true');
  assert.equal(active.dataset.model, 'glm-5.3');
  assert.match(textOf(active), /\$1\.40 \/ \$4\.40 per Mtok · 200K context/);
  assert.match(textOf(rowsOf(a).find(r => r.dataset.model === 'gpt-5.6')), /subscription · 1\.1M context.*image/);
  // The levels the model takes; auto is no button of its own: the model's default level is
  // outlined while none is chosen, and the line under them names it.
  assert.deepEqual(effortsShown(a), ['off','low','medium','high','max']);
  assert.equal(a.nodes.get('effort-choice').children.find(b => b.className === 'active').dataset.effort, 'max');
  assert.match(textOf(a.nodes.get('effort-default')), /High is this model's default\./);
  assert.equal(a.nodes.get('effort-default').children.find(n => n.className === 'link-like')?.textContent, 'Use the default', 'a chosen level can go back to auto');
  assert.equal(a.nodes.get('model-apply').textContent, 'Done', 'nothing changed yet');
  // A model with fewer levels: an effort it does not take falls back to auto.
  rowsOf(a).find(r => r.dataset.model === 'glm-5.2').onclick();
  assert.deepEqual(effortsShown(a), ['off','high']);
  assert.equal(a.nodes.get('effort-choice').children.find(b => b.className === 'active'), undefined, 'none chosen: auto');
  assert.equal(a.nodes.get('effort-choice').children.find(b => b.className === 'auto').dataset.effort, 'high', 'the level auto takes, outlined');
  assert.match(textOf(a.nodes.get('effort-default')), /High is this model's default; none chosen\./);
  assert.match(a.nodes.get('model-note').textContent, /^Next turn: glm-5\.2 · auto effort$/);
  a.nodes.get('effort-choice').children.find(b => b.dataset.effort === 'high').onclick();
  a.nodes.get('picker-network').checked = true; a.nodes.get('picker-network').onchange();
  assert.equal(a.nodes.get('model-apply').textContent, 'Apply');
  await a.nodes.get('model-apply').onclick(); await a.flush();
  assert.deepEqual(fields(commands(a).at(-1)), {provider:'glm', model:'glm-5.2', effort:'high', network:true});
  assert.equal(commands(a).at(-1).kind, 'settings');
  assert.equal(a.nodes.get('model-dialog').open, false);
  assert.match(a.nodes.get('toast').textContent, /Next turn uses: glm-5\.2 · high · network on/);
  // A signed-out provider's models lead to its sign-in, not to a choice.
  await a.nodes.get('model-chip').onclick(); await a.flush();
  rowsOf(a).find(r => r.dataset.model === 'claude-opus').onclick(); await a.flush();
  assert.equal(a.nodes.get('model-dialog').open, false); assert(a.nodes.get('settings-dialog').open);
  assert(a.nodes.get('login-settings').open, 'Host › Sign-ins opens'); assert.match(a.nodes.get('login-title').textContent, /Anthropic/);
  a.nodes.get('settings-dialog').close();
  // Search narrows; an unlisted id can still be used with the current provider.
  await a.nodes.get('model-chip').onclick(); await a.flush();
  a.nodes.get('model-search').value = 'gpt'; a.nodes.get('model-search').oninput();
  assert.deepEqual(rowsOf(a).map(r => r.dataset.model), ['gpt-5.6']);
  a.nodes.get('model-search').value = 'glm-6-preview'; a.nodes.get('model-search').oninput();
  const custom = a.nodes.get('model-list').children.find(n => n.className === 'model-row custom');
  assert.match(textOf(custom), /Use “glm-6-preview” with glm/);
  custom.onclick(); await a.nodes.get('model-apply').onclick(); await a.flush();
  assert.deepEqual(fields(commands(a).at(-1)), {provider:'glm', model:'glm-6-preview'}, 'an unlisted model keeps the effort');
  // While a turn runs, settings wait for the next one.
  busy = true; await a.subject.openThread('a'); await a.flush();
  await a.nodes.get('model-chip').onclick(); await a.flush();
  rowsOf(a).find(r => r.dataset.model === 'glm-5.2').onclick();
  assert.equal(a.nodes.get('model-apply').textContent, 'Apply after this turn');
});

test('starting a thread is just typing: the chip\'s choices and folded options go with the first message', async () => {
  const a = await app(path => path === '/api/models' ? response(catalog) : undefined);
  await a.flush();
  assert.equal(a.nodes.get('model-chip').hidden, false, 'the home composer has the chip');
  assert.equal(a.nodes.get('chip-model').textContent, 'glm-5.3'); assert.equal(a.nodes.get('chip-effort').textContent, 'high', 'auto shows the model default');
  await a.nodes.get('model-chip').onclick(); await a.flush();
  assert.equal(a.nodes.get('model-label').textContent, 'NEW THREAD'); assert.equal(a.nodes.get('picker-more').hidden, false);
  rowsOf(a).find(r => r.dataset.model === 'gpt-5.6').onclick();
  a.nodes.get('effort-choice').children.find(b => b.dataset.effort === 'xhigh').onclick();
  a.nodes.get('picker-toolchain').checked = true; a.nodes.get('picker-toolchain').onchange();
  a.nodes.get('new-name').value = 'Named'; a.nodes.get('new-budget').value = '3'; a.nodes.get('new-budget').onchange();
  a.nodes.get('model-dialog').close();
  assert.equal(a.nodes.get('chip-provider').textContent, 'chatgpt', 'the provider shows when the model name does not say it');
  assert.equal(a.nodes.get('chip-model').textContent, 'gpt-5.6'); assert.equal(a.nodes.get('chip-effort').textContent, 'xhigh');
  assert.equal(a.nodes.get('model-chip').dataset.toolchain, 'true');
  a.nodes.get('prompt').value = 'Port the parser'; await a.nodes.get('composer').onsubmit({preventDefault() {}}); await a.flush();
  const sent = commands(a).at(-1);
  assert.equal(sent.kind, 'new');
  assert.deepEqual({...fields(sent), text: undefined}, {name:'Named', text: undefined, provider:'chatgpt', model:'gpt-5.6', effort:'xhigh', compaction:'model', network:false, toolchain:true, max_usd:3});
  assert.equal(JSON.parse(a.storage.get('new-extras')).name, '', 'a name is for one thread; the rest is remembered');
  // Typed, at home: "use glm-5.2 high" sets what the next thread starts with.
  await a.subject.home(); await a.subject.runIntent('use glm-5.2 high');
  assert.deepEqual(JSON.parse(a.storage.get('new-settings')), {provider:'glm', model:'glm-5.2', effort:'high', network:false, toolchain:true});
  assert.equal(commands(a).length, 1, 'nothing is sent for a thread not started');
});

test('without the host catalog the picker still offers each provider\'s model and every effort', async () => {
  const a = await app();
  await a.nodes.get('model-chip').onclick(); await a.flush();
  assert(a.subject.catalog.fallback);
  assert.deepEqual(rowsOf(a).map(r => [r.dataset.provider, r.dataset.model]), [['glm','demo']]);
  assert.deepEqual(effortsShown(a), ['off','low','medium','high','xhigh','max']);
});

test('typed settings: model, network, toolchain, budget and compaction go through the settings command', async () => {
  const a = await app(path => path === '/api/models' ? response(catalog) : undefined);
  await a.subject.openThread('a'); await a.flush();
  const typed = async text => {a.nodes.get('prompt').value = text; await a.nodes.get('composer').onsubmit({preventDefault() {}}); await a.flush(); return fields(commands(a).at(-1));};
  assert.deepEqual(await typed('use glm-5.2 max'), {provider:'glm', model:'glm-5.2', effort:'max'});
  assert.deepEqual(await typed('/model gpt-5.6'), {provider:'chatgpt', model:'gpt-5.6'}, 'the thread keeps its (auto) effort');
  assert.deepEqual(await typed('network on'), {network:true});
  assert.deepEqual(await typed('disable toolchain'), {toolchain:false});
  assert.deepEqual(await typed('budget $7.50'), {max_usd:7.5});
  assert.deepEqual(await typed('no budget'), {max_usd:null});
  assert.deepEqual(await typed('compaction digest'), {compaction:'digest'});
  const count = commands(a).length;
  a.nodes.get('prompt').value = 'use the async API'; await a.nodes.get('composer').onsubmit({preventDefault() {}}); await a.flush();
  assert.equal(commands(a).at(-1).kind, 'say', 'an unknown model name is a message to the model');
  assert.equal(commands(a).length, count + 1);
});

test('settings waiting for the next turn show above the status row and open the picker', async () => {
  let pending = {model:'glm-5.3', effort:'max'};
  const a = await app(path => path.endsWith('/snapshot') ? response(state({busy:true, complete:false, pending_settings: pending})) : undefined);
  await a.subject.openThread('a');
  assert.equal(a.nodes.get('pending-settings').hidden, false);
  assert.equal(a.nodes.get('pending-settings').textContent, 'Applies next turn: glm-5.3 · max');
  await a.nodes.get('pending-settings').onclick(); await a.flush();
  assert(a.nodes.get('model-dialog').open);
  a.nodes.get('model-dialog').close();
  pending = {network:true, max_usd:2}; a.subject.render(state({pending_settings: pending}));
  assert.equal(a.nodes.get('pending-settings').textContent, 'Applies next turn: network on · budget $2.00');
  a.subject.render(state({pending_settings: null}));
  assert.equal(a.nodes.get('pending-settings').hidden, true);
});

test('the host sheet folds its sections, each saying its state; rollback is a host-level command', async () => {
  const a = await app(path => {
    if(path === '/api/sessions') return response({sessions:[{id:'a',name:'Alpha'},{id:'s',name:'Self',dev:true}],providers:[{id:'glm',model:'demo'}],dev:true});
    if(path === '/api/host') return response({version:'0.9.0',build:'b605732abcdef99',schema:16,engine:'e',sessions:2,running:0,supervised:true,dev:true,code_policy:{models:[{provider:'glm',model:'glm-5.3'}],tools:[{name:'web',risk:'network'},{name:'mcp',risk:'danger'}],limits:null}});
    if(path === '/api/devices') return response({devices:[{id:'d',label:'iPhone',created:1,seen:2,current:true},{id:'e',label:'Laptop',created:1,seen:2}]});
    if(path === '/api/sessions/s/snapshot') return response(state({head:42,revision:42}));
  });
  assert.equal(a.nodes.get('settings').hidden, false, 'the gear is on the thread list');
  await a.nodes.get('settings').onclick(); await a.flush();
  for(const id of ['appearance-section','login-settings','screens-section','notify-section','code-section','usage-section','host-section']) assert.equal(a.nodes.get(id).open, false, `${id} starts folded`);
  assert.equal(a.nodes.get('appearance-state').textContent, '');
  assert.equal(a.nodes.get('logins-state').textContent, '0 of 1 signed in');
  assert.equal(a.nodes.get('devices-state').textContent, '2 paired');
  assert.equal(a.nodes.get('notify-state').textContent, 'unsupported');
  assert.equal(a.nodes.get('code-state').textContent, '1 model · 2 tools');
  assert.equal(a.nodes.get('host-state').textContent, 'build b605732abcde');
  assert.equal(a.nodes.get('rollback').disabled, false);
  a.nodes.get('rollback').onclick(); assert(a.nodes.get('confirm-dialog').open); await a.confirm();
  const sent = commands(a).at(-1);
  assert.deepEqual([sent.kind, sent.session, sent.expected_revision], ['rollback',undefined,undefined]);
  // Inside a thread the header has the title menu instead of the gear.
  await a.subject.openThread('a');
  assert.equal(a.nodes.get('settings').hidden, true); assert.equal(a.nodes.get('title-button').hidden, false);
});

test('a thread that changed Spindle shows its change at the end: files, host moved, a requested install, Install and Dismiss', async () => {
  let change = {prefix:'spindle/', changed:['spindle/store/ui/app.js','store/ui/style.css'], changed_count:2, behind:true, conflicts:[], request:{revision:10, summary:'Model chip in the composer', at:Date.now() - 60000}, installing:false, host_build:'b605732'};
  let busy = false;
  const a = await app(path => path.endsWith('/snapshot') ? response(state({busy, complete: !busy, self_edit: change, files:[{path:'spindle/store/ui/app.js',digest:'x',size:1},{path:'spindle/store/ui/style.css',digest:'y',size:1}]})) : undefined);
  await a.subject.openThread('a');
  const card = a.nodes.get('history').children.at(-1);
  assert.equal(card.className, 'tool-card change-card requested');
  assert.match(textOf(card), /install requested/); assert.match(textOf(card), /Model chip in the composer/);
  assert.match(textOf(card), /2 changed files in spindle\//);
  assert.match(textOf(card), /The host source moved since this thread last synced/);
  assert.match(textOf(card), /host build b605732/);
  const links = card.descendants().filter(n => n.className === 'path-link');
  assert.deepEqual(links.map(l => l.textContent), ['store/ui/app.js','store/ui/style.css'], 'paths are shown under the prefix');
  assert.match(links[1].href, /path=spindle%2Fstore%2Fui%2Fstyle\.css/, 'a path the snapshot gives without the prefix gets it');
  const buttons = Object.fromEntries(card.descendants().filter(n => n.tagName === 'button').map(b => [b.textContent, b]));
  assert.deepEqual(Object.keys(buttons), ['Dismiss','Update','Install…']);
  assert(buttons['Install…'].className.includes('primary'));
  buttons['Install…'].onclick();
  assert(a.nodes.get('confirm-dialog').open); assert.equal(a.nodes.get('confirm-title').textContent, 'Install this Spindle change?');
  assert.equal(a.nodes.get('confirm-yes').textContent, 'Install');
  assert.equal(a.nodes.get('confirm-steps').hidden, false);
  assert(a.nodes.get('confirm-steps').children.some(step => /Restarts the host as soon as no thread is running/.test(step.textContent)));
  assert(a.nodes.get('confirm-steps').children.some(step => /unit tests/.test(step.textContent)));
  assert.equal(commands(a).length, 0, 'nothing is sent before confirming');
  await a.confirm();
  assert.deepEqual([commands(a).at(-1).kind, commands(a).at(-1).expected_revision], ['promote', 10]);
  buttons.Dismiss.onclick(); await a.flush();
  assert.equal(commands(a).at(-1).kind, 'dismiss_install');
  // Typed, the same confirmation.
  a.nodes.get('prompt').value = '/install'; await a.nodes.get('composer').onsubmit({preventDefault() {}});
  assert(a.nodes.get('confirm-dialog').open); a.nodes.get('confirm-dialog').close();
  // Conflicts: Update is the way forward; Install waits.
  change = {...change, request:null, conflicts:['spindle/store/ui/app.js']}; a.subject.render(state({self_edit: change}));
  const conflicted = a.nodes.get('history').children.at(-1);
  const now = Object.fromEntries(conflicted.descendants().filter(n => n.tagName === 'button').map(b => [b.textContent, b]));
  assert.equal(now['Install…'].disabled, true); assert(now.Update.className.includes('primary'));
  assert.match(textOf(conflicted), /Conflicts with the host source: spindle\/store\/ui\/app\.js/);
  // A gate stopped the install: its output on the card, and one tap hands it to the
  // model as the next message (with what to do about it).
  change = {...change, conflicts:[], failure:{command:'cargo clippy --workspace -- -D warnings', output:'error: this `map_or` can be simplified\n --> store/src/agent.rs:3478'}};
  a.subject.render(state({self_edit: change}));
  const failed = a.nodes.get('history').children.at(-1);
  assert.match(textOf(failed), /Install stopped at: cargo clippy --workspace -- -D warnings/);
  assert.match(textOf(failed), /map_or` can be simplified/);
  const fix = failed.descendants().find(n => n.tagName === 'button' && n.textContent === 'Ask the thread to fix it');
  assert(fix && fix.className.includes('primary'));
  fix.onclick(); await a.flush();
  const asked = commands(a).at(-1);
  assert.equal(asked.kind, 'say');
  assert.match(asked.text, /stopped at `cargo clippy --workspace -- -D warnings`[\s\S]*map_or[\s\S]*call spindle_install again/);
  change = {...change, failure:null};
  // Installing: progress follows as live notices; nothing on the card acts.
  busy = true; change = {...change, conflicts:[], installing:true}; a.subject.render(state({busy:true, complete:false, self_edit: change}));
  const installing = a.nodes.get('history').children.at(-1);
  assert.match(textOf(installing), /Installing: checks and the build run offline; the host restarts when no thread is running/);
  assert(installing.descendants().filter(n => n.tagName === 'button').every(b => b.disabled));
  // A checkout with nothing changed, requested, running or refused shows no card.
  busy = false; a.subject.render(state({self_edit: {...change, changed:[], changed_count:0, conflicts:[], installing:false, request:null}}));
  assert(!a.nodes.get('history').children.some(n => /Spindle change/.test(textOf(n))), 'no card for an idle checkout');
  a.subject.render(state({busy:true, complete:false, self_edit: change}));
  // The thread menu says it too.
  a.nodes.get('title-button').onclick();
  assert.equal(a.nodes.get('menu-change').hidden, false); assert.equal(a.nodes.get('menu-change-state').textContent, '2 files · installing');
});

test('what a tool shows appears in the conversation as a card per kind; a show call is only its cards', async () => {
  const token = 'p'.repeat(64), base = `/api/preview/${token}/`, display = [
    {path:'site/index.html', kind:'html', title:'Report', size:900, revision:10},
    {path:'chart.png', kind:'image', title:'Chart', size:4000, revision:10},
    {path:'logo.svg', kind:'svg', size:300, revision:10},
    {path:'notes/plan.md', kind:'markdown', title:'Plan', size:80, revision:10},
    {path:'paper.pdf', kind:'pdf', title:'Paper', size:90000, revision:10},
    {path:'log.txt', kind:'text', size:40, revision:10},
    {path:'data.bin', kind:'file', size:7, revision:10},
  ];
  const messages = [{role:'assistant',content:[{type:'tool_use',id:'s1',name:'show',input:{paths:display.map(d => d.path)}}]},{role:'user',content:[{type:'tool_result',tool_use_id:'s1',content:'shown',display}]},
    {role:'assistant',content:[{type:'tool_use',id:'w1',name:'write_file',input:{path:'out.html',content:'<p>'}}]},{role:'user',content:[{type:'tool_result',tool_use_id:'w1',content:'Wrote out.html',display:[{path:'out.html',kind:'html',revision:10}]}]}];
  const a = await app(path => {
    if(path.endsWith('/snapshot')) return response(state({messages, files: display.map(d => ({path:d.path,digest:'d',size:d.size}))}));
    if(path === '/api/sessions/a/preview') return response({token, url:base, expires_in:1800});
    if(path.startsWith('/api/sessions/a/file?revision=10&path=notes%2Fplan.md')) return {ok:true,status:200,text: async () => '# Plan\n\n- **one**\n- two <b>raw</b>'};
    if(path.startsWith('/api/sessions/a/file?revision=10&path=log.txt')) return {ok:true,status:200,text: async () => 'line 1\nline 2'};
  });
  await a.subject.openThread('a'); await a.flush();
  const history = a.nodes.get('history').children;
  assert(!history.some(n => n.dataset?.call === 's1'), 'no separate card for the show call');
  const cards = history.filter(n => n.tagName === 'figure');
  assert.deepEqual(cards.map(c => c.dataset.kind), ['html','image','svg','markdown','pdf','text','file','html']);
  const find = (card, tag) => card.descendants().find(n => n.tagName === tag);
  const frame = find(cards[0], 'iframe');
  assert.equal(frame.src, `${base}site/index.html`); assert.equal(frame.sandbox, 'allow-scripts allow-forms allow-modals'); assert.equal(frame.referrerpolicy, 'no-referrer');
  assert(cards[0].descendants().some(n => n.className === 'display-open'), 'the inline page is a picture: a tap opens the viewer');
  assert.equal(find(cards[1], 'img').src, `${base}chart.png`); assert.equal(find(cards[1], 'img').alt, 'Chart');
  assert.equal(find(cards[2], 'img').src, `${base}logo.svg`, 'an SVG shows as an image (no script)');
  const md = cards[3].descendants();
  assert(md.some(n => n.tagName === 'h3' && textOf(n).includes('Plan')));
  assert(md.some(n => n.tagName === 'strong' && textOf(n).includes('one')));
  assert(md.some(n => n.tagName === 'li' && textOf(n).includes('<b>raw</b>')), 'markup in Markdown stays text');
  assert(!md.some(n => n.tagName === 'b'));
  assert.equal(find(cards[4], 'a').href, `${base}paper.pdf`); assert.equal(find(cards[4], 'a').rel, 'noopener noreferrer');
  assert.equal(cards[5].descendants().find(n => n.className === 'display-pre').textContent, 'line 1\nline 2');
  assert.equal(textOf(cards[6]).includes('View file'), true);
  assert(history.some(n => n.dataset?.call === 'w1'), 'another tool keeps its card, with its display under it');
  assert.equal(a.calls.filter(c => c.path === '/api/sessions/a/preview').length, 1, 'one preview link serves every card of the revision');
  // A re-render keeps the same cards (a loaded preview does not reload).
  a.subject.render(a.subject.snapshot);
  assert.equal(a.nodes.get('history').children.filter(n => n.tagName === 'figure')[0], cards[0]);
  // Expanding opens the viewer over the conversation.
  await cards[0].descendants().find(n => n.className === 'display-expand').onclick(); await a.flush();
  assert(a.nodes.get('viewer-dialog').open); assert.equal(a.nodes.get('file-html').src, `${base}site/index.html`); assert.equal(a.nodes.get('viewer-back').hidden, true);
  // A live result shows its display at once.
  a.nodes.get('viewer-dialog').close();
  const stream = a.streams.at(-1);
  stream.listeners.snapshot({lastEventId:'boot:1',data:JSON.stringify(state({busy:true,complete:false}))});
  stream.listeners.preview({lastEventId:'boot:2',data:JSON.stringify({type:'tool',id:'s2',name:'show',input:{path:'new.png'}})});
  stream.listeners.preview({lastEventId:'boot:3',data:JSON.stringify({type:'result',content:'shown',failed:false,display:[{path:'new.png',kind:'image',revision:11}]})});
  await a.flush();
  assert(a.nodes.get('live').children.some(n => n.tagName === 'figure' && n.dataset.path === 'new.png'));
});

test('workspace paths in replies, prompts and tool results open the file viewer over the chat', async () => {
  const files = [{path:'src/lib.rs',digest:'a',size:10},{path:'Cargo.toml',digest:'b',size:3},{path:'tests/upload.rs',digest:'c',size:5}];
  const messages = [{role:'user',content:[{type:'text',text:'Look at src/lib.rs please'}]},
    {role:'assistant',content:[{type:'text',text:'In `tests/upload.rs` and ./Cargo.toml (not src/missing.rs or https://x.example/src/lib.rs).'},{type:'tool_use',id:'b1',name:'bash',input:{command:'grep -rn sleep'}}]},
    {role:'user',content:[{type:'tool_result',tool_use_id:'b1',content:'/workspace/tests/upload.rs:7: sleep(50)'}]},
    {role:'assistant',content:[{type:'tool_use',id:'e1',name:'edit_file',input:{path:'src/lib.rs',old_string:'a',new_string:'b'}}]},{role:'user',content:[{type:'tool_result',tool_use_id:'e1',content:'Edited'}]},
    {role:'assistant',content:[{type:'tool_use',id:'w1',name:'write_file',input:{path:'Cargo.toml',content:'x\ny\n'}}]},{role:'user',content:[{type:'tool_result',tool_use_id:'w1',content:'Wrote'}]}];
  const a = await app(path => {
    if(path.endsWith('/snapshot')) return response(state({files, messages}));
    if(path.startsWith('/api/sessions/a/file?')) return {ok:true,status:200,text: async () => 'pub fn x() {}\n'};
  });
  await a.subject.openThread('a');
  const links = a.nodes.get('history').descendants().filter(n => n.className === 'path-link');
  assert.deepEqual(links.map(l => textOf(l).trim()), ['src/lib.rs','tests/upload.rs','./Cargo.toml','/workspace/tests/upload.rs','src/lib.rs','Cargo.toml'], 'known files only, in prose, code spans, results and the changes line');
  assert.equal(links[0].href, '/api/sessions/a/file?revision=10&path=src%2Flib.rs', 'long-press can open the raw file');
  await links[2].onclick({preventDefault() {}}); await a.flush();
  assert(a.nodes.get('viewer-dialog').open); assert.equal(a.nodes.get('file-view-name').textContent, 'Cargo.toml');
  assert.equal(a.nodes.get('file-content').textContent, 'pub fn x() {}\n'); assert.equal(a.nodes.get('files-dialog').open, false, 'not the Files sheet');
  // The turn's edits, collapsed at its end.
  const changes = a.nodes.get('history').children.find(n => n.className === 'turn-changes');
  assert.equal(changes.children[0].textContent, '✎ 2 files changed');
  // Tool cards say what came back, collapsed.
  const subs = a.nodes.get('history').descendants().filter(n => n.className === 'tool-sub').map(n => n.textContent);
  assert.deepEqual(subs, ['/workspace/tests/upload.rs:7: sleep(50)', '+1 −1', '2 lines']);
});

test('steps fold into one row, Stop takes Send\'s place, a steered message is a checked bubble, and a delete waits a moment for Undo', async () => {
  const calls = [1,2,3].map(n => ({type:'tool_use', id:`c${n}`, name:'bash', input:{command:`echo ${n}`}}));
  const results = [1,2,3].map(n => ({type:'tool_result', tool_use_id:`c${n}`, content: n === 2 ? 'boom' : 'ok', is_error: n === 2}));
  const messages = [{role:'assistant',content:[{type:'text',text:'Checking.'},...calls]},{role:'user',content:[...results,{type:'text',text:'x',steer:'also this'}]}];
  const a = await app(path => path.endsWith('/snapshot') ? response(state({messages})) : undefined);
  await a.subject.openThread('a');
  // Three calls in a row: one folding row, its failure counted; the cards inside it.
  const group = a.nodes.get('history').children.find(n => n.className === 'steps');
  assert(group, 'a run of steps is one group');
  assert.deepEqual(group.children.slice(1).map(c => c.dataset.call), ['c1','c2','c3']);
  assert.match(textOf(group.children[0]), /Ran 3 steps/);
  assert.equal(group.children[0].children.find(n => n.className === 'steps-count')?.['aria-label'], '1 failed');
  assert.equal(group.open, false, 'folded until opened');
  // What was sent while it ran: a bubble, checked once read.
  const steered = a.nodes.get('history').children.find(n => n.children?.[0]?.textContent === 'YOU · WHILE RUNNING');
  assert.equal(steered.children[0].children.find(n => n.tagName === 'svg')?.['aria-label'], 'Read by the running turn');
  // While a run goes, Stop is in Send's place until something is written.
  a.subject.render(state({busy:true, complete:false}));
  assert.equal(a.nodes.get('stop').hidden, false); assert.equal(a.nodes.get('send').hidden, true);
  a.nodes.get('prompt').value = 'steer it'; a.nodes.get('prompt').oninput();
  assert.equal(a.nodes.get('stop').hidden, true); assert.equal(a.nodes.get('send').hidden, false);
  a.nodes.get('prompt').value = ''; a.nodes.get('prompt').oninput();
  // Delete from a row's ⋯: off the list at once, Undo brings it back; nothing is sent until
  // the moment passes.
  await a.subject.home(); await a.flush();
  const rows = () => a.nodes.get('sessions').children.filter(n => typeof n.className === 'string' && n.className.startsWith('session-row'));
  const named = () => rows().map(r => textOf(r).includes('Alpha') ? 'Alpha' : 'Beta');
  assert.deepEqual(named(), ['Alpha','Beta']);
  const deleting = () => {rows()[0].children[1].onclick({stopPropagation() {}}); a.nodes.get('sessions').children.find(n => n.className === 'row-actions').children[0].onclick();};
  deleting();
  assert.deepEqual(named(), ['Beta']); assert.match(a.nodes.get('toast').textContent, /Deleted “Alpha”/);
  const undo = a.nodes.get('toast').children.find(n => n.className === 'toast-action');
  assert.equal(undo.textContent, 'Undo');
  undo.onclick({stopPropagation() {}});
  assert.deepEqual(named(), ['Alpha','Beta']);
  assert(!a.calls.some(c => c.data?.kind === 'delete'), 'nothing sent before the moment passes');
  // Going back into it (the browser's Back, say) keeps it too.
  deleting(); await a.subject.openThread('a'); await a.subject.home(); await a.flush();
  assert.deepEqual(named(), ['Alpha','Beta']); a.subject.finishDelete();
  assert(!a.calls.some(c => c.data?.kind === 'delete'), 'reopened, it was kept');
  deleting(); a.subject.finishDelete(); await a.flush();
  const sent = a.calls.filter(c => c.data?.kind === 'delete');
  assert.deepEqual(sent.map(c => c.data.session), ['a']);
  assert(!('expected_revision' in sent[0].data), 'a delete names no revision of another thread');
});
