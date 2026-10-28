// Run one prompt in a session directory and print what happens.
//
//   node example.mjs DIR PROVIDER "PROMPT"
//
// Uses `spindle` from PATH (or $SPINDLE_BIN) and your Dext logins.
import { SpindleError, StdioClient } from './spindle.mjs';

const [dir, provider, prompt] = process.argv.slice(2);
const api = new StdioClient();
api.on('event', (event) => {
  if (event.type === 'model_delta') process.stdout.write(event.text);
  if (event.type === 'tool_call') console.log(`\n$ ${event.name} ${JSON.stringify(event.input)}`);
  if (event.type === 'tool_result') console.log(event.content.slice(0, 400));
  if (event.type === 'stopped') console.log(`\n[${event.reason}] revision ${event.revision}`);
});
try {
  await api.open(dir);
} catch (error) {
  if (!(error instanceof SpindleError) || error.kind !== 'not_found') throw error;
  await api.create(dir, provider);
}
const result = await api.prompt(dir, prompt);
console.log('head:', result.head);
for (const file of (await api.files(dir)).files) console.log(String(file.size).padStart(9), file.path);
await api.close();
