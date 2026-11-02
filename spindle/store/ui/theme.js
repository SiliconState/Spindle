// Runs before the stylesheet paints: Paper unless this screen chose Light, Dim or System
// (Light or Dim as the system is), and the textured background unless it chose Plain.
// Dim is the dark theme: a soft charcoal, easier on the eyes than near-black (a screen
// that chose the old Dark gets Dim).
// Settings > Appearance selects through spindleTheme.set and spindleTheme.setTexture.
(() => {
  const media = matchMedia('(prefers-color-scheme: dark)');
  // The browser's bar takes the header's colour (--chrome), so the two read as one.
  const colors = {paper: '#ece6d8', light: '#f0f0f0', dim: '#2c2c2a'};
  const modes = ['paper','light','dim','system'];
  const textures = ['textured','plain'];
  let mode = 'paper';
  let texture = 'textured';
  try {const saved = localStorage.getItem('spindle-theme'); if (modes.includes(saved)) mode = saved; else if (saved === 'dark') mode = 'dim';} catch {}
  try {const saved = localStorage.getItem('spindle-texture'); if (textures.includes(saved)) texture = saved;} catch {}
  function apply() {
    const shown = mode === 'system' ? (media.matches ? 'dim' : 'light') : mode;
    document.documentElement.dataset.theme = shown;
    document.documentElement.dataset.texture = texture;
    document.querySelector('meta[name="theme-color"]').content = colors[shown];
  }
  function set(next) {
    if (!modes.includes(next)) return;
    mode = next;
    try {localStorage.setItem('spindle-theme',mode);} catch {}
    apply();
  }
  function setTexture(next) {
    if (!textures.includes(next)) return;
    texture = next;
    try {localStorage.setItem('spindle-texture',texture);} catch {}
    apply();
  }
  globalThis.spindleTheme = {get: () => mode, set, texture: () => texture, setTexture};
  apply();
  media.addEventListener('change',apply);
})();
