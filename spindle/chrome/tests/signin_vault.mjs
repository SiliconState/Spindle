// Spindle for Browser: browser sign-in and the key vault, end to end in real Chrome with
// scripted services (no account, no cost). It checks:
//
//   1. a provider with a browser sign-in shows Sign in, not a key field; signing in opens
//      the provider's page in a tab, the redirect to its loopback address is caught and
//      the tab closed (nothing listens there), the code is exchanged with its PKCE
//      verifier, and the row says who is signed in;
//   2. a turn on that provider refreshes a token about to expire first (the refresh
//      token rotating), then sends the new token with the account its own claim names
//      (`chatgpt-account-id`), as the ChatGPT backend reads it;
//   3. a key saved in Settings is sealed in IndexedDB (no record holds a secret as
//      text, nor does any file of the profile); a key a former build kept as text is
//      sealed when the engine starts;
//   4. a passphrase locks the keys: locked, a send asks to unlock and nothing reaches the
//      provider; a wrong passphrase is refused; unlocked, turns run; a restarted engine
//      is locked again; the passphrase changes and comes off, the keys still readable;
//   5. a refresh that fails while the token is still good keeps it in use; a refused one
//      ends the sign-in: nothing is sent, the turn and the row say to sign in again;
//   6. a forgotten passphrase: "Forgot it" deletes every saved credential after asking,
//      and the threads stay.
//
//   SPINDLE_BROWSER=/path/to/chrome node chrome/tests/signin_vault.mjs DIST
import assert from 'node:assert/strict';
import { createHash, randomBytes } from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { launch, open } from './cdp.mjs';

const distArg = process.argv[2];
const browserBinary = process.env.SPINDLE_BROWSER;
if (!distArg || !browserBinary) throw new Error('usage: SPINDLE_BROWSER=... node signin_vault.mjs DIST');

const work = fs.mkdtempSync(join(tmpdir(), 'spindle-signin-'));
let browser = null;
process.on('exit', () => {
  browser?.child.kill('SIGKILL');
  fs.rmSync(work, { recursive: true, force: true });
});
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => process.exit(130));

const b64url = (data) => Buffer.from(data).toString('base64url');
const jwt = (claims) => `${b64url('{"alg":"none"}')}.${b64url(JSON.stringify(claims))}.sig`;
const ACCOUNT = `acct-${randomBytes(4).toString('hex')}`;
const EMAIL = 'tester@example.com';
const apiKey = `sk-vault-${randomBytes(12).toString('hex')}`;
const legacyKey = `sk-legacy-${randomBytes(12).toString('hex')}`;

// A free port where nothing listens: the sign-in's registered loopback redirect.
const callbackPort = await new Promise((done) => {
  const probe = net.createServer().listen(0, '127.0.0.1', () => {
    const { port } = probe.address();
    probe.close(() => done(port));
  });
});
const redirectUri = `http://localhost:${callbackPort}/auth/callback`;

