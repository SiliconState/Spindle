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
//      and the threads stay;
//   7. Sign in with ChatGPT (`chatgpt-plan`): "Continue with ChatGPT" registers Spindle
//      (the registration entrypoint, the app's name, this installation's ID, the API the
//      token is for, a nonce), keeps the client the callback names, exchanges as it, and
//      takes the ID token only signed with the issuer's published key, for that client and
//      nonce; first time, it says the plan is in use; a turn sends the API's bearer token
//      and a body the route takes (no output cap, tools in a namespace); sign-out revokes
//      the session at the issuer and keeps the registration, so signing in again is the
//      same client for the same account; a forged ID token, another account or a declined
//      plan permission saves nothing and ends the session it opened; another account
//      registers anew, and a client whose code was refused is kept for the next sign-in.
//
//   SPINDLE_BROWSER=/path/to/chrome [SPINDLE_SHOTS=DIR] node chrome/tests/signin_vault.mjs DIST
//   (SPINDLE_SHOTS: pictures of the plan sign-in's dialog and row)
import assert from 'node:assert/strict';
import { createHash, generateKeyPairSync, randomBytes, sign } from 'node:crypto';
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
/** A site's secret (its Authorization header's token). */
const siteToken = `sk-site-${randomBytes(12).toString('hex')}`;
const legacyKey = `sk-legacy-${randomBytes(12).toString('hex')}`;

// A free port where nothing listens: the sign-in's registered loopback redirect.
const callbackPort = await new Promise((done) => {
  const probe = net.createServer().listen(0, '127.0.0.1', () => {
    const { port } = probe.address();
    probe.close(() => done(port));
  });
});
const redirectUri = `http://localhost:${callbackPort}/auth/callback`;
// Sign in with ChatGPT's redirect is on 127.0.0.1 (OpenAI: never `localhost`).
const planRedirect = `http://127.0.0.1:${callbackPort}/auth/callback`;
const PLAN_SCOPE = 'openid profile email offline_access resource.invoke chatgpt.tokens.use.direct';
// The client OpenAI issues a registration (each registration its own).
const issue = () => `oaiapp_${randomBytes(6).toString('hex')}`;
const ISSUED = issue();
const SUBJECT = `user-${randomBytes(4).toString('hex')}`;
// The issuer's signing key (its public half published at /jwks), and another it never
// published (a forged token).
const signing = generateKeyPairSync('rsa', { modulusLength: 2048 });
const forger = generateKeyPairSync('rsa', { modulusLength: 2048 });
const published = { ...signing.publicKey.export({ format: 'jwk' }), kid: 'k1', alg: 'RS256', use: 'sig' };
const signed = (claims, key = signing.privateKey) => {
  const head = b64url(JSON.stringify({ alg: 'RS256', kid: 'k1', typ: 'JWT' }));
  const body = b64url(JSON.stringify(claims));
  return `${head}.${body}.${sign('RSA-SHA256', Buffer.from(`${head}.${body}`), key).toString('base64url')}`;
};

