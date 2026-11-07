// Browser sign-in for providers whose catalog entry has an OAuth flow Spindle can run
// (authorization code with PKCE, a loopback redirect). The panel runs the part a person
// sees: it opens the provider's sign-in page in a tab and catches the redirect to the
// registered loopback address, which Chrome reports to the extension before anything
// answers there, then closes the tab. The engine runs the rest: it exchanges the code
// for tokens, keeps them sealed in the vault, and refreshes them before they expire.
// A token never reaches the panel.
//
// Sign in with ChatGPT (`chatgpt-plan`, OpenAI's flow for open-source apps that run
// locally) adds: a first sign-in registers Spindle as the user's own client (its
// `client_id` names the registration entrypoint and the callback names the client
// issued, kept for later sign-ins to that account), each request names the installation
// (`ext_agent_host_id`) and the API the token is for (`resource`), the ID token is checked
// against the issuer's published keys, and the plan permission must be granted.

const REFRESH_MARGIN_MS = 5 * 60_000;
/** A sign-in whose token does not say when it expires is refreshed after this. */
const UNKNOWN_LIFETIME_MS = 60 * 60_000;
const SIGN_IN_MS = 10 * 60_000;
const TOKEN_MS = 30_000;

const base64url = (bytes) => btoa(String.fromCharCode(...bytes)).replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '');

/** The scope that lets a sign-in's token use the user's ChatGPT plan. */
export const PLAN_SCOPE = 'chatgpt.tokens.use.direct';
/** Where a ChatGPT user reviews and limits what apps use of their plan. */
export const CHATGPT_USAGE = 'https://chatgpt.com/settings/usage';
/** The name a first Sign in with ChatGPT gives the client it registers: the app's own
 * name, the same for every installation (the user may edit it on OpenAI's page). */
const AGENT_NAME = 'Spindle for Browser';
/** How far a clock may stray when an ID token's times are checked. */
const SKEW_MS = 2 * 60_000;

/** The OAuth flow `profile` signs in with here, or null: code + PKCE (`generic`, or
 * `chatgpt-plan`: Sign in with ChatGPT) whose redirect is a plain loopback address. */
export function flowOf(profile) {
  const flow = profile?.oauth_flow;
  if (!flow || !['generic', 'chatgpt-plan'].includes(flow.protocol ?? 'generic') || !flow.redirect_uri) return null;
  if (flow.protocol === 'chatgpt-plan' && !secure(flow.issuer)) return null;
  try {
    const redirect = new URL(flow.redirect_uri);
    const loopback = redirect.protocol === 'http:' && ['localhost', '127.0.0.1'].includes(redirect.hostname);
    return loopback && !redirect.search && !redirect.hash ? flow : null;
  } catch {
    return null;
  }
}

/** Whether `profile` signs in with Sign in with ChatGPT (the user's ChatGPT plan pays
 * for its requests). */
export const planSignIn = (profile) => flowOf(profile)?.protocol === 'chatgpt-plan';

/** The sites a sign-in reaches: the provider's API, its token endpoint (and an OpenID
 * issuer's keys), and the loopback redirect (so Chrome reports the redirect's address to
 * Spindle). */
export function signInSites(profile) {
  const flow = flowOf(profile);
  const urls = [profile.base_url, flow.token_url, flow.redirect_uri, ...(flow.issuer ? [flow.issuer] : [])];
  return [...new Set(urls.map((url) => `${new URL(url).origin}/*`))];
}

/** The API a `chatgpt-plan` token is for: the profile's base URL (`resource`). */
const resourceOf = (profile) => profile.base_url.replace(/\/+$/, '');

/** HTTPS, or HTTP to a literal loopback address (a server on this machine, as in tests),
 * as the engine allows a provider's endpoint. */
function secure(url) {
  try {
    const at = new URL(url);
    return at.protocol === 'https:' || (at.protocol === 'http:' && ['127.0.0.1', '[::1]'].includes(at.hostname));
  } catch {
    return false;
  }
}

/** Runs the visible part of a sign-in (panel only): resolves with what the engine
 * exchanges, `{code, verifier, redirect}`, and for Sign in with ChatGPT `{nonce,
 * clientId}` too (the client the callback named, or the one signed in again with).
 * `start` (from the engine, `signInStart`) holds a `chatgpt-plan` sign-in's `hostId` and
 * the `registration` it signs in again with (`{clientId, account, declined}`; none: a
 * first sign-in, which registers Spindle). */
