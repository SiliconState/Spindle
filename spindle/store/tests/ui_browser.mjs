import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {readFile, writeFile, mkdtemp, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import test from 'node:test';

const browser = process.env.SPINDLE_BROWSER;
test('real DOM mobile startup, sheets, viewer, history, commands and layout (offline fixture)', {skip: !browser}, async () => {
  const ui = new URL('../ui/', import.meta.url);
  let html = await readFile(new URL('index.html',ui),'utf8');
  const [css,theme,terminal,commands,text,app] = await Promise.all(['style.css','theme.js','terminal.js','commands.js','text.js','app.js'].map(name => readFile(new URL(name,ui),'utf8')));
  const clean = text => text.replace(/^import .*;\n/gm,'').replace(/^export /gm,'');
  const fixture = `
    const requests = [];
    const stored = new Map();
    Object.defineProperty(globalThis,'localStorage',{value:{getItem:k=>stored.get(k),setItem:(k,v)=>stored.set(k,v)}});
    Object.defineProperty(globalThis,'crypto',{value:{randomUUID:()=>String(Math.random())}});
    globalThis.EventSource = class {close() {} addEventListener() {}};
    globalThis.fetch = async (path,options={}) => {
      const data = options.body ? JSON.parse(options.body) : null;
      requests.push({path,data});
      let value;
      const files=[{path:'site/index.html',digest:'h',size:900},...Array.from({length:30},(_,i)=>({path:'src/module_'+i+'.rs',digest:'d'+i,size:100+i}))];
      if(path === '/api/sessions') value={sessions:[{id:'fixture',name:'Mobile review'}],providers:[{id:'glm',model:'fixture-model'}],dev:false};
      else if(path === '/api/models') value={providers:[{id:'glm',name:'GLM',signed_in:true,subscription:false,default_model:'fixture-model',models:[{id:'fixture-model',label:'Fixture model',efforts:['off','low','high','max'],default_effort:'high',image:true,context:200000,price:{input:1.4,output:4.4,cached:0.26}},{id:'fixture-model-with-a-much-longer-identifier-than-fits',label:'A model whose label is long enough to need truncating on a phone',efforts:['off'],default_effort:'off',image:true,context:1050000,price:{input:12.5,output:100,cached:1}}]},{id:'anthropic',name:'Anthropic',signed_in:false,subscription:false,default_model:'x',models:[{id:'x',label:'X',efforts:['high'],default_effort:'high',image:false,context:null,price:null}]}]};
      else if(path.includes('/snapshot')) value={head:10,revision:10,busy:false,complete:true,messages:[{role:'user',content:[{type:'text',text:'A real mobile DOM fixture for src/module_1.rs'}]},{role:'assistant',content:[{type:'text',text:'Ready for work.'},{type:'tool_use',id:'s1',name:'show',input:{path:'site/index.html'}}]},{role:'user',content:[{type:'tool_result',tool_use_id:'s1',content:'shown',display:[{path:'site/index.html',kind:'html',title:'Report',size:900,revision:10}]}]}],files,steps:[{revision:1,label:'commit-start'},{revision:10,label:'done'}],provider:'glm',model:'fixture-model',isolation:'required',target:'Settled',settings:{provider:'glm',model:'fixture-model',effort:'max',network:true,toolchain:false,compaction:'model',max_usd:null},self_edit:{prefix:'spindle/',changed:['spindle/store/ui/app.js'],changed_count:1,behind:true,conflicts:[],request:{revision:10,summary:'Install the new chip',at:1},installing:false,host_build:'b605732'}};
      else if(path.endsWith('/preview')) value={token:'t',url:'/preview/t/',expires_in:1800};
      else if(path.includes('/file?')) return {ok:true,status:200,type:'basic',text:async()=>'fn main() {}'};
      else if(path.startsWith('/api/usage')) value={today:{usd:0.1},month:{usd:2},all:{usd:9}};
      else if(path === '/api/skin') value={active:null,staged:null,versions:[]};
      else if(path === '/api/host') value={version:'fixture',build:'test',schema:14,engine:'dext-turn-v1',sessions:1,running:0,code_policy:{models:[],tools:[]},origin:'https://example.invalid'};
      else if(path === '/api/commands') value={id:data.request_id,session:data.session,state:'completed'};
      else if(path === '/api/devices') value={devices:[{id:'d',label:'iPhone · Safari',created:1,seen:2,current:true}]};
      else if(path === '/api/ping') value={boot:'fixture'};
      else if(path === '/api/logins') value={logins:[{provider:'glm',name:'GLM',status:'auth',signed_in:true},{provider:'chatgpt',name:'ChatGPT',status:'missing',signed_in:false}]};
      else throw Error('Unexpected fetch '+path);
      return {ok:true,status:200,text:async()=>JSON.stringify(value)};
    };
    globalThis.fixture={requests};
  `;
  const check = `
    const check=(ok,why)=>{if(!ok) throw Error(why);};
    await init();
    check(document.querySelectorAll('.session').length===1,'startup thread list');
    const overflow=()=>[...document.querySelectorAll('body *')].filter(e=>e.getClientRects().length && e.getBoundingClientRect().right > innerWidth+1 && getComputedStyle(e).position!=='fixed').map(e=>e.id||e.className||e.tagName);
    // The thread list starts a thread from its own composer, with the model chip.
    check(!document.getElementById('dock').hidden && !document.getElementById('model-chip').hidden,'home composer with chip');
    check(document.getElementById('attach').hidden,'no attachments for a thread not started');
    check(!overflow().length,'home overflow '+overflow());
    // The picker: nothing wider than the sheet; effort and Done in reach without scrolling.
    await openPicker();
    // Measured where the sheet comes to rest (it rises 28px as it opens).
    for(const animation of document.getAnimations()) animation.finish();
    const sheet=document.getElementById('model-dialog').getBoundingClientRect();
    const wide=[...document.querySelectorAll('#model-dialog *')].filter(e=>e.getClientRects().length && e.getBoundingClientRect().right>sheet.right+1).map(e=>e.className||e.tagName);
    check(!wide.length,'picker overflow '+wide);
    const done=document.getElementById('model-apply').getBoundingClientRect();
    check(done.bottom<=innerHeight+1,'picker action in reach '+JSON.stringify({inner:innerHeight,done:[done.top,done.bottom],sheet:[sheet.top,sheet.bottom],list:document.getElementById('model-list').getBoundingClientRect().height}));
    check(document.getElementById('effort-choice').children.length===5,'efforts of the model: '+document.getElementById('effort-choice').children.length);
    check(sheet.bottom>=innerHeight-1,'the picker is a bottom sheet '+JSON.stringify(sheet));
    document.getElementById('model-dialog').close();
    await openThread('fixture');
    check(!document.getElementById('dock').hidden,'thread dock');
    check(document.getElementById('settings').hidden && !document.getElementById('title-button').hidden,'thread header: the title is the menu');
    check(document.querySelector('.display-card iframe') && document.querySelector('.change-card'),'display card and change card');
    check(document.querySelector('.message .path-link'),'paths in prose link');
    check(!overflow().length,'thread overflow '+overflow());
    openThreadMenu();
    check(document.getElementById('thread-dialog').open,'thread menu');
    check(document.activeElement?.id!=='rename','opening the menu does not raise the keyboard');
    check(!overflow().length,'menu overflow '+overflow());
    document.getElementById('thread-dialog').close();
    await openHost();
    check(document.getElementById('settings-dialog').open,'host sheet opened');
    for(const d of document.querySelectorAll('#settings-dialog details')) d.open=true;
    check(document.documentElement.scrollWidth<=innerWidth,'page overflow');
    check(!overflow().length,'host overflow '+overflow());
    document.getElementById('settings-dialog').close();
    // Files is the whole sheet; a file's viewer fills the screen; neither is held to a fixed split.
    await openFiles();
    const list=document.getElementById('file-list').getBoundingClientRect();
    check(list.height>innerHeight*0.6,'file list held short: '+list.height);
    document.querySelector('#file-list button').click(); await new Promise(r=>setTimeout(r,200));
    check(document.getElementById('viewer-dialog').open && !document.getElementById('viewer-back').hidden,'viewer over files');
    const frame=document.getElementById('file-html').getBoundingClientRect();
    check(frame.height>=innerHeight*0.7 && frame.bottom<=innerHeight+1,'viewer preview fills the screen: '+frame.height+' of '+innerHeight);
    // The toolbar keeps a buffer under the header's hairline, and the same below its buttons.
    const head=document.querySelector('.viewer-head'), tool=document.querySelector('#file-view-head button');
    const gapAbove=tool.getBoundingClientRect().top-(head.getBoundingClientRect().bottom+parseFloat(getComputedStyle(head).borderBottomWidth));
    const gapBelow=document.getElementById('viewer-body').getBoundingClientRect().top-tool.getBoundingClientRect().bottom;
    check(gapAbove>=8 && gapBelow>=8,'viewer toolbar spacing '+gapAbove+'/'+gapBelow);
    check(!document.getElementById('file-view-network').hidden,'explicit GitHub access control');
    document.getElementById('file-view-network').click();
    check(document.getElementById('confirm-dialog').open,'GitHub requires confirmation');
    check(!fixture.requests.some(r=>r.data?.network==='github'),'no GitHub grant before confirmation');
    await document.getElementById('confirm-form').onsubmit({preventDefault(){}});
    check(fixture.requests.some(r=>r.data?.network==='github'),'confirmed preview grant');
    check(!overflow().length,'viewer toolbar overflow '+overflow());
    const networkButton=document.getElementById('file-view-network').getBoundingClientRect();
    check(networkButton.height>=44,'GitHub access touch target');
    document.getElementById('viewer-back').click();
    check(!document.getElementById('viewer-dialog').open && document.getElementById('files-dialog').open,'back to the list');
    document.getElementById('files-dialog').close();
    openHistory(); await backToLive();
    check(!document.getElementById('history-dialog').open,'history closes');
    render({...snapshot,busy:true,complete:false});
    check(!document.getElementById('send').disabled,'typed Stop reachable');
    document.getElementById('prompt').value='retry';
    await document.getElementById('composer').onsubmit({preventDefault(){}});
    check(document.getElementById('confirm-dialog').open,'typed Retry confirmation');
    check(!fixture.requests.some(r=>r.path==='/api/commands'),'confirmation prevents dispatch');
    document.getElementById('confirm-dialog').close();
    const reach=e=>e.getBoundingClientRect().height+(e.closest('#bar') ? -parseFloat(getComputedStyle(e,'::before').top)||0 : 0);
    const small=[...document.querySelectorAll('button')].filter(e=>e.getClientRects().length && !e.hidden).filter(e=>reach(e)<44).map(e=>e.id||e.textContent);
    check(!small.length,'small touch targets '+small);
    const phase=document.getElementById('phase').getBoundingClientRect();
    check(reach(document.getElementById('phase'))>=44 && phase.width>=88,'status row target '+phase.height+'x'+phase.width);
    check(document.getElementById('bar').getBoundingClientRect().height<=30,'compact status row');
    check(document.documentElement.scrollWidth<=innerWidth,'thread overflow');
    // A notice raised while a dialog is open shows above it and can be tapped, not inert behind it.
    await openHost(); toast('Visible over settings');
    const t=document.getElementById('toast').getBoundingClientRect(), top=document.elementFromPoint(t.left+t.width/2,t.top+t.height/2);
    check(top && top.closest('#toast'),'toast hidden behind the open dialog: '+(top?.id||top?.tagName)+' '+JSON.stringify(t)+' '+document.getElementById('toast').matches(':popover-open'));
    hideToast(); document.getElementById('settings-dialog').close();
    // Code and bash blocks all wrap; an opened tool card survives a re-render.
    const fence=String.fromCharCode(96).repeat(3), nl=String.fromCharCode(10);
    const longPath='/workspace/spindle/src/some/deeply/nested/module/path/that/keeps/going/on/and/on.rs:123:45';
    const call={type:'tool_use',id:'c1',name:'edit_file',input:{path:'a.rs',old_string:'let a = 1;'+nl+'let b = 2;',new_string:'let a = 1;'+nl+'let b = 3;'}};
    const bash={type:'tool_use',id:'c2',name:'bash',input:{command:'cd /workspace && cargo test --release --locked --workspace --all-features --lib --bins 2>&1 | tee /tmp/spindle-test-output.log | tail -n 40'}};
    const code={type:'tool_use',id:'c3',name:'run_code',input:{language:'typescript',code:'const snapshot = await fetchSnapshot(sessionIdentifier, revisionNumber, {timeout: 30000, retries: 2});'}};
    const fenced={...snapshot,busy:false,complete:true,messages:[{role:'assistant',content:[{type:'text',text:'Change:'+nl+nl+fence+'rust'+nl+'let value = compute_something_long(argument_one, argument_two, argument_three);'.repeat(2)+nl+fence},call,bash,code]},{role:'user',content:[{type:'tool_result',tool_use_id:'c1',content:'ok'},{type:'tool_result',tool_use_id:'c2',content:'warning: '+longPath+nl+'test result: ok'},{type:'tool_result',tool_use_id:'c3',content:longPath}]}]};
    render(fenced);
    check(document.querySelector('.message pre.code'),'code fence block');
    check(document.documentElement.scrollWidth<=innerWidth,'code block overflow');
    check(document.querySelectorAll('.tool-card pre.diff .diff-add').length===1 && document.querySelectorAll('.tool-card pre.diff .diff-del').length===1,'edit shown as a diff');
    for(const card of document.querySelectorAll('.tool-card')) card.open=true;
    const pres=[...document.querySelectorAll('.message pre.code, .tool-card pre')];
    const sideways=pres.filter(p=>p.getClientRects().length && p.scrollWidth>p.clientWidth+1).map(p=>p.className||p.tagName);
    check(pres.length>=5 && !sideways.length,'code and bash blocks wrap instead of scrolling sideways: '+sideways);
    check(pres.every(p=>getComputedStyle(p).whiteSpace==='pre-wrap'),'code and bash blocks share one wrapping rule');
    document.querySelector('.tool-card').open=true; render(fenced);
    check(document.querySelector('.tool-card').open,'opened card collapsed by a re-render');
    // The jump pill rides above the dock, clear of the status row's upward tap zone.
    document.getElementById('jump').hidden=false;
    check(document.getElementById('jump').getBoundingClientRect().bottom<=document.getElementById('bar').getBoundingClientRect().top-22,'jump overlaps the status row');
    document.body.dataset.review='passed';
  `;
  // Function replacers: a source containing "$`" or "$'" must be inlined literally.
  html = html.replace('<link rel="stylesheet" href="/style.css">',() => `<style>${css}</style>`)
    .replace('<script src="/theme.js"></script>',() => `<script>${fixture}${theme}</script>`)
    .replace('<script type="module" src="/app.js"></script>',() => `<script type="module">${clean(terminal)}\n${clean(commands)}\n${clean(text)}\n${clean(app).replace(/init\(\)\.catch\(error => \{if\(!\$\('pair-dialog'\)\.open\) fail\(error\);\}\);\s*$/,'')}\ntry {${check}} catch(error) {document.body.dataset.review='FAILED: '+error.message;}</script>`);
  const profile = await mkdtemp(join(tmpdir(),'spindle-ui-browser-'));
  try {
    // A file, not a data: URL: the inlined page outgrew the 128 KiB argument limit.
    const page = join(profile,'fixture.html');
    await writeFile(page, html);
    // --headless: a full Chrome needs it (chrome-headless-shell accepts and ignores it).
    const result = spawnSync(browser,['--headless','--no-sandbox','--disable-gpu','--no-proxy-server',`--user-data-dir=${profile}`,'--window-size=393,852','--virtual-time-budget=2000','--dump-dom',`file://${page}`],{encoding:'utf8',timeout:20000,maxBuffer:2*1024*1024});
    assert.equal(result.error,undefined,result.error?.message);
    assert.equal(result.status,0,result.stderr);
    assert.match(result.stdout, /data-review="passed"/,result.stdout.match(/data-review="[^"]+"/)?.[0] || result.stderr);
  } finally {await rm(profile,{recursive:true,force:true});}
});
