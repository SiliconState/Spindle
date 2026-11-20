import type { EmbeddedClient, SpindleEvent } from '../../clients/js/embedded.mjs';

declare const client: EmbeddedClient;
const resting: Promise<{ session: string; rested: boolean }> = client.restSession('/session');
void resting;
client.status('/session');
client.status('/session', {});
client.status('/session', { history: {} });
client.status('/session', { history: { from: 4 } });
// @ts-expect-error history.from is a message index, not a string
client.status('/session', { history: { from: '4' } });
// @ts-expect-error resting takes a session path
client.restSession(4);
client.on('event', (event: SpindleEvent) => {
  if (event.type === 'compacted') {
    const cleared: boolean = event.cleared;
    const before: number = event.before;
    void [cleared, before];
  }
});