export async function signInInTab(profile, start = {}) {
  const flow = flowOf(profile);
  if (!flow) throw new Error(`${profile.display_name} does not sign in through a browser here`);
  const plan = flow.protocol === 'chatgpt-plan';
  const registration = plan ? start.registration ?? null : null;
  if (plan && !start.hostId) throw new Error('this browser has no installation ID to sign in with');
  const { verifier, challenge, state } = await pkce();
  const nonce = plan ? base64url(crypto.getRandomValues(new Uint8Array(16))) : null;
  const url = new URL(flow.authorize_url);
  const query = {
    // A provider's own sign-in options first: none takes the place of the request's own.
    ...(flow.authorize_params ?? {}),
    response_type: 'code', client_id: registration?.clientId ?? flow.client_id, redirect_uri: flow.redirect_uri, scope: flow.scope,
    code_challenge: challenge, code_challenge_method: 'S256', state,
    ...(flow.audience ? { audience: flow.audience } : {}),
    ...(plan && {
      resource: resourceOf(profile), ext_agent_host_id: start.hostId, nonce,
      // The name only when registering; signing in again names the account it is for,
      // and asks again for the plan permission once the user declined it.
      ...(registration ? { ...(registration.account && { login_hint: registration.account }), ...(registration.declined && { prompt: 'consent' }) } : { agent_name_hint: AGENT_NAME }),
    }),
  };
  for (const [name, value] of Object.entries(query)) url.searchParams.set(name, value);
  // The tab starts blank and goes to the sign-in page only once its redirect is watched:
  // a provider that answers at once cannot get there first.
  const tab = await chrome.tabs.create({ url: 'about:blank' });
  const caught = await new Promise((resolve, reject) => {
    const end = (outcome) => {
      clearTimeout(timer);
      chrome.tabs.onUpdated.removeListener(updated);
      chrome.tabs.onRemoved.removeListener(removed);
      outcome();
    };
    const updated = (tabId, change, now) => {
      const address = change.url ?? now?.url;
      if (tabId !== tab.id || !address) return;
      let at;
      try {
        at = new URL(address);
      } catch {
        return;
      }
      const target = new URL(flow.redirect_uri);
      if (at.origin !== target.origin || at.pathname !== target.pathname) return;
      chrome.tabs.remove(tab.id).catch(() => {});
      const params = at.searchParams;
      if (params.get('state') !== state) return end(() => reject(new Error('the sign-in came back for another request; try again')));
      if (plan && params.get('error') === 'access_denied') {
        return end(() => reject(Object.assign(new Error('Spindle was not allowed to use your ChatGPT plan, so nothing was saved: sign in again and allow it, or add an OpenAI API key'), { kind: 'declined' })));
      }
      if (params.get('error')) {
        const why = params.get('error_description') || params.get('error');
        return end(() => reject(new Error(`${profile.display_name} did not sign you in: ${why}`)));
      }
      const code = params.get('code');
      if (!code) return end(() => reject(new Error('the sign-in came back without a code')));
      if (!plan) return end(() => resolve({ code }));
      // A first sign-in is complete only with the client ChatGPT issued; signing in again
      // keeps the one it asked with, and an answer naming another is not this account's.
      const issued = params.get('client_id')?.trim() || null;
      if (registration && issued && issued !== registration.clientId) {
        return end(() => reject(new Error('ChatGPT answered for another registration than this one; try again')));
      }
      const clientId = registration ? registration.clientId : issued;
      if (!clientId || clientId === flow.client_id) {
        return end(() => reject(new Error('ChatGPT did not finish registering Spindle (no client came back); try again')));
      }
      end(() => resolve({ code, clientId }));
    };
    const removed = (tabId) => {
      if (tabId === tab.id) end(() => reject(Object.assign(new Error('the sign-in tab was closed'), { kind: 'cancelled' })));
    };
    const timer = setTimeout(() => {
      chrome.tabs.remove(tab.id).catch(() => {});
      end(() => reject(new Error('the sign-in took too long; try again')));
    }, SIGN_IN_MS);
    chrome.tabs.onUpdated.addListener(updated);
    chrome.tabs.onRemoved.addListener(removed);
    chrome.tabs.update(tab.id, { url: url.href }).catch((error) => end(() => reject(error)));
  });
  return { ...caught, verifier, redirect: flow.redirect_uri, ...(plan && { nonce }) };
}

