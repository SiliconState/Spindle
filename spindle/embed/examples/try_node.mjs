// Try the embedded package in Node with a real model: one session directory, one
// prompt, what happens printed as it streams. Optionally moves sessions by bundle.
//
//   SPINDLE_KEY=... node try_node.mjs --provider deepseek --dir ./try "PROMPT"
//
// Options (all optional except what your run needs):
//   --provider ID   a provider from the catalog (default: the catalog's active one)
//   --model M       a model of that provider (default: its default model)
//   --catalog FILE  providers in Dext's format (default: ~/.dext/providers.json when
//                   present, otherwise Dext's built-in providers)
//   --dir DIR       the session directory (default ./spindle-try); created if missing
//   --import FILE   first continue the session bundle FILE in DIR (DIR must be new)
//   --export FILE   after the prompt, move the session into the bundle FILE
//   --glue FILE     the package's Node glue (default: the kit's node/spindle_embed.js)
//
// The key comes from SPINDLE_KEY and stays in this process: the app adds it to each
// request; Spindle never sees or stores it.
import fs from 'node:fs';
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = fileURLToPath(new URL('.', import.meta.url));
// The clients sit beside this file in a kit, and in clients/js in the repository.
const clients = [join(here, '../clients'), join(here, '../../clients/js')].find((dir) =>
  fs.existsSync(join(dir, 'embedded-node.mjs')),
);
const { nodeClient, SpindleError } = await import(join(clients, 'embedded-node.mjs'));

const options = { dir: './spindle-try', words: [] };
const args = process.argv.slice(2);
while (args.length) {
  const arg = args.shift();
  if (arg.startsWith('--')) options[arg.slice(2)] = args.shift();
  else options.words.push(arg);
}
const prompt = options.words.join(' ');
const dir = resolve(options.dir);
const glue = options.glue ? resolve(options.glue) : join(here, '../node/spindle_embed.js');
if (!fs.existsSync(glue)) throw new Error(`no Node glue at ${glue}; build the kit (embed/kit.sh) or pass --glue`);
const catalogFile = options.catalog ?? join(homedir(), '.dext', 'providers.json');
const catalog = fs.existsSync(catalogFile) ? JSON.parse(fs.readFileSync(catalogFile, 'utf8')) : undefined;
const key = process.env.SPINDLE_KEY;

const client = nodeClient({
  glue,
  catalog,
  // Only the key: the package says where it goes for each provider (from the catalog).
  credentials: async () => {
    if (!key) throw new Error('set SPINDLE_KEY to the provider key, in the same command: SPINDLE_KEY=your-key node try_node.mjs ...');
    return { key };
  },
});
client.on('event', (event) => {
  if (event.type === 'model_delta') process.stdout.write(event.text);
  if (event.type === 'tool_call') console.log(`\n$ ${event.name} ${JSON.stringify(event.input)}`);
  if (event.type === 'tool_result') console.log(String(event.content).slice(0, 400));
  if (event.type === 'unisolated') console.log(`\n(note: ${event.reason})`);
  if (event.type === 'stopped') console.log(`\n[${event.reason}] revision ${event.revision}`);
});

try {
  if (options.import) {
    const status = await client.importSession(dir, fs.readFileSync(options.import));
    console.log(`imported ${options.import} into ${dir}: head ${status.head}`);
  } else {
    try {
      await client.open(dir);
    } catch (error) {
      if (!(error instanceof SpindleError) || error.kind !== 'not_found') throw error;
      const provider = options.provider ?? catalog?.active_provider;
      if (!provider) throw new Error('pass --provider ID');
      await client.create(dir, provider, options.model ? { model: options.model } : {});
      console.log(`created a ${provider} session in ${dir}`);
    }
  }
  if (prompt) {
    const result = await client.prompt(dir, prompt);
    console.log(`head ${result.head}${result.unsent.length ? `; not read in time: ${result.unsent.join(' | ')}` : ''}`);
  }
  const status = await client.status(dir);
  console.log(`${status.provider} / ${status.model}: ${status.state}; head ${status.head}`);
  for (const file of (await client.files(dir)).files) console.log(String(file.size).padStart(9), file.path);
  if (options.export) {
    const bytes = await client.exportSession(dir);
    fs.writeFileSync(options.export, bytes);
    console.log(`moved the session into ${options.export} (${bytes.length} bytes); ${dir} is sealed`);
  }
} catch (error) {
  console.error(`\n${error.message}`);
  process.exitCode = 1;
} finally {
  await client.close();
}
