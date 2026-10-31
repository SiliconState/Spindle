// A page a thread made, shown by view.html and the panel (page.mjs). This document is a
// sandboxed extension page (its own origin, no extension APIs), and Spindle's service
// worker serves it under a Connection-Allowlist (background.js), so where Chrome holds
// it to one the page's scripts run without reaching Spindle, its keys or the network.
// It tells its parent first whether it is so held (`contained`), then becomes the first
// page its parent sends it (one that may run only contained, only if it is), and tells
// the parent once that is written in (the parent times it).

/** Whether Chrome holds this document to a Connection-Allowlist: a connection made
 * under one is given none of its ICE servers, as under nothing else. Asking sends
 * nothing; without WebRTC there is no telling, so it is not. */
const contained = (() => {
  try {
    const probe = new RTCPeerConnection({ iceServers: [{ urls: 'stun:127.0.0.1:9' }] });
    const held = probe.getConfiguration().iceServers.length === 0;
    probe.close();
    return held;
  } catch {
    return false;
  }
})();

let shown = false;
addEventListener('message', ({ data, source }) => {
  if (shown || source !== parent || typeof data?.html !== 'string' || (data.contained && !contained)) return;
  shown = true;
  document.open();
  document.write(data.html);
  document.close();
  parent.postMessage({ spindleShown: true }, '*');
});
parent.postMessage({ spindleReady: true, contained }, '*');