/** A PKCE pair and a state for one sign-in or connect, `{verifier, challenge, state}`:
 * S256, the challenge base64url(SHA-256(verifier)) without padding (RFC 7636). */
export async function pkce() {
  const verifier = base64url(crypto.getRandomValues(new Uint8Array(32)));
  const challenge = base64url(new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(verifier))));
  const state = base64url(crypto.getRandomValues(new Uint8Array(16)));
  return { verifier, challenge, state };
}

/** What a connect tab's address says: null while it is not the host's
 * `/connect/done`; there, `{code}` when the host connected this browser, and a refusal
 * (thrown) when the user denied it or the answer is for another request (`state`). */
export function connectAnswer(address, base, state) {
  let at;
  try {
    at = new URL(address);
  } catch {
    return null;
  }
  if (at.origin !== new URL(base).origin || at.pathname !== '/connect/done') return null;
  const params = at.searchParams;
  if (params.get('state') !== state) throw new Error('the connect came back for another request; try again');
  if (params.get('error')) throw Object.assign(new Error('your host was not connected: it was denied on its page'), { kind: 'denied' });
  const code = params.get('code');
  if (!code) throw new Error('the connect came back without a code');
  return { code };
}

/** One-click connect to a Spindle host (panel only): opens the host's own page for
 * `client` (this browser's name there) in a tab, where the user approves (pairing that
 * tab first if needed), catches the tab reaching `/connect/done`, checks it answers this
 * request, and closes it. Resolves with what the engine trades for a browser token,
 * `{code, verifier}`; no token passes through a URL or this page. */
export async function connectInTab(base, client) {
  const { verifier, challenge, state } = await pkce();
  const url = new URL('/connect', base);
  for (const [name, value] of Object.entries({ client, challenge, state })) url.searchParams.set(name, value);
  const tab = await chrome.tabs.create({ url: 'about:blank' });
  const code = await new Promise((resolve, reject) => {
    const end = (outcome) => {
      clearTimeout(timer);
      chrome.tabs.onUpdated.removeListener(updated);
      chrome.tabs.onRemoved.removeListener(removed);
      outcome();
    };
    const updated = (tabId, change, now) => {
      const address = change.url ?? now?.url;
      if (tabId !== tab.id || !address) return;
      let answer;
      try {
        answer = connectAnswer(address, base, state);
      } catch (error) {
        chrome.tabs.remove(tab.id).catch(() => {});
        return end(() => reject(error));
      }
      if (!answer) return;
      chrome.tabs.remove(tab.id).catch(() => {});
      end(() => resolve(answer.code));
    };
    const removed = (tabId) => {
      if (tabId === tab.id) end(() => reject(Object.assign(new Error('the connect tab was closed'), { kind: 'cancelled' })));
    };
    const timer = setTimeout(() => {
      chrome.tabs.remove(tab.id).catch(() => {});
      end(() => reject(new Error('connecting took too long; try again')));
    }, SIGN_IN_MS);
    chrome.tabs.onUpdated.addListener(updated);
    chrome.tabs.onRemoved.addListener(removed);
    chrome.tabs.update(tab.id, { url: url.href }).catch((error) => end(() => reject(error)));
  });
  return { code, verifier };
}

/** A JWT's payload, or null. */
function payloadOf(token) {
  try {
    const part = token.split('.')[1].replaceAll('-', '+').replaceAll('_', '/');
    const bytes = Uint8Array.from(atob(part.padEnd(Math.ceil(part.length / 4) * 4, '=')), (c) => c.charCodeAt(0));
    return JSON.parse(new TextDecoder().decode(bytes));
  } catch {
    return null;
  }
}

/** One request to the token endpoint (engine only) as `clientId` (the flow's own, but
 * for Sign in with ChatGPT's issued one): its JSON, or an error naming why. */
