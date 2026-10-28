// Runs before the stylesheet paints: Paper unless this screen chose Dark or System, and
// the textured background unless it chose Plain. Settings > Appearance selects through
// spindleTheme.set and spindleTheme.setTexture.
(() => {
  const media = matchMedia('(prefers-color-scheme: dark)');
  const modes = ['paper','dark','system'];
  const textures = ['textured','plain'];
  let mode = 'paper';
  let texture = 'textured';
  try {const saved = localStorage.getItem('spindle-theme'); if (modes.includes(saved)) mode = saved;} catch {}
  try {const saved = localStorage.getItem('spindle-texture'); if (textures.includes(saved)) texture = saved;} catch {}
  function apply() {
    const dark = mode === 'dark' || (mode === 'system' && media.matches);
    document.documentElement.dataset.theme = dark ? 'dark' : 'paper';
    document.documentElement.dataset.texture = texture;
    document.querySelector('meta[name="theme-color"]').content = dark ? '#111511' : '#f6f2e9';
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
