// The owner the session crash test kills: sends the first request of a turn, says
// so once it is on its way, and waits (its response never comes) to be killed.
import { nodeClient } from '../../clients/js/embedded-node.mjs';
import { KEY, catalog, hold, scriptedFetch } from './scripted.mjs';

const [glue, dir] = process.argv.slice(2);
const fetch = scriptedFetch([hold()]);
fetch.holding = () => process.stdout.write('sent\n');
const client = nodeClient({ glue, catalog: catalog(), credentials: async () => ({ authorization: `Bearer ${KEY}` }), fetch });
await client.create(dir, 'fake');
client.prompt(dir, 'hello').catch(() => {});
setInterval(() => {}, 1 << 30);
