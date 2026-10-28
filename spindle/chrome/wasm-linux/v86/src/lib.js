// Stand-in for v86's lib.js: the hex formatter the 9p files log with, and how
// filestorage.js loads a file of a share's root (fetch, with v86's callback shape).
export function h(n, len) {
  return `0x${(n ? n.toString(16) : '').toUpperCase().padStart(len || 1, '0')}`;
}

export async function load_file(url, { done, progress } = {}) {
  void progress;
  const response = await fetch(url);
  if (!response.ok) throw new Error(`${url}: HTTP ${response.status}`);
  await done(await response.arrayBuffer());
}
