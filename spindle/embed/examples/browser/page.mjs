// The page: a form and a log, talking to the worker that runs the session.
const $ = (id) => document.getElementById(id);
const log = (text, kind) => {
  const line = document.createElement('span');
  if (kind) line.className = kind;
  line.textContent = text;
  $('log').append(line);
  $('log').scrollTop = $('log').scrollHeight;
};

const worker = new Worker('./worker.mjs', { type: 'module' });
const waiting = new Map();
let next = 0;
worker.onerror = (event) => log(`\nworker failed: ${event.message}\n`, 'error');
worker.onmessage = ({ data }) => {
  if (data.ready) return void providers();
  if (data.event) return show(data.event);
  waiting.get(data.id)?.(data);
  waiting.delete(data.id);
};
const ask = (action, args = {}, transfer = []) => new Promise((resolve) => {
  const id = ++next;
  waiting.set(id, resolve);
  worker.postMessage({ id, action, ...args }, transfer);
});

/** The catalog's providers in the list, its active one chosen. */
async function providers() {
  const reply = await ask('providers');
  if (!reply.ok) return log(`error: ${reply.error}\n`, 'error');
  for (const provider of reply.result.providers) {
    const option = new Option(`${provider.name} (${provider.model})`, provider.id);
    option.selected = provider.id === reply.result.active;
    $('provider').append(option);
  }
  log('ready\n', 'note');
}

function show(event) {
  if (event.type === 'model_delta') log(event.text);
  else if (event.type === 'tool_call') log(`\n$ ${event.name} ${JSON.stringify(event.input)}\n`, 'tool');
  else if (event.type === 'tool_result') log(`${String(event.content).slice(0, 400)}\n`, 'tool');
  else if (event.type === 'stopped') log(`\n[${event.reason}] revision ${event.revision}\n`, 'note');
  else if (event.type === 'unisolated') log(`\n(${event.reason})\n`, 'note');
}
const fields = () => ({ dir: $('dir').value, provider: $('provider').value, model: $('model').value });
const report = (reply, ok) => log(`${reply.ok ? ok(reply.result) : `error: ${reply.error}`}\n`, reply.ok ? 'note' : 'error');

$('send').onclick = async () => {
  log(`\n> ${$('text').value}\n`, 'note');
  report(await ask('prompt', { ...fields(), text: $('text').value }), (r) => `head ${r.head}; files: ${r.files.join(', ') || 'none'}`);
};
$('stop').onclick = async () => report(await ask('stop', fields()), () => 'stopping');
$('status').onclick = async () => report(await ask('status', fields()), (r) => `${r.provider} / ${r.model}: ${r.state}; head ${r.head}`);
$('export').onclick = async () => {
  const reply = await ask('export', fields());
  if (!reply.ok) return report(reply);
  const name = `${fields().dir.split('/').filter(Boolean).join('-') || 'session'}.spindle`;
  const link = document.createElement('a');
  link.href = URL.createObjectURL(new Blob([reply.result.bytes]));
  link.download = name;
  link.click();
  log(`moved the session into ${name} (${reply.result.bytes.length} bytes); this copy is sealed\n`, 'note');
};
$('import').onchange = async () => {
  const file = $('import').files[0];
  if (!file) return;
  const bytes = await file.arrayBuffer();
  report(await ask('import', { dir: fields().dir, bytes }, [bytes]), (r) => `imported ${file.name}: head ${r.head}`);
  $('import').value = '';
};