async function tokenRequest(profile, form, fetchImpl, clientId = flowOf(profile).client_id) {
  const flow = flowOf(profile);
  const host = new URL(flow.token_url).host;
  let response;
  try {
    response = await fetchImpl(flow.token_url, {
      method: 'POST', credentials: 'omit', redirect: 'error', cache: 'no-store', signal: AbortSignal.timeout(TOKEN_MS),
      headers: { 'content-type': 'application/x-www-form-urlencoded', accept: 'application/json' },
      body: new URLSearchParams({ ...form, client_id: clientId }).toString(),
    });
  } catch (error) {
    throw new Error(`${host} could not be reached (${error.message ?? error})`);
  }
  const body = (await response.json().catch(() => null)) ?? {};
  if (!response.ok) {
    const why = body.error_description || body.error || `HTTP ${response.status}`;
    throw Object.assign(new Error(`${host} refused: ${why}`), { status: response.status, code: body.error ?? null });
  }
  if (typeof body.access_token !== 'string' || !body.access_token.trim()) throw new Error(`${host} answered without an access token`);
  return body;
}

/** The secret a token response makes (kept in the vault as is). A Sign in with ChatGPT
 * one also keeps its issued client, the account it verified (`subject`) and the scopes
 * granted, which a refresh keeps unless it says otherwise. */
function signedIn(body, previous) {
  const access = body.access_token.trim();
  const claims = payloadOf(access);
  const identity = payloadOf(body.id_token ?? '') ?? {};
  const expires = Number.isFinite(body.expires_in) ? Date.now() + body.expires_in * 1000
    : Number.isFinite(claims?.exp) ? claims.exp * 1000
      : Date.now() + UNKNOWN_LIFETIME_MS;
  const account = identity.email ?? claims?.['https://api.openai.com/profile']?.email ?? previous?.account ?? null;
  const refresh = typeof body.refresh_token === 'string' && body.refresh_token.trim() ? body.refresh_token.trim() : null;
  const scopes = typeof body.scope === 'string' ? body.scope.split(/\s+/).filter(Boolean) : previous?.scopes;
  return {
    kind: 'signin', access, refresh: refresh ?? previous?.refresh ?? null, expires,
    account: typeof account === 'string' ? account : null,
    ...(previous?.clientId && { clientId: previous.clientId, subject: previous.subject }),
    ...(scopes && { scopes }),
    // Where the sign-in may go stays as it was saved (the engine checks it).
    ...(previous?.origin && { origin: previous.origin }),
    ...(previous?.tokenOrigin && { tokenOrigin: previous.tokenOrigin }),
  };
}

/** Exchanges a caught sign-in for tokens (engine only): `{secret, registration}` (the
 * secret the vault keeps; the client a Sign in with ChatGPT one registered or used, else
 * null). A Sign in with ChatGPT one
 * (`caught.clientId`, `caught.nonce`) is exchanged as its issued client for the API it is
 * for, its ID token checked against the issuer's keys; `registration` (the one it signed
 * in again with: `{clientId, subject}`) must be the account the token is for. Without the
 * plan permission it fails (`kind: 'declined'`, carrying the `registration` it made, so
 * the next sign-in asks again as that client). */