// ---- The scripted services: an OAuth server, a ChatGPT-style backend, a key provider.
const seen = { authorize: [], token: [], responses: [], chat: [], planAuthorize: [], planToken: [], planRevoke: [], planResponses: [] };
// The plan sign-in's scripted issuer: what it does with the next sign-in (`mode`: ok,
// forge, other (another account), decline (no plan scope), grant (the code refused)),
// and the client the next registration is issued.
const plan = { code: null, challenge: null, nonce: null, client: null, mode: 'ok', refresh: null, next: ISSUED, tokens: [] };
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
  if (request.method === 'GET' && url.pathname === '/.well-known/openid-configuration') {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ issuer: origin, authorization_endpoint: `${origin}/plan/authorize`, token_endpoint: `${origin}/plan/token`,
      jwks_uri: `${origin}/jwks`, revocation_endpoint: `${origin}/plan/revoke` }));
    return;
  }
  if (request.method === 'GET' && url.pathname === '/jwks') {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ keys: [published] }));
    return;
  }
  if (request.method === 'GET' && url.pathname === '/plan/authorize') {
    const asked = Object.fromEntries(url.searchParams);
    seen.planAuthorize.push(asked);
    plan.code = `plan-code-${randomBytes(6).toString('hex')}`;
    [plan.challenge, plan.nonce] = [asked.code_challenge, asked.nonce];
    const back = new URL(asked.redirect_uri);
    back.searchParams.set('code', plan.code);
    back.searchParams.set('state', asked.state);
    // A first sign-in registers: the callback names the client issued. Signing in again
    // as that client names none.
    if (asked.client_id === 'dynamic_agent_client') back.searchParams.set('client_id', plan.next);
    plan.client = asked.client_id === 'dynamic_agent_client' ? plan.next : asked.client_id;
    response.writeHead(302, { location: back.href }).end();
    return;
  }
  if (request.method === 'POST' && url.pathname === '/plan/token') {
    const form = Object.fromEntries(new URLSearchParams(await readBody(request)));
    seen.planToken.push(form);
    const now = Math.floor(Date.now() / 1000);
    const access = (n) => `plan-access-${n}-${randomBytes(4).toString('hex')}`;
    let answer = null;
    if (form.grant_type === 'authorization_code' && plan.mode !== 'grant') {
      const verified = createHash('sha256').update(form.code_verifier ?? '').digest('base64url') === plan.challenge;
      if (form.code === plan.code && verified && form.client_id === plan.client && form.redirect_uri === planRedirect && form.resource === `${origin}/v1`) {
        const sub = plan.mode === 'other' ? 'someone-else' : SUBJECT;
        const id = { iss: origin, aud: [plan.client], sub, email: EMAIL, nonce: plan.nonce, iat: now, exp: now + 3600 };
        plan.refresh = `plan-refresh-${randomBytes(4).toString('hex')}`;
        const token = access(1);
        plan.tokens.push(token, plan.refresh);
        answer = { access_token: token, refresh_token: plan.refresh, token_type: 'Bearer', expires_in: 3600,
          id_token: signed(id, plan.mode === 'forge' ? forger.privateKey : signing.privateKey),
          scope: plan.mode === 'decline' ? 'openid profile email offline_access' : PLAN_SCOPE };
      }
    }
    response.writeHead(answer ? 200 : 400, { 'content-type': 'application/json' });
    response.end(JSON.stringify(answer ?? { error: 'invalid_grant' }));
    return;
  }
  if (request.method === 'POST' && url.pathname === '/plan/revoke') {
    seen.planRevoke.push(Object.fromEntries(new URLSearchParams(await readBody(request))));
    response.writeHead(200).end();
    return;
  }
  if (request.method === 'POST' && url.pathname === '/v1/responses') {
    seen.planResponses.push({ headers: request.headers, body: await readBody(request) });
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    response.end(`data: ${JSON.stringify({ type: 'response.output_text.delta', delta: 'Your plan answers.' })}\n\n`
      + `data: ${JSON.stringify({ type: 'response.completed', response: { status: 'completed', output: [], usage: { input_tokens: 12, output_tokens: 4 } } })}\n\n`);
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
        scope: 'openid profile email offline_access', redirect_uri: redirectUri,
        // A provider's own sign-in options, as the Codex route's catalog entry carries them.
        authorize_params: { codex_cli_simplified_flow: 'true', originator: 'spindle', client_id: 'not-this' } },
      model_specs: { 'gpt-test': { context_window: 200000, max_output_tokens: 4096 } },
    },
    {
      id: 'scripted', display_name: 'Scripted', api_provider: 'openai', request_contract: 'openai-chat-completions',
      base_url: `${origin}/v1`, default_model: 'scripted-model', models: ['scripted-model'], requires_api_key: true,
      model_specs: { 'scripted-model': { context_window: 200000, max_output_tokens: 4096 } },
    },
    {
      id: 'plan', display_name: 'ChatGPT plan', api_provider: 'openai', request_contract: 'chatgpt-plan',
      base_url: `${origin}/v1`, default_model: 'gpt-test', models: ['gpt-test'], requires_api_key: true,
      oauth_flow: { authorize_url: `${origin}/plan/authorize`, token_url: `${origin}/plan/token`, client_id: 'dynamic_agent_client',
        scope: PLAN_SCOPE, redirect_uri: planRedirect, protocol: 'chatgpt-plan', issuer: origin },
      model_specs: { 'gpt-test': { context_window: 200000, max_output_tokens: 4096 } },
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
fs.writeFileSync(join(dist, 'manifest.json'), JSON.stringify({ ...manifest, host_permissions: [`${origin}/*`, `http://localhost:${callbackPort}/*`, `http://127.0.0.1:${callbackPort}/*`] }, null, 2));

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
const unfoldProviders = () => panel.eval(`document.querySelector('[data-fold="providers"]').open = true; document.querySelector('[data-fold="vault"]').open = true`);
const row = (id) => `.provider[data-provider="${id}"]`;
const unfoldSites = () => panel.eval(`for (const name of ['commands', 'sites']) { const fold = document.querySelector('[data-fold="' + name + '"]'); if (fold) fold.open = true; }`);
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
  const buttons = await panel.eval(`[...document.querySelectorAll('${row('subscription')} button')].map((b) => b.getAttribute('aria-label') ?? b.textContent)`);
  const hasKeyField = await panel.eval(`Boolean(document.querySelector('${row('subscription')} input'))`);
  check('a browser sign-in provider offers Sign in and no key field', () => {
    assert.deepEqual(buttons, ['Sign in to Subscription']);
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
    assert.equal(asked.originator, 'spindle');
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

  // 7. Sign in with ChatGPT (run here, and signed out at its end, so the sections after
  //    see the vault as before).
  const planRow = row('plan');
  const signOutPlan = async () => {
    const button = `${planRow} button[aria-label="Sign out of ChatGPT plan"]`;
    await panel.click(button).catch(async (error) => {
      const state = await panel.eval(`({ settings: document.querySelector('#settings-dialog').open, row: document.querySelector('${planRow}')?.outerHTML.slice(0, 1500) ?? null, kinds: spindlePanel.engine.vault.kinds, toast: document.querySelector('#toast')?.textContent })`);
      throw new Error(`${error.message}\n${JSON.stringify(state)}`);
    });
    await panel.until(`spindlePanel.engine.vault.kinds.plan === undefined || null`, { what: 'the sign-out' });
  };
  const planButtons = () => panel.eval(`[...document.querySelectorAll('${planRow} button')].map((b) => b.getAttribute('aria-label') ?? b.textContent)`);
  await panel.click('#settings');
  await unfoldProviders();
  const firstButtons = await planButtons();
  await panel.click(`${planRow} button[data-act="signin"]`);
  await panel.until(`spindlePanel.engine.vault.kinds.plan === 'signin'`, { timeout: 60_000, what: 'the plan sign-in' });
  const welcomed = await panel.until(`document.querySelector('#plan-dialog')?.open || null`, { what: 'the plan confirmation' }).catch(() => false);
  const welcomeText = await panel.eval(`document.querySelector('#plan-dialog').innerText`);
  if (process.env.SPINDLE_SHOTS) await panel.screenshot(join(process.env.SPINDLE_SHOTS, 'plan-dialog.png'));
  await panel.eval(`document.querySelector('#plan-dialog').close()`);
  await panel.until(`document.querySelector('${planRow} .provider-account')?.textContent === '${EMAIL}' || null`, { what: 'the plan row to say who' });
  const signedButtons = await planButtons();
  // Four actions on its row, still within a side panel's width.
  const planOverflow = await overflow();
  if (process.env.SPINDLE_SHOTS) {
    await panel.eval(`document.querySelector('${planRow}').scrollIntoView({ block: 'center' })`);
    await panel.screenshot(join(process.env.SPINDLE_SHOTS, 'plan-row.png'));
  }
  const firstAsk = seen.planAuthorize.at(-1);
  const firstExchange = seen.planToken.at(-1);
  check('Continue with ChatGPT registers Spindle and exchanges as the client issued', () => {
    assert.deepEqual(firstButtons, ['Continue with ChatGPT'], 'no other account to offer yet');
    assert.equal(firstAsk.client_id, 'dynamic_agent_client');
    assert.equal(firstAsk.agent_name_hint, 'Spindle for Browser');
    assert.match(firstAsk.ext_agent_host_id, /^urn:uuid:[0-9a-f-]{36}$/);
    assert.equal(firstAsk.resource, `${origin}/v1`);
    assert.equal(firstAsk.redirect_uri, planRedirect);
    assert.equal(firstAsk.scope, PLAN_SCOPE);
    assert.ok(firstAsk.nonce?.length >= 16 && firstAsk.state, JSON.stringify(firstAsk));
    assert.equal(firstAsk.login_hint, undefined);
    assert.equal(firstAsk.codex_cli_simplified_flow, undefined, 'no Codex options');
    assert.equal(firstExchange.client_id, ISSUED, 'exchanged as the issued client, never the entrypoint');
    assert.equal(firstExchange.resource, `${origin}/v1`);
    assert.ok(welcomed, 'the first sign-in says the plan is in use');
    assert.match(welcomeText, /You're using your ChatGPT plan/);
    assert.deepEqual(signedButtons.map((b) => b.split(':')[0]), ['Continue with ChatGPT', 'Manage usage', 'Sign out of ChatGPT plan'], signedButtons.join(' | '));
    assert.ok(planOverflow <= 0, `the dialog scrolls ${planOverflow}px sideways`);
  });
  await panel.eval(`document.querySelector('#settings-dialog').close()`);
  await choose('plan');
  const planNote = await panel.eval(`(() => { document.querySelector('#model-chip').click(); const shown = !document.querySelector('#model-plan').hidden; document.querySelector('#model-dialog').close(); return shown; })()`);
  const planTurn = await newThreadSays('Use my plan.');
  const planSent = seen.planResponses.at(-1);
  const planBody = planSent ? JSON.parse(planSent.body) : {};
  check('a turn on the plan sends the API its bearer token and a body the route takes', () => {
    assert.equal(planTurn.state, 'idle', JSON.stringify(planTurn));
    assert.ok(planTurn.answer.includes('Your plan answers.'), planTurn.answer);
    assert.match(planSent.headers.authorization, /^Bearer plan-access-1-/);
    assert.equal(planSent.headers['chatgpt-account-id'], undefined);
    assert.equal(planBody.store, false);
    assert.equal(planBody.stream, true);
    assert.equal(planBody.max_output_tokens, undefined);
    assert.equal(planBody.tools?.length, 1);
    assert.equal(planBody.tools[0].type, 'namespace');
    assert.ok(planBody.tools[0].tools.every((tool) => tool.type === 'function'), JSON.stringify(planBody.tools[0]).slice(0, 300));
    assert.ok(planNote, 'the model page says the ChatGPT plan is in use');
  });
  // Sign out: the session revoked at the issuer, the registration kept.
  await panel.click('#settings');
  await unfoldProviders();
  const hostId = firstAsk.ext_agent_host_id;
  const signedOutRefresh = plan.refresh;
  await signOutPlan();
  const revoked = seen.planRevoke.at(-1);
  const kept = await panel.eval('spindlePanel.engine.registrations');
  const signedOutButtons = await planButtons();
  // Again: the same client, for the same account; no welcome this time.
  await panel.click(`${planRow} button[data-act="signin"]`);
  await panel.until(`spindlePanel.engine.vault.kinds.plan === 'signin'`, { timeout: 60_000, what: 'the plan sign-in again' });
  const again = seen.planAuthorize.at(-1);
  const welcomedAgain = await panel.eval(`document.querySelector('#plan-dialog').open`);
  check('sign-out revokes the session and keeps the registration; signing in again is that client', () => {
    assert.equal(revoked?.token, signedOutRefresh, 'the refresh token of the session signed out');
    assert.equal(revoked.token_type_hint, 'refresh_token');
    assert.equal(revoked.client_id, ISSUED);
    assert.deepEqual(kept, { plan: EMAIL });
    assert.deepEqual(signedOutButtons, ['Continue with ChatGPT', 'Use another ChatGPT account'], 'signed out, another account can register');
    assert.equal(again.client_id, ISSUED);
    assert.equal(again.login_hint, EMAIL);
    assert.equal(again.agent_name_hint, undefined);
    assert.equal(again.ext_agent_host_id, hostId, "the same installation's ID");
    assert.equal(welcomedAgain, false, 'said once only');
  });
  // What is refused saves nothing: a forged ID token, another account, a declined plan.
  const refusals = {};
  // Each refusal leaves nothing signed in: one sign-out before them.
  await signOutPlan();
  const revokedBefore = seen.planRevoke.length;
  for (const mode of ['forge', 'other', 'decline']) {
    plan.mode = mode;
    // The last refusal's words gone first: each attempt is read from its own.
    await panel.eval(`document.querySelector('#toast-text').textContent = ''`);
    await panel.click(`${planRow} button[data-act="signin"]`);
    refusals[mode] = await panel.until(`(() => { const t = document.querySelector('#toast-text')?.textContent ?? ''; return t.startsWith('Not signed in') ? t : null; })()`, { timeout: 60_000, what: `the ${mode} refusal` });
    refusals[`${mode}Kept`] = await panel.eval('spindlePanel.engine.vault.kinds.plan ?? null');
  }
  const refusedRevoked = seen.planRevoke.slice(revokedBefore);
  plan.mode = 'ok';
  await panel.click(`${planRow} button[data-act="signin"]`);
  await panel.until(`spindlePanel.engine.vault.kinds.plan === 'signin'`, { timeout: 60_000, what: 'the plan sign-in after the refusals' });
  const afterDecline = seen.planAuthorize.at(-1);
  check('a forged ID token, another account or a declined plan permission saves nothing, and ends the session it opened', () => {
    assert.match(refusals.forge, /could not be verified/, refusals.forge);
    assert.match(refusals.other, /another ChatGPT account/, refusals.other);
    assert.match(refusals.decline, /not allowed to use your ChatGPT plan/, refusals.decline);
    for (const mode of ['forge', 'other', 'decline']) assert.equal(refusals[`${mode}Kept`], null, mode);
    assert.equal(refusedRevoked.length, 3, JSON.stringify(refusedRevoked));
    assert.ok(refusedRevoked.every((form) => /^plan-refresh-/.test(form.token) && form.client_id === ISSUED), JSON.stringify(refusedRevoked));
    assert.equal(afterDecline.prompt, 'consent', 'after a decline, the plan permission is asked for again');
  });
  // Another account, whose first code is refused: the client issued it is kept, so the
  // next sign-in is that client, not a third registration.
  await signOutPlan();
  const OTHER = issue();
  plan.next = OTHER;
  plan.mode = 'grant';
  await panel.eval(`document.querySelector('#toast-text').textContent = ''`);
  await panel.click(`${planRow} button[data-act="another"]`);
  const grantRefused = await panel.until(`(() => { const t = document.querySelector('#toast-text')?.textContent ?? ''; return t.startsWith('Not signed in') ? t : null; })()`, { timeout: 60_000, what: 'the refused code' });
  const anotherAsk = seen.planAuthorize.at(-1);
  plan.mode = 'ok';
  await panel.click(`${planRow} button[data-act="signin"]`);
  await panel.until(`spindlePanel.engine.vault.kinds.plan === 'signin'`, { timeout: 60_000, what: 'the sign-in as the client kept' });
  const keptAsk = seen.planAuthorize.at(-1);
  check('another account registers anew; a client whose code was refused is kept for the next sign-in', () => {
    assert.match(grantRefused, /invalid_grant/, grantRefused);
    assert.equal(anotherAsk.client_id, 'dynamic_agent_client');
    assert.equal(anotherAsk.agent_name_hint, 'Spindle for Browser');
    assert.equal(anotherAsk.login_hint, undefined, 'no hint of the other account');
    assert.equal(keptAsk.client_id, OTHER, 'the client issued, signed in again');
    assert.equal(keptAsk.agent_name_hint, undefined);
    assert.equal(seen.planToken.at(-1).client_id, OTHER);
  });
  await signOutPlan();
  await panel.eval(`document.querySelector('#settings-dialog').close()`);

  // 3. A key saved through Settings is sealed at rest.
  await panel.click('#settings');
  await unfoldProviders();
  await panel.click(`${row('scripted')} .provider-actions button`);
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

  // 3b. A site's secret header is sealed like a key, and listed by its site, not as a
  //     provider's.
  await panel.click('#settings');
  await unfoldSites();
  await panel.type('#site-url', `${origin}/api/`);
  await panel.type('#site-secret', `Bearer ${siteToken}`);
  await panel.click('#site-form button.primary');
  const siteName = await panel.until(`(spindlePanel.engine.sites?.[0]?.secret?.length && (spindlePanel.engine.vault.sites ?? []).length && spindlePanel.engine.sites[0].name) || null`, { what: 'the site and its secret' });
  const siteRecords = await rawKeys();
  const siteVault = await vaultState();
  const siteKept = await panel.eval('spindlePanel.engine.sites[0]');
  await panel.eval(`document.querySelector('#settings-dialog').close()`);
  check("a site's secret header is sealed like a key, and listed by its site", () => {
    const record = siteRecords.find((kept) => kept.name === `:site:${siteName}`);
    assert.ok(record?.sealed && record.text === null && record.kind === 'site', JSON.stringify(siteRecords));
    assert.deepEqual(siteVault.sites, [siteName]);
    assert.ok(!(`:site:${siteName}` in siteVault.kinds), 'not a provider');
    assert.deepEqual(siteKept.secret, ['Authorization'], 'the header Settings names when none is typed');
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
  // Locked, no secret header can be kept: the site is not added either.
  await unfoldSites();
  await panel.type('#site-url', `${origin}/other/`);
  await panel.type('#site-secret', `Bearer ${siteToken}-other`);
  await panel.click('#site-form button.primary');
  const lockedSite = await panel.until(`(() => { const t = document.querySelector('#toast')?.textContent ?? ''; return t.includes('locked') ? t : null; })()`, { what: 'the refusal' });
  const lockedSites = await panel.eval('spindlePanel.engine.sites.length');
  await panel.eval(`for (const id of ['site-url', 'site-header', 'site-secret']) document.getElementById(id).value = ''`);
  check('while the keys are locked, no secret header is kept (nor its site)', () => {
    assert.match(lockedSite, /locked/);
    assert.equal(lockedSites, 1);
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
  const resealed = await rawKeys();
  await panel.eval(`document.querySelector('#settings-dialog').close()`);
  const deviceTurn = await newThreadSays('Back to the device key.');
  check('a restarted engine is locked; the passphrase changes and comes off; keys still read', () => {
    assert.ok(unlockedOverflow <= 0, `the dialog scrolls ${unlockedOverflow}px sideways`);
    assert.ok(deviceTurn.answer.includes('Key works.'), deviceTurn.answer);
    assert.equal(seen.chat.at(-1).authorization, `Bearer ${apiKey}`);
    assert.ok(resealed.some((kept) => kept.name === `:site:${siteName}` && kept.sealed), `the site's secret stays: ${JSON.stringify(resealed)}`);
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
  await unfoldSites();
  const goneMark = await panel.until(`document.querySelector('#sites .site-secret.lost')?.textContent || null`, { what: "the site's secret marked gone" });
  check('a forgotten passphrase deletes the saved keys and secret headers (after asking); threads stay', () => {
    assert.deepEqual(afterReset, []);
    assert.equal(threadsAfter, threadsBefore);
    assert.match(goneMark, /Authorization header gone/);
  });
} finally {
  await browser.close().catch(() => {});
  server.close();
}

// Nothing in the profile's files holds a secret as text (Chrome has closed them).
const secrets = [apiKey, legacyKey, 'refresh-1', 'refresh-2', siteToken, ...plan.tokens];
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
