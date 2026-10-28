// Browser sign-in for providers whose catalog entry has an OAuth flow Spindle can run
// (authorization code with PKCE, a loopback redirect). The panel runs the part a person
// sees: it opens the provider's sign-in page in a tab and catches the redirect to the
// registered loopback address, which Chrome reports to the extension before anything
// answers there, then closes the tab. The engine runs the rest: it exchanges the code
// for tokens, keeps them sealed in the vault, and refreshes them before they expire.
// A token never reaches the panel.

const REFRESH_MARGIN_MS = 5 * 60_000;
/** A sign-in whose token does not say when it expires is refreshed after this. */
const UNKNOWN_LIFETIME_MS = 60 * 60_000;
const SIGN_IN_MS = 10 * 60_000;
const TOKEN_MS = 30_000;

const base64url = (bytes) => btoa(String.fromCharCode(...bytes)).replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '');

/** The OAuth flow `profile` signs in with here, or null: a standard one (code + PKCE)
 * whose redirect is a plain loopback address. */
export function flowOf(profile) {
  const flow = profile?.oauth_flow;
  if (!flow || (flow.protocol ?? 'generic') !== 'generic' || !flow.redirect_uri) return null;
  try {
    const redirect = new URL(flow.redirect_uri);
    const loopback = redirect.protocol === 'http:' && ['localhost', '127.0.0.1'].includes(redirect.hostname);
    return loopback && !redirect.search && !redirect.hash ? flow : null;
  } catch {
    return null;
  }
}

/** The sites a sign-in reaches: the provider's API, its token endpoint, and the
 * loopback redirect (so Chrome reports the redirect's address to Spindle). */
export function signInSites(profile) {
  const flow = flowOf(profile);
  return [profile.base_url, flow.token_url, flow.redirect_uri].map((url) => `${new URL(url).origin}/*`);
}

const chatgpt = (profile) => profile.request_contract === 'chatgpt-responses' || profile.api_provider === 'chatgpt';

/** Runs the visible part of a sign-in (panel only): resolves with what the engine
 * exchanges, `{code, verifier, redirect}`. */
export async function signInInTab(profile) {
  const flow = flowOf(profile);
  if (!flow) throw new Error(`${profile.display_name} does not sign in through a browser here`);
  const verifier = base64url(crypto.getRandomValues(new Uint8Array(32)));
  const challenge = base64url(new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(verifier))));
  const state = base64url(crypto.getRandomValues(new Uint8Array(16)));
  const url = new URL(flow.authorize_url);
  const query = {
    response_type: 'code', client_id: flow.client_id, redirect_uri: flow.redirect_uri, scope: flow.scope,
    code_challenge: challenge, code_challenge_method: 'S256', state,
    ...(flow.audience ? { audience: flow.audience } : {}),
    // The ChatGPT sign-in page's own options for a coding agent's sign-in.
    ...(chatgpt(profile) ? { id_token_add_organizations: 'true', codex_cli_simplified_flow: 'true', originator: 'spindle' } : {}),
  };
  for (const [name, value] of Object.entries(query)) url.searchParams.set(name, value);
  // The tab starts blank and goes to the sign-in page only once its redirect is watched:
  // a provider that answers at once cannot get there first.
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
      if (params.get('error')) {
        const why = params.get('error_description') || params.get('error');
        return end(() => reject(new Error(`${profile.display_name} did not sign you in: ${why}`)));
      }
      const code = params.get('code');
      end(() => (code ? resolve(code) : reject(new Error('the sign-in came back without a code'))));
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
  return { code, verifier, redirect: flow.redirect_uri };
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

/** One request to the token endpoint (engine only): its JSON, or an error naming why. */
async function tokenRequest(profile, form, fetchImpl) {
  const flow = flowOf(profile);
  const host = new URL(flow.token_url).host;
  let response;
  try {
    response = await fetchImpl(flow.token_url, {
      method: 'POST', credentials: 'omit', redirect: 'error', cache: 'no-store', signal: AbortSignal.timeout(TOKEN_MS),
      headers: { 'content-type': 'application/x-www-form-urlencoded', accept: 'application/json' },
      body: new URLSearchParams({ ...form, client_id: flow.client_id }).toString(),
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

/** The secret a token response makes (kept in the vault as is). */
function signedIn(body, previous) {
  const access = body.access_token.trim();
  const claims = payloadOf(access);
  const identity = payloadOf(body.id_token ?? '') ?? {};
  const expires = Number.isFinite(body.expires_in) ? Date.now() + body.expires_in * 1000
    : Number.isFinite(claims?.exp) ? claims.exp * 1000
      : Date.now() + UNKNOWN_LIFETIME_MS;
  const account = identity.email ?? claims?.['https://api.openai.com/profile']?.email ?? previous?.account ?? null;
  const refresh = typeof body.refresh_token === 'string' && body.refresh_token.trim() ? body.refresh_token.trim() : null;
  return {
    kind: 'signin', access, refresh: refresh ?? previous?.refresh ?? null, expires,
    account: typeof account === 'string' ? account : null,
  };
}

/** Exchanges a caught sign-in for tokens (engine only). */
export async function exchange(profile, { code, verifier, redirect }, fetchImpl = fetch) {
  if (!flowOf(profile) || redirect !== flowOf(profile).redirect_uri) throw new Error('that sign-in does not belong to this provider');
  const body = await tokenRequest(profile, { grant_type: 'authorization_code', code, code_verifier: verifier, redirect_uri: redirect }, fetchImpl);
  return signedIn(body);
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
  try {
    return signedIn(await tokenRequest(profile, { grant_type: 'refresh_token', refresh_token: held.refresh }, fetchImpl), held);
  } catch (error) {
    if ([400, 401, 403].includes(error.status)) return { ...held, ended: true, refresh: null };
    if (good) return null;
    throw error;
  }
}

/** Why a sign-in cannot be used any more (`ended`), to tell the user. */
export const endedText = (profile) =>
  `your ${profile?.display_name ?? 'provider'} sign-in has ended: sign in again in Settings, Providers`;