export async function exchange(profile, caught, fetchImpl = fetch, registration = null) {
  const flow = flowOf(profile);
  const { code, verifier, redirect } = caught;
  if (!flow || redirect !== flow.redirect_uri) throw new Error('that sign-in does not belong to this provider');
  if (flow.protocol !== 'chatgpt-plan') {
    const body = await tokenRequest(profile, { grant_type: 'authorization_code', code, code_verifier: verifier, redirect_uri: redirect }, fetchImpl);
    return { secret: signedIn(body), registration: null };
  }
  const clientId = caught.clientId;
  if (!clientId || clientId === flow.client_id) throw new Error('ChatGPT did not finish registering Spindle (no client came back); try again');
  if (registration && clientId !== registration.clientId) throw new Error('ChatGPT answered for another registration than this one; try again');
  const body = await tokenRequest(profile, {
    grant_type: 'authorization_code', code, code_verifier: verifier, redirect_uri: redirect, resource: resourceOf(profile),
  }, fetchImpl, clientId);
  try {
    const identity = await verifiedIdentity(flow, body.id_token, clientId, caught.nonce, fetchImpl);
    if (registration?.subject && identity.sub !== registration.subject) {
      throw new Error(`you chose another ChatGPT account than ${registration.account ?? 'the one Spindle signed in with'}: sign in with that one, or use another account`);
    }
    const account = typeof identity.email === 'string' ? identity.email : null;
    const made = { clientId, subject: identity.sub, account };
    const scopes = typeof body.scope === 'string' ? body.scope.split(/\s+/).filter(Boolean) : [];
    if (!scopes.includes(PLAN_SCOPE)) {
      throw Object.assign(new Error('Spindle was not allowed to use your ChatGPT plan, so nothing was saved: sign in again and allow it, or add an OpenAI API key'), { kind: 'declined', registration: { ...made, declined: true } });
    }
    return { secret: { ...signedIn(body, made), account }, registration: made };
  } catch (error) {
    // Not kept: the session it opened is ended at the issuer, not left open unused.
    const refresh = typeof body.refresh_token === 'string' ? body.refresh_token.trim() : '';
    if (refresh) await revoke(profile, { kind: 'signin', refresh, clientId }, fetchImpl, async () => {}).catch(() => {});
    throw error;
  }
}

/** A JSON document from `url` (engine only): an OpenID discovery document or key set. */
async function fetchJson(url, fetchImpl) {
  let response;
  try {
    response = await fetchImpl(url, { credentials: 'omit', redirect: 'error', cache: 'no-store', signal: AbortSignal.timeout(TOKEN_MS), headers: { accept: 'application/json' } });
  } catch (error) {
    throw new Error(`${new URL(url).host} could not be reached (${error.message ?? error})`);
  }
  const body = await response.json().catch(() => null);
  if (!response.ok || !body || typeof body !== 'object') throw new Error(`${new URL(url).host} answered HTTP ${response.status} for ${new URL(url).pathname}`);
  return body;
}

/** The issuer's discovery document, checked to name that issuer, with its keys and
 * revocation endpoint on the issuer's own site. */
async function discovery(flow, fetchImpl) {
  const issuer = flow.issuer.replace(/\/+$/, '');
  const found = await fetchJson(`${issuer}/.well-known/openid-configuration`, fetchImpl);
  const own = (url) => secure(url) && new URL(url).origin === new URL(issuer).origin;
  if (found.issuer !== issuer || !own(found.jwks_uri)) throw new Error(`${new URL(issuer).host} names another issuer or keys elsewhere; nothing was saved`);
  return { issuer, jwks: found.jwks_uri, revocation: own(found.revocation_endpoint) ? found.revocation_endpoint : null };
}

const fromBase64url = (text) => Uint8Array.from(atob(text.replaceAll('-', '+').replaceAll('_', '/').padEnd(Math.ceil(text.length / 4) * 4, '=')), (c) => c.charCodeAt(0));

/** An ID token's claims once it is shown to be the issuer's, for `clientId`, and the
 * answer to this sign-in (`nonce`): its RS256 signature against the issuer's published
 * keys, then its issuer, audience, times and nonce. */
