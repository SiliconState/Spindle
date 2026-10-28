// Only shows run notifications (phones refuse page-made ones) and opens the thread on a
// tap. There is no fetch handler: nothing is cached or intercepted, no push server.
// A new version takes over at once, so a fixed handler never waits for every tab to close.
self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', event => event.waitUntil(self.clients.claim()));
self.addEventListener('notificationclick', event => {
  event.notification.close();
  const url = new URL(event.notification.data?.url || '/', self.location.origin).href;
  event.waitUntil(self.clients.matchAll({type: 'window', includeUncontrolled: true}).then(windows => {
    const open = windows.find(client => new URL(client.url).origin === self.location.origin);
    if (!open) return self.clients.openWindow(url);
    // The open tab may show another thread (or the list): it switches to this one.
    open.postMessage({open: url});
    return open.focus();
  }));
});
