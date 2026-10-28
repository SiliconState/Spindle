// Spindle's calls for Node scripts, as native run_code JavaScript has them (and as
// synchronous): model, models, tool, tools, batch, limits, http, image. Each goes through
// the `spindle` command to the runner agent, which asks Spindle; Spindle journals it.
//
//   const { model, tool } = require('spindle');   // or, in .mjs and .ts files:
//   import { model, tool } from 'spindle';
//   console.log(model('Say hello in French').text);
'use strict';
const { execFileSync } = require('child_process');
const fs = require('fs');

function ask(method, input) {
  const reply = JSON.parse(execFileSync('spindle', ['call', method, '-'], { input: JSON.stringify(input), encoding: 'utf8', maxBuffer: 64 << 20 }));
  if (!reply.ok) throw new Error((reply.error || 'the call failed') + (reply.may_have_run ? ' [may_have_run]' : ''));
  return reply.data;
}

const model = (prompt, options = {}) => ask('model', { prompt, ...options });
const tool = (name, args = {}) => ask('tool', { name, arguments: args });
const tools = { search: (query = '') => ask('search', { query }), describe: (name) => ask('describe', { name }), call: tool };
const batch = (calls) => ask('batch', {
  calls: calls.map(({ method, input = {} }) => ({ method, input: method === 'tool' ? { name: input.name, arguments: input.arguments || {} } : { ...input } })),
});
const models = (prompts, options = {}) => batch(prompts.map((prompt) => ({ method: 'model', input: { prompt, ...options } })))
  .map((r) => (r.ok ? r.data : { error: r.error, may_have_run: !!r.may_have_run }));
const limits = () => ask('limits', {});
const http = (url, options = {}) => ask('http', { ...options, url });
function image(file) {
  const data = fs.readFileSync(file);
  const kind = data.subarray(0, 8).equals(Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])) ? 'image/png'
    : data[0] === 0xff && data[1] === 0xd8 && data[2] === 0xff ? 'image/jpeg'
    : data.subarray(0, 4).toString('latin1') === 'GIF8' ? 'image/gif'
    : data.subarray(0, 4).toString('latin1') === 'RIFF' && data.subarray(8, 12).toString('latin1') === 'WEBP' ? 'image/webp' : null;
  if (!kind) throw new Error(`image: ${file} is not PNG, JPEG, GIF or WebP`);
  return { media_type: kind, data: data.toString('base64') };
}

module.exports = { model, models, tool, tools, batch, limits, http, image };
