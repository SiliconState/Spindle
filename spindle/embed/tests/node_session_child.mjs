// The owner the session crash tests kill. By default it sends the first request of a
// turn, says so once it is on its way, and waits (its response never comes) to be
// killed. With `stop`, its turn runs a command in the app's runner, the user stops it,
// and the runner never answers: it says so, and waits to be killed between the stop and
// the stopped command's record.
import { nodeClient } from '../../clients/js/embedded-node.mjs';
import { KEY, call, catalog, hold, scriptedFetch } from './scripted.mjs';

const [glue, dir, mode] = process.argv.slice(2);
const credentials = async () => ({ authorization: `Bearer ${KEY}` });
if (mode === 'stop') {
  let client;
  const runner = {
    run() {
      setTimeout(() => client.stop(dir).then(() => process.stdout.write('stopping\n')), 20);
      return new Promise(() => {});
    },
  };
  client = nodeClient({ glue, catalog: catalog(), credentials, fetch: scriptedFetch([call('bash', { command: 'sleep 100' })]), runner });
  await client.create(dir, 'fake', { runner: 'app' });
  client.prompt(dir, 'run it').catch(() => {});
} else {
  const fetch = scriptedFetch([hold()]);
  fetch.holding = () => process.stdout.write('sent\n');
  const client = nodeClient({ glue, catalog: catalog(), credentials, fetch });
  await client.create(dir, 'fake');
  client.prompt(dir, 'hello').catch(() => {});
}
setInterval(() => {}, 1 << 30);
