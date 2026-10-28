// Marks the header link of the section being read (aria-current), as the page scrolls.
// The page works the same without it.
const links = new Map();
for (const link of document.querySelectorAll('.top nav a[href^="#"]')) links.set(link.hash.slice(1), link);
const sections = [...document.querySelectorAll('main section[id]')];

let queued = false;
function mark() {
  queued = false;
  // The section whose top has passed a line a third of the way down the window.
  const line = window.innerHeight / 3;
  let current = '';
  for (const section of sections) if (section.getBoundingClientRect().top <= line) current = section.id;
  for (const [id, link] of links) {
    if (id === current) link.setAttribute('aria-current', 'true');
    else link.removeAttribute('aria-current');
  }
}

if (links.size) {
  addEventListener('scroll', () => {
    if (!queued) {
      queued = true;
      requestAnimationFrame(mark);
    }
  }, { passive: true });
  addEventListener('resize', mark);
  mark();
}