// ---- The scripted services: an OAuth server, a ChatGPT-style backend, a key provider.
const seen = { authorize: [], token: [], responses: [], chat: [] };
const issued = { code: null, challenge: null, refreshes: 0, refreshMode: 'ok', exchanged: null };
const readBody = async (request) => {
  const parts = [];
  for await (const part of request) parts.push(part);
  return Buffer.concat(parts).toString();
};
const server = http.createServer(async (request, response) => {
  const url = new URL(request.url, 'http://test');
  if (request.method === 'GET' && url.pathname === '/oauth/authorize') {
    seen.authorize.push(Object.fromEntries(url.searchParams));
    issued.code = `code-${randomBytes(6).toString('hex')}`;
    issued.challenge = url.searchParams.get('code_challenge');
    const back = new URL(url.searchParams.get('redirect_uri'));
    back.searchParams.set('code', issued.code);
    back.searchParams.set('state', url.searchParams.get('state'));
    response.writeHead(302, { location: back.href }).end();
    return;
  }
  if (request.method === 'POST' && url.pathname === '/oauth/token') {
    const form = Object.fromEntries(new URLSearchParams(await readBody(request)));
    seen.token.push(form);
    const tokens = (n, expiresIn) => ({
      access_token: jwt({ exp: Math.floor(Date.now() / 1000) + expiresIn, 'https://api.openai.com/auth': { chatgpt_account_id: ACCOUNT } }),
      refresh_token: `refresh-${n}`, id_token: jwt({ email: EMAIL }), expires_in: expiresIn,
    });
    let answer = null;
    if (form.grant_type === 'authorization_code') {
      const verified = createHash('sha256').update(form.code_verifier ?? '').digest('base64url') === issued.challenge;
      // The first token is about to expire: the first turn refreshes it.
      if (form.code === issued.code && verified && form.client_id === 'app_test' && form.redirect_uri === redirectUri) {
        answer = tokens(issued.refreshes + 1, 60);
        issued.exchanged = answer.access_token;
      }
    } else if (form.grant_type === 'refresh_token' && issued.refreshMode !== 'ok') {
      // A token endpoint that is down, or one that refuses the refresh token.
      response.writeHead(issued.refreshMode === 'down' ? 503 : 400, { 'content-type': 'application/json' });
      response.end(JSON.stringify(issued.refreshMode === 'down' ? { error: 'temporarily_unavailable' } : { error: 'invalid_grant' }));
      return;
    } else if (form.grant_type === 'refresh_token' && form.refresh_token === `refresh-${issued.refreshes + 1}`) {
      issued.refreshes += 1;
      answer = tokens(issued.refreshes + 1, 3600);
    }
    response.writeHead(answer ? 200 : 400, { 'content-type': 'application/json' });
    response.end(JSON.stringify(answer ?? { error: 'invalid_grant', error_description: 'not this test’s grant' }));
    return;
  }
  if (request.method === 'POST' && url.pathname === '/codex/responses') {
    seen.responses.push({ headers: request.headers, body: await readBody(request) });
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    response.end(`data: ${JSON.stringify({ type: 'response.output_text.delta', delta: 'Signed in and answering.' })}\n\n`
      + `data: ${JSON.stringify({ type: 'response.completed', response: { status: 'completed', output: [], usage: { input_tokens: 12, output_tokens: 4 } } })}\n\n`);
    return;
  }
  if (request.method === 'POST' && url.pathname === '/v1/chat/completions') {
    seen.chat.push({ authorization: request.headers.authorization, body: await readBody(request) });
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    const chunks = [
      { id: 'c', object: 'chat.completion.chunk', choices: [{ index: 0, delta: { content: 'Key works.' }, finish_reason: null }] },
      { id: 'c', object: 'chat.completion.chunk', choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] },
    ];
    response.end(`${chunks.map((chunk) => `data: ${JSON.stringify(chunk)}\n\n`).join('')}data: [DONE]\n\n`);
    return;
  }
  response.writeHead(404).end();
});
const port = await new Promise((done) => server.listen(0, '127.0.0.1', () => done(server.address().port)));
const origin = `http://127.0.0.1:${port}`;

const catalogFile = join(work, 'providers.json');
fs.writeFileSync(catalogFile, JSON.stringify({
  version: 4, active_provider: 'subscription', providers: [
    {
      id: 'subscription', display_name: 'Subscription', api_provider: 'chatgpt', request_contract: 'chatgpt-responses',
      base_url: `${origin}/codex`, default_model: 'gpt-test', models: ['gpt-test'], requires_api_key: true,
      oauth_flow: { authorize_url: `${origin}/oauth/authorize`, token_url: `${origin}/oauth/token`, client_id: 'app_test',
        scope: 'openid profile email offline_access', redirect_uri: redirectUri },
      model_specs: { 'gpt-test': { context_window: 200000, max_output_tokens: 4096 } },
    },
    {
      id: 'scripted', display_name: 'Scripted', api_provider: 'openai', request_contract: 'openai-chat-completions',
      base_url: `${origin}/v1`, default_model: 'scripted-model', models: ['scripted-model'], requires_api_key: true,
      model_specs: { 'scripted-model': { context_window: 200000, max_output_tokens: 4096 } },
    },
    {
      id: 'legacy', display_name: 'Legacy', api_provider: 'openai', request_contract: 'openai-chat-completions',
      base_url: `${origin}/v1`, default_model: 'scripted-model', models: ['scripted-model'], requires_api_key: true,
    },
  ],
}));

// The test's copy of the extension: what the sign-in asks Chrome for, granted up front
// (headless Chrome cannot be clicked through a permission prompt).
const dist = join(work, 'extension');
fs.cpSync(resolve(distArg), dist, { recursive: true });
const manifest = JSON.parse(fs.readFileSync(join(dist, 'manifest.json'), 'utf8'));
fs.writeFileSync(join(dist, 'manifest.json'), JSON.stringify({ ...manifest, host_permissions: [`${origin}/*`, `http://localhost:${callbackPort}/*`] }, null, 2));

