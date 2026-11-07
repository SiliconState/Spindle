// A page a thread made, shown by view.html and the panel (page.mjs). This document is a
// sandboxed extension page (its own origin, no extension APIs), and Spindle's service
// worker serves it under a Connection-Allowlist (background.js), so where Chrome holds
// it to one the page's scripts run without reaching Spindle, its keys or the network.
// It tells its parent first whether it is so held (`contained`), then becomes the first
// page its parent sends it (one that may run only contained, only if it is), and tells
// the parent once that is written in (the parent times it). It prints when its parent
// asks (the file tab's Print), and on Ctrl+P (⌘P) once its parent says the key is its.

// The sandbox lets this frame print (allow-modals: Chrome counts printing as a modal),
// which would let the page show alert, confirm, prompt and a "Leave site?" question too,
// and print unasked (as it loads, say, in the panel). Those stay as they were without it:
// alert shows nothing, confirm says no, prompt answers nothing, and the page's own print()
// prints only while a click or key of the user's is fresh (its own Print button). The
// page is written into this same window, so these hold for it (writing it keeps what is
// set on the window and its prototypes); a frame it makes has its own window and may.
const printing = globalThis.print;
Object.assign(globalThis, {
  alert: () => {},
  confirm: () => false,
  prompt: () => null,
  print: () => {
    if (navigator.userActivation?.isActive) printing();
  },
});
// No "Leave site?": a beforeunload listener can neither cancel the event nor give it
// words, and no beforeunload handler is kept (one written as the body's attribute is
// taken off once the page is in). Listeners here run in the order they were added, so a
// listener of Spindle's added after the page could not come first.
try {
  const unload = BeforeUnloadEvent.prototype;
  Object.defineProperty(unload, 'preventDefault', { value: () => {}, configurable: true });
  Object.defineProperty(unload, 'returnValue', { get: () => '', set: () => {}, configurable: true });
  for (const holder of [globalThis, HTMLBodyElement.prototype, HTMLFrameSetElement.prototype]) {
    Object.defineProperty(holder, 'onbeforeunload', { get: () => null, set: () => {}, configurable: true });
  }
} catch (error) {
  console.warn('Spindle preview: a page here may ask "Leave site?"', error);
}

/** Whether Ctrl+P (⌘P) prints this page: its parent says so (the file tab does; in the
 * panel the key stays the browser's). */
let keys = false;
/** Ctrl+P (⌘P on a Mac), as Chrome prints on (keyCode: the P key on any layout). */
const MAC = /Mac/i.test(navigator.userAgentData?.platform ?? navigator.platform);
const printKey = (event) => event.keyCode === 80 && !event.altKey && !event.shiftKey
  && (MAC ? event.metaKey && !event.ctrlKey : event.ctrlKey && !event.metaKey);

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
  if (source !== parent) return;
  if (data?.spindleKeys === true) keys = true;
  if (shown || typeof data?.html !== 'string' || (data.contained && !contained)) return;
  shown = true;
  document.open();
  document.write(data.html);
  document.close();
  document.body?.removeAttribute('onbeforeunload');
  // Writing the page took every listener off this window (this one too): from here on
  // these are heard, for a page with no scripts of its own too (Print only now: before,
  // it would print the empty frame).
  addEventListener('message', ({ data, source }) => {
    if (source !== parent) return;
    if (data?.spindlePrint === true) printing();
    if (data?.spindleKeys === true) keys = true;
  });
  addEventListener('keydown', (event) => {
    if (!keys || !printKey(event)) return;
    event.preventDefault();
    printing();
  });
  parent.postMessage({ spindleShown: true }, '*');
});
parent.postMessage({ spindleReady: true, contained }, '*');
