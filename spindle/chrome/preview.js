// A page a thread made, shown by view.html: this document is a sandboxed extension page
// (its own origin, no extension APIs), so the page's scripts run without reaching
// Spindle. It becomes the first page its parent sends it.
let shown = false;
addEventListener('message', ({ data, source }) => {
  if (shown || source !== parent || typeof data?.html !== 'string') return;
  shown = true;
  document.open();
  document.write(data.html);
  document.close();
});
