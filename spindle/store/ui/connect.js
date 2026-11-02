// The connect page (GET /connect): a paired screen answers a client that asks for a
// browser token of this host. The host renders who asks; this script pairs the screen
// if needed (the same POST /api/pair as the main page), then sends Allow or Deny to
// POST /api/connect and follows the address it answers with (/connect/done...), which
// the asking client watches for. No token passes through this page.
(() => {
  const $ = id => document.getElementById(id);
  const card = $('connect-card');
  const ask = {client: card.dataset.client, challenge: card.dataset.challenge, state: card.dataset.state};
  async function post(path, body) {
    const response = await fetch(path, {method: 'POST', headers: {'Content-Type': 'application/json', 'X-Spindle': '1'}, body: JSON.stringify(body)});
    const data = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(data.error || `The host answered ${response.status}.`);
    return data;
  }
  function asking() { $('connect-pair').hidden = true; $('connect-ask').hidden = false; $('allow').focus(); }
  // The tab may have opened without the cookie (a navigation the browser counts as
  // cross-site): a same-origin request carries it, so ask the host once more.
  if (!$('connect-pair').hidden) fetch('/api/ping').then(response => { if (response.ok) asking(); }).catch(() => {});
  $('pair-form').onsubmit = async event => {
    event.preventDefault();
    try { await post('/api/pair', {key: $('pair-key').value}); $('pair-key').value = ''; asking(); }
    catch (error) { $('pair-error').textContent = error.message; }
  };
  async function answer(allow) {
    const buttons = [$('allow'), $('deny')];
    for (const button of buttons) button.disabled = true;
    try { const {redirect} = await post('/api/connect', {...ask, allow}); location.replace(redirect); }
    catch (error) { $('connect-error').textContent = error.message; for (const button of buttons) button.disabled = false; }
  }
  $('allow').onclick = () => answer(true);
  $('deny').onclick = () => answer(false);
})();