async function verifiedIdentity(flow, idToken, clientId, nonce, fetchImpl) {
  const refused = (why) => new Error(`ChatGPT's sign-in could not be verified (${why}); nothing was saved`);
  const parts = typeof idToken === 'string' ? idToken.split('.') : [];
  if (parts.length !== 3) throw refused('no ID token');
  let header;
  let claims;
  try {
    header = JSON.parse(new TextDecoder().decode(fromBase64url(parts[0])));
    claims = JSON.parse(new TextDecoder().decode(fromBase64url(parts[1])));
  } catch {
    throw refused('an ID token that does not read');
  }
  if (header?.alg !== 'RS256') throw refused(`signed with ${header?.alg ?? 'nothing'}`);
  const { issuer, jwks } = await discovery(flow, fetchImpl);
  const keys = (await fetchJson(jwks, fetchImpl)).keys ?? [];
  const jwk = Array.isArray(keys) && keys.find((key) => key?.kty === 'RSA' && (header.kid == null || key.kid === header.kid) && (key.use == null || key.use === 'sig'));
  if (!jwk) throw refused('signed with a key the issuer does not publish');
  let signed;
  try {
    const key = await crypto.subtle.importKey('jwk', { kty: 'RSA', n: jwk.n, e: jwk.e, alg: 'RS256', ext: true }, { name: 'RSASSA-PKCS1-v1_5', hash: 'SHA-256' }, false, ['verify']);
    signed = await crypto.subtle.verify('RSASSA-PKCS1-v1_5', key, fromBase64url(parts[2]), new TextEncoder().encode(`${parts[0]}.${parts[1]}`));
  } catch {
    throw refused('a key or signature that does not read');
  }
  if (!signed) throw refused('a signature that does not match');
  const audience = Array.isArray(claims.aud) ? claims.aud : [claims.aud];
  const now = Date.now();
  if (claims.iss !== issuer) throw refused('another issuer');
  if (!audience.includes(clientId)) throw refused('meant for another client');
  if (!Number.isFinite(claims.exp) || claims.exp * 1000 < now - SKEW_MS) throw refused('expired');
  if (Number.isFinite(claims.iat) && claims.iat * 1000 > now + SKEW_MS) throw refused('issued in the future');
  if (!nonce || claims.nonce !== nonce) throw refused('the answer to another sign-in');
  if (typeof claims.sub !== 'string' || !claims.sub) throw refused('no account named');
  return claims;
}

/** Ends a Sign in with ChatGPT session at the issuer before it is forgotten (engine
 * only): its refresh token revoked as its issued client. True once the issuer confirmed
 * it (an empty 200, also for a token already invalid) or there was none to end; false
 * when that could not be confirmed (a network fault or 5xx is tried again, briefly). */
export async function revoke(profile, held, fetchImpl = fetch, wait = (ms) => new Promise((done) => setTimeout(done, ms))) {
  const flow = flowOf(profile);
  if (flow?.protocol !== 'chatgpt-plan' || held?.kind !== 'signin' || !held.refresh || !held.clientId) return true;
  let endpoint;
  try {
    endpoint = (await discovery(flow, fetchImpl)).revocation;
  } catch {
    return false;
  }
  if (!endpoint) return false;
  for (let attempt = 0; attempt < 3; attempt++) {
    if (attempt) await wait(500 * 2 ** attempt);
    try {
      const response = await fetchImpl(endpoint, {
        method: 'POST', credentials: 'omit', redirect: 'error', cache: 'no-store', signal: AbortSignal.timeout(TOKEN_MS),
        headers: { 'content-type': 'application/x-www-form-urlencoded' },
        body: new URLSearchParams({ token: held.refresh, token_type_hint: 'refresh_token', client_id: held.clientId }).toString(),
      });
      if (response.ok) return true;
      if (response.status < 500) return false;
    } catch {}
  }
  return false;
}

/** A sign-in about to expire, refreshed (engine only), or marked `ended` when the
 * provider refuses its refresh token (or it expired with none); null to use it as it is.
 * A rate limit, a server fault or no answer leaves a token that is still good in use, and
 * fails only a request whose token has already expired. */
export async function refreshed(profile, held, fetchImpl = fetch) {
  if (held.kind !== 'signin' || held.ended) return null;
  if (held.expires - Date.now() > REFRESH_MARGIN_MS) return null;
  const good = held.expires > Date.now();
  if (!held.refresh || !flowOf(profile)) return good ? null : { ...held, ended: true };
  // Sign in with ChatGPT refreshes as its issued client, for the API it is for (no scope:
  // the grant stays as it was).
  const plan = flowOf(profile).protocol === 'chatgpt-plan';
  if (plan && !held.clientId) return { ...held, ended: true, refresh: null };
  try {
    const form = { grant_type: 'refresh_token', refresh_token: held.refresh, ...(plan && { resource: resourceOf(profile) }) };
    return signedIn(await tokenRequest(profile, form, fetchImpl, plan ? held.clientId : undefined), held);
  } catch (error) {
    if ([400, 401, 403].includes(error.status)) return { ...held, ended: true, refresh: null };
    if (good) return null;
    throw error;
  }
}

/** Why a sign-in cannot be used any more (`ended`), to tell the user. */
export const endedText = (profile) =>
  `your ${profile?.display_name ?? 'provider'} sign-in has ended: sign in again in Settings, Providers`;