const results = [];
function check(name, run) {
  try {
    run();
    results.push([name, true]);
    console.log(`ok   ${name}`);
  } catch (error) {
    results.push([name, false]);
    console.log(`FAIL ${name}\n     ${error.message.split('\n').join('\n     ')}`);
  }
}

const profile = join(work, 'profile');
browser = await launch(browserBinary, { profile, downloads: join(work, 'downloads') });
const extensionId = (await browser.send('Extensions.loadUnpacked', { path: dist })).id;
const panel = await open(browser, `chrome-extension://${extensionId}/panel.html`, { newWindow: true });
await panel.size(420, 860);
await panel.until('globalThis.spindlePanel?.engine', { timeout: 60_000, what: 'the engine' });

const vaultState = () => panel.eval('spindlePanel.engine.vault');
/** Restart engine (Settings, open), and the new engine's first snapshot. */
async function restartEngine(until) {
  await panel.eval(`globalThis.beforeRestart = spindlePanel.engine; document.querySelector('[data-fold="about"]').open = true`);
  await panel.click('#restart-engine');
  await panel.until(`spindlePanel.engine && spindlePanel.engine !== globalThis.beforeRestart && (${until})`, { timeout: 60_000, what: 'a new engine' });
}
/** The raw records of the extension's 'keys' store, as stored. */
const rawKeys = () => panel.eval(`new Promise((done) => {
  const request = indexedDB.open('spindle');
  request.onsuccess = () => {
    const tx = request.result.transaction('keys');
    const all = tx.objectStore('keys').getAll();
    const names = tx.objectStore('keys').getAllKeys();
    tx.oncomplete = () => done(names.result.map((name, i) => {
      const record = all.result[i];
      return { name, text: typeof record === 'string' ? record : null, kind: record?.kind ?? null, sealed: record?.data instanceof ArrayBuffer && record?.iv?.length === 12 };
    }));
  };
})`);
const unfoldProviders = () => panel.eval(`document.querySelector('[data-fold="providers"]').open = true`);
const row = (id) => `.provider[data-provider="${id}"]`;
async function choose(provider) {
  // From the thread list: the chip there sets new threads' model.
  await panel.click('#home');
  await panel.click('#model-chip');
  await panel.click('#model-provider .picker-field');
  await panel.click(`#model-provider [role="option"][data-value="${provider}"]`);
  await panel.click('#model-apply');
}
async function newThreadSays(prompt) {
  await panel.click('#home');
  const before = await panel.eval('spindlePanel.current');
  await panel.type('#prompt', prompt);
  await panel.click('#send');
  const id = await panel.until(`spindlePanel.current !== ${JSON.stringify(before)} && spindlePanel.current`, { what: 'the new thread' });
  const state = await panel.until(`(() => { const s = spindlePanel.threads.get(${JSON.stringify(id)})?.state; return s && s !== 'running' ? s : null; })()`, { timeout: 60_000, what: 'the turn' });
  const answer = await panel.until(`(() => { const t = [...document.querySelectorAll('#history .message.assistant')].at(-1)?.innerText ?? ''; return t.trim() || null; })()`, { timeout: 15_000, what: 'an answer' }).catch(() => '');
  return { id, state, answer };
}

try {
  // A key a former build kept as text, there before this build's engine looked.
  await panel.eval(`new Promise((done) => { const r = indexedDB.open('spindle'); r.onsuccess = () => { const tx = r.result.transaction('keys', 'readwrite'); tx.objectStore('keys').put(${JSON.stringify(legacyKey)}, 'legacy'); tx.oncomplete = done; }; })`);

  // Providers from the catalog file.
  await panel.click('#settings');
  await panel.files('#catalog-file', [catalogFile]);
  await panel.until(`spindlePanel.engine.catalogSaved && spindlePanel.engine.catalog.providers.some((p) => p.id === 'subscription')`, { what: 'the catalog' });
  // Importing a catalog restarts the client, not the engine; the engine seals text keys
  // at its own start: restart it.
  await restartEngine(`spindlePanel.engine.vault.kinds.legacy === 'key'`);
  const afterStart = await rawKeys();
  check('a key a former build kept as text is sealed when the engine starts', () => {
    const legacy = afterStart.find((record) => record.name === 'legacy');
    assert.ok(legacy?.sealed && legacy.text === null, JSON.stringify(afterStart));
  });

  // 1. Sign in through the browser.
  if (!(await panel.eval(`document.querySelector('#settings-dialog').open`))) await panel.click('#settings');
  await unfoldProviders();
  const buttons = await panel.eval(`[...document.querySelectorAll('${row('subscription')} button')].map((b) => b.textContent)`);
  const hasKeyField = await panel.eval(`Boolean(document.querySelector('${row('subscription')} input'))`);
  check('a browser sign-in provider offers Sign in and no key field', () => {
    assert.deepEqual(buttons, ['Sign in']);
    assert.equal(hasKeyField, false);
  });
  const tabsBefore = (await browser.send('Target.getTargets')).targetInfos.filter((t) => t.type === 'page').length;
  await panel.click(`${row('subscription')} button`);
  await panel.until(`spindlePanel.engine.vault.kinds.subscription === 'signin'`, { timeout: 60_000, what: 'the sign-in' });
  await panel.until(`document.querySelector('${row('subscription')} .provider-state')?.textContent === 'signed in' && document.querySelector('${row('subscription')} .provider-account')?.textContent === '${EMAIL}' || null`, { what: 'the row to say who' });
  // At a side panel's width nothing in Settings runs off its side, a long account included.
  const overflow = () => panel.eval(`(() => { const d = document.querySelector('#settings-dialog'); return d.scrollWidth - d.clientWidth; })()`);
  await panel.size(320, 860);
  const signedInOverflow = await overflow();
  const tabsAfter = (await browser.send('Target.getTargets')).targetInfos.filter((t) => t.type === 'page');
  check('signing in runs the provider page in a tab, catches the redirect and closes the tab', () => {
    const asked = seen.authorize[0];
    assert.equal(asked.client_id, 'app_test');
    assert.equal(asked.redirect_uri, redirectUri);
    assert.equal(asked.code_challenge_method, 'S256');
    assert.equal(asked.codex_cli_simplified_flow, 'true');
    assert.equal(seen.token[0].grant_type, 'authorization_code');
    assert.equal(tabsAfter.length, tabsBefore, `tabs left: ${tabsAfter.map((t) => t.url).join(', ')}`);
    assert.ok(!tabsAfter.some((t) => t.url.includes(`:${callbackPort}/`)), 'the callback tab is closed');
  });
  check('Settings fits a side panel: a signed-in row with a long account does not widen it', () => {
    assert.ok(signedInOverflow <= 0, `the dialog scrolls ${signedInOverflow}px sideways`);
  });
  await panel.eval(`document.querySelector('#settings-dialog').close()`);

  // 2. A turn on the signed-in provider.
  await choose('subscription');
  const signedTurn = await newThreadSays('Say hello.');
  check('a turn refreshes a token about to expire, then sends it with its account', () => {
    assert.equal(signedTurn.state, 'idle', JSON.stringify(signedTurn));
    assert.ok(signedTurn.answer.includes('Signed in and answering.'), signedTurn.answer);
    assert.equal(issued.refreshes, 1, 'one refresh');
    const sent = seen.responses.at(-1).headers;
    const claims = JSON.parse(Buffer.from(sent.authorization.split(' ')[1].split('.')[1], 'base64url'));
    assert.ok(claims.exp * 1000 - Date.now() > 30 * 60_000, 'the refreshed token went');
    assert.equal(sent['chatgpt-account-id'], ACCOUNT);
    assert.equal(sent['openai-beta'], 'responses=experimental');
  });

  // 3. A key saved through Settings is sealed at rest.
  await panel.click('#settings');
  await unfoldProviders();
  await panel.click(`${row('scripted')} .provider-head button`);
  await panel.type(`${row('scripted')} input`, apiKey);
  await panel.click(`${row('scripted')} button.primary`);
  await panel.until(`document.querySelector('${row('scripted')} .provider-state')?.textContent === 'key saved' || null`, { what: 'the key' });
  const records = await rawKeys();
  check('saved keys and sign-ins are sealed, never kept as text', () => {
    assert.deepEqual(records.map((record) => record.name).sort(), ['legacy', 'scripted', 'subscription']);
    for (const record of records) assert.ok(record.sealed && record.text === null, JSON.stringify(record));
  });
  await panel.eval(`document.querySelector('#settings-dialog').close()`);
  await choose('scripted');
  const keyTurn = await newThreadSays('Check the key.');
  check('a sealed key still reaches its provider', () => {
    assert.ok(keyTurn.answer.includes('Key works.'), keyTurn.answer);
    assert.equal(seen.chat.at(-1).authorization, `Bearer ${apiKey}`);
  });

  // 4. A passphrase.
  await panel.click('#settings');
  await unfoldProviders();
  await panel.type('#vault input[aria-label="New passphrase"]', 'correct horse battery');
  await panel.type('#vault input[aria-label="Same passphrase again"]', 'correct horse battery');
  await panel.click('#vault [data-vault="set"]');
  await panel.until(`spindlePanel.engine.vault.mode === 'passphrase' || null`, { timeout: 60_000, what: 'the passphrase' });
  const lockedRecords = await rawKeys();
  await panel.click('#vault [data-vault="lock"]');
  await panel.until(`spindlePanel.engine.vault.locked || null`, { what: 'the lock' });
  await panel.eval(`document.querySelector('#settings-dialog').close()`);
  const chatBefore = seen.chat.length;
  await panel.click('#home');
  await panel.type('#prompt', 'Locked?');
  await panel.click('#send');
  const lockedNote = await panel.until(`document.querySelector('#settings-dialog').open && document.querySelector('#settings-note').textContent || null`, { what: 'the unlock ask' });
  check('locked keys ask to be unlocked, and nothing is sent', () => {
    assert.match(lockedNote, /unlock/i);
    assert.equal(seen.chat.length, chatBefore);
    for (const record of lockedRecords) assert.ok(record.sealed, JSON.stringify(record));
  });
  await unfoldProviders();
  await panel.type('#vault input[aria-label="Passphrase"]', 'not the passphrase');
  await panel.click('#vault [data-vault="unlock"]');
  await panel.until(`document.querySelector('#toast')?.textContent.includes('not the passphrase') || null`, { what: 'the refusal' });
  const stillLocked = (await vaultState()).locked;
  await panel.eval(`(() => { const box = document.querySelector('#vault input[aria-label="Passphrase"]'); box.value = ''; })()`);
  await panel.type('#vault input[aria-label="Passphrase"]', 'correct horse battery');
  await panel.click('#vault [data-vault="unlock"]');
  await panel.until(`spindlePanel.engine.vault.locked === false || null`, { timeout: 60_000, what: 'the unlock' });
  await panel.eval(`document.querySelector('#settings-dialog').close()`);
  await panel.eval(`(() => { const box = document.querySelector('#prompt'); box.value = ''; box.dispatchEvent(new Event('input')); })()`);
  const unlockedTurn = await newThreadSays('Unlocked now.');
  check('a wrong passphrase is refused; unlocked, turns run', () => {
    assert.equal(stillLocked, true);
    assert.ok(unlockedTurn.answer.includes('Key works.'), unlockedTurn.answer);
  });

  // A restarted engine is locked again.
  await panel.click('#settings');
  await restartEngine(`spindlePanel.engine.vault.locked === true`);
  // Restart engine closes Settings; open again, it shows the new engine's vault.
  if (!(await panel.eval(`document.querySelector('#settings-dialog').open`))) await panel.click('#settings');
  await unfoldProviders();
  await panel.type('#vault input[aria-label="Passphrase"]', 'correct horse battery');
  await panel.click('#vault [data-vault="unlock"]');
  await panel.until(`spindlePanel.engine.vault.locked === false || null`, { timeout: 60_000, what: 'the unlock' });
  // Changed, then removed: the keys still read. The form fits the side panel too.
  const unlockedOverflow = await overflow();
  await panel.eval(`document.querySelector('#vault .vault-more').open = true`);
  await panel.type('#vault input[aria-label="Current passphrase"]', 'correct horse battery');
  await panel.type('#vault input[aria-label="New passphrase (to change it)"]', 'a different passphrase');
  await panel.type('#vault input[aria-label="New passphrase again"]', 'a different passphrase');
  await panel.click('#vault [data-vault="change"]');
  await panel.until(`document.querySelector('#toast')?.textContent.includes('Passphrase changed') || null`, { timeout: 60_000, what: 'the change' });
  await panel.eval(`document.querySelector('#vault .vault-more').open = true`);
  await panel.type('#vault input[aria-label="Current passphrase"]', 'a different passphrase');
  await panel.click('#vault [data-vault="remove"]');
  await panel.until(`spindlePanel.engine.vault.mode === 'device' || null`, { timeout: 60_000, what: 'the passphrase removed' });
  await panel.eval(`document.querySelector('#settings-dialog').close()`);
  const deviceTurn = await newThreadSays('Back to the device key.');
  check('a restarted engine is locked; the passphrase changes and comes off; keys still read', () => {
    assert.ok(unlockedOverflow <= 0, `the dialog scrolls ${unlockedOverflow}px sideways`);
    assert.ok(deviceTurn.answer.includes('Key works.'), deviceTurn.answer);
    assert.equal(seen.chat.at(-1).authorization, `Bearer ${apiKey}`);
  });

  // 5. A sign-in whose refresh fails: while its token is still good, a token endpoint that
  //    is down fails nothing; once the provider refuses the refresh token, the sign-in has
  //    ended, the turn says so, and the row asks to sign in again.
  await panel.click('#settings');
  await unfoldProviders();
  await panel.click(`${row('subscription')} .provider-actions button`);
  await panel.until(`(() => { const t = document.querySelector('#toast')?.textContent ?? ''; return t.startsWith('Signed in') || null; })()`, { timeout: 60_000, what: 'signing in again' });
  await panel.eval(`document.querySelector('#settings-dialog').close()`);
  await choose('subscription');
  issued.refreshMode = 'down';
  const downTurn = await newThreadSays('Token endpoint down.');
  const usedWhileDown = seen.responses.at(-1).headers.authorization;
  issued.refreshMode = 'refuse';
  const before = seen.responses.length;
  const endedTurn = await newThreadSays('Refresh refused.');
  const endedNote = await panel.eval(`spindlePanel.threads.get(${JSON.stringify(endedTurn.id)})?.note ?? ''`);
  await panel.click('#settings');
  await unfoldProviders();
  const endedRow = await panel.eval(`document.querySelector('${row('subscription')} .provider-state')?.textContent`);
  check('a refresh that fails keeps a good token in use; a refused one ends the sign-in, and says so', () => {
    assert.ok(downTurn.answer.includes('Signed in and answering.'), downTurn.answer);
    assert.equal(usedWhileDown, `Bearer ${issued.exchanged}`, 'the token still good went');
    assert.equal(seen.responses.length, before, 'nothing was sent once the sign-in ended');
    assert.match(endedNote, /sign-in has ended/);
    assert.equal(endedRow, 'sign-in ended');
  });

  // 6. A forgotten passphrase: locked, "Forgot it" deletes every saved credential (after
  //    asking) and the keys start over without one; threads stay.
  await panel.type('#vault input[aria-label="New passphrase"]', 'forgotten passphrase');
  await panel.type('#vault input[aria-label="Same passphrase again"]', 'forgotten passphrase');
  await panel.click('#vault [data-vault="set"]');
  await panel.until(`spindlePanel.engine.vault.mode === 'passphrase' || null`, { timeout: 60_000, what: 'the passphrase' });
  await panel.click('#vault [data-vault="lock"]');
  await panel.until(`spindlePanel.engine.vault.locked || null`, { what: 'the lock' });
  const threadsBefore = await panel.eval('spindlePanel.threads.size');
  await panel.click('#vault [data-vault="forgot"]');
  await panel.click('#confirm-yes');
  await panel.until(`spindlePanel.engine.vault.mode === 'device' && !spindlePanel.engine.vault.locked || null`, { timeout: 60_000, what: 'the reset' });
  const afterReset = await rawKeys();
  const threadsAfter = await panel.eval('spindlePanel.threads.size');
  check('a forgotten passphrase deletes the saved keys (after asking); threads stay', () => {
    assert.deepEqual(afterReset, []);
    assert.equal(threadsAfter, threadsBefore);
  });
} finally {
  await browser.close().catch(() => {});
  server.close();
}

// Nothing in the profile's files holds a secret as text (Chrome has closed them).
const secrets = [apiKey, legacyKey, 'refresh-1', 'refresh-2'];
const leaks = [];
const walk = (dir) => {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) walk(path);
    else if (entry.isFile()) {
      const bytes = fs.readFileSync(path);
      for (const secret of secrets) if (bytes.includes(secret)) leaks.push(`${secret.slice(0, 10)}… in ${path.slice(profile.length)}`);
    }
  }
};
walk(profile);
check('no file of the Chrome profile holds a key or refresh token as text', () => {
  // A key a former build kept as text can linger in storage files until Chrome compacts
  // them; this test's own saves never write one.
  const own = leaks.filter((leak) => !leak.startsWith(legacyKey.slice(0, 10)));
  assert.deepEqual(own, []);
});

const failed = results.filter(([, ok]) => !ok);
console.log(`\n${results.length - failed.length}/${results.length} passed`);
process.exit(failed.length ? 1 : 0);
