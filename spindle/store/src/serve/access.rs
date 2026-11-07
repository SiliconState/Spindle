//! Cloudflare Access. With a team configured, a request that arrived through the
//! public origin (the tunnel) must carry an Access token: the `Cf-Access-Jwt-Assertion`
//! header cloudflared adds (or the `CF_Authorization` cookie), an RS256 JWT signed by
//! one of the team's current keys, issued by the team, for this application's AUD tag,
//! and unexpired. It is a second lock beside device pairing: a stolen device cookie
//! alone does not get through the tunnel, and neither does any request if the Access
//! application is removed or the hostname is routed here without it. A request is
//! taken as through the tunnel by its Host, or by Cloudflare's `Cf-Ray` or
//! `Cf-Connecting-Ip` header whatever its Host says. Other loopback requests (Host
//! 127.0.0.1/localhost) are not affected; they still need a paired device.
//!
//! The team's keys are fetched from `https://TEAM/cdn-cgi/access/certs`, kept in
//! `access-keys.json` so a restart does not depend on reaching Cloudflare (with keys
//! kept, the host listens at once and refreshes them after), refreshed hourly and when
//! a token names a key not yet known (at most once a minute). An hourly refresh runs
//! behind the request that found the set stale (that request verifies with the keys
//! kept); only a key not yet known is fetched while its request waits, with a short
//! timeout, one fetch at a time.
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Clock difference tolerated for exp/nbf/iat: generous, because a laptop's (or WSL's)
/// clock can drift by minutes after sleep, and Access tokens live for hours.
pub(super) const LEEWAY: u64 = 300;
const REFRESH: Duration = Duration::from_secs(3600);
const RETRY: Duration = Duration::from_secs(60);
const MAX_TOKEN: usize = 8192;
/// A fetch a request waits for (a token signed by a key not yet known): short, as the
/// request holds one of the host's connections meanwhile.
const INLINE: (Duration, Duration) = (Duration::from_secs(3), Duration::from_secs(5));
/// A fetch nothing waits for (at start, or behind a request): connect, then in all.
const BEHIND: (Duration, Duration) = (Duration::from_secs(5), Duration::from_secs(10));

pub(super) struct Access {
    /// The team domain, e.g. `example.cloudflareaccess.com`.
    team: String,
    /// The application's AUD tag.
    audience: String,
    cache: PathBuf,
    /// Shared with a refresh running behind a request.
    keys: Arc<Mutex<Keys>>,
}

#[derive(Default)]
struct Keys {
    /// kid -> (modulus, exponent), big-endian.
    rsa: BTreeMap<String, (Vec<u8>, Vec<u8>)>,
    fetched: Option<Instant>,
    attempted: Option<Instant>,
}

/// Who Access let through, and until when (seconds since the epoch): the token's
/// expiry with the clock leeway, past which it no longer verifies. An event stream
/// opened with it ends then (the reconnect meets Access again).
#[derive(Debug, PartialEq)]
pub(super) struct Identity {
    pub email: Option<String>,
    pub until: u64,
}

impl Access {
    /// `team` is the team domain (an `https://` prefix is accepted) and `audience` the
    /// application's AUD tag (64 hex characters).
    pub(super) fn new(team: &str, audience: &str, root: &std::path::Path) -> Result<Self, String> {
        let team = team
            .trim()
            .trim_start_matches("https://")
            .trim_end_matches('/')
            .to_ascii_lowercase();
        if team.is_empty()
            || !team.ends_with(".cloudflareaccess.com")
            || !team
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        {
            return Err("--access-team needs the team domain, NAME.cloudflareaccess.com".into());
        }
        let audience = audience.trim().to_ascii_lowercase();
        if audience.len() != 64 || !audience.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("--access-aud needs the application's AUD tag (64 hex characters)".into());
        }
        let access = Self {
            team,
            audience,
            cache: root.join("access-keys.json"),
            keys: Arc::default(),
        };
        if let Ok(text) = std::fs::read_to_string(&access.cache) {
            access.lock().rsa = parse_keys(&text);
        }
        // With no keys kept, nothing through the tunnel could get in: fetched before the
        // host listens. Kept keys verify at once, and `refresh_kept` (called once the
        // host listens) brings them up to date, so a slow Cloudflare never holds the
        // start (it once held it for the whole fetch timeout).
        if access.lock().rsa.is_empty()
            && let Err(error) = access.refresh(BEHIND)
        {
            eprintln!("spindle serve: Cloudflare Access keys unavailable for now: {error}");
        }
        Ok(access)
    }

    /// Brings the kept keys up to date unless a fetch was just tried (by `new`, with
    /// none kept): run off the start path, once the host listens.
    pub(super) fn refresh_kept(&self) {
        if self.lock().attempted.is_some() {
            return;
        }
        if let Err(error) = self.refresh(BEHIND) {
            eprintln!(
                "spindle serve: Cloudflare Access keys not refreshed (the kept ones still verify): {error}"
            );
        }
    }

    pub(super) fn team(&self) -> &str {
        &self.team
    }

    /// A team with no keys that never fetches any (the host's tests: every token fails).
    #[cfg(all(test, unix))]
    pub(super) fn keyless() -> Self {
        Self {
            team: "test.cloudflareaccess.com".into(),
            audience: "0".repeat(64),
            cache: PathBuf::new(),
            keys: Arc::new(Mutex::new(Keys {
                fetched: Some(Instant::now()),
                attempted: Some(Instant::now()),
                ..Keys::default()
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Keys> {
        lock(&self.keys)
    }

    /// Fetches the team's keys (never while holding the lock) and keeps them, within
    /// `timeouts` (connect, in all).
    fn refresh(&self, timeouts: (Duration, Duration)) -> Result<(), String> {
        fetch(&self.team, &self.cache, &self.keys, timeouts)
    }

    /// Refreshes the set on a thread of its own: nothing waits for it.
    fn refresh_behind(&self) {
        let (team, cache, keys) = (self.team.clone(), self.cache.clone(), self.keys.clone());
        std::thread::spawn(move || {
            if let Err(error) = fetch(&team, &cache, &keys, BEHIND) {
                eprintln!(
                    "spindle serve: Cloudflare Access keys not refreshed (the kept ones still verify): {error}"
                );
            }
        });
    }

    /// Keeps a fetched key set (the team's whole set: a key it no longer lists is gone).
    #[cfg(test)]
    fn keep(&self, text: &str) -> Result<(), String> {
        keep(&self.cache, &self.keys, text)
    }

    /// The key for `kid`, refreshing the set when it is stale (behind this request) or
    /// does not have it (while it waits).
    fn key(&self, kid: &str) -> Option<(Vec<u8>, Vec<u8>)> {
        self.key_refreshed_by(kid, || self.refresh(INLINE), || self.refresh_behind())
    }

    /// `key`, with `inline` the fetch a request waits for and `behind` the one it does
    /// not (a test's stand-ins for Cloudflare). At most one of them starts per minute:
    /// the attempt is marked before either is called, under the same lock that saw it due.
    fn key_refreshed_by(
        &self,
        kid: &str,
        inline: impl FnOnce() -> Result<(), String>,
        behind: impl FnOnce(),
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        let (known, due) = {
            let mut keys = self.lock();
            let known = keys.rsa.get(kid).cloned();
            let stale = keys.fetched.is_none_or(|at| at.elapsed() > REFRESH);
            let may_retry = keys.attempted.is_none_or(|at| at.elapsed() > RETRY);
            let due = (known.is_none() || stale) && may_retry;
            if due {
                keys.attempted = Some(Instant::now());
            }
            (known, due)
        };
        if !due {
            return known;
        }
        if known.is_some() {
            // Stale, but the key is listed: it verifies now, and the set is brought up to
            // date behind this request (a key the team dropped is refused once it is).
            behind();
            return known;
        }
        if let Err(error) = inline() {
            eprintln!("spindle serve: Cloudflare Access keys not refreshed: {error}");
        }
        // The set as it is now: the team's current keys after a refresh, the keys it
        // had after a failed one.
        self.lock().rsa.get(kid).cloned()
    }

    /// Checks the request's Access token; the error says why it was refused.
    pub(super) fn check(&self, headers: &BTreeMap<String, String>) -> Result<Identity, String> {
        let token = headers
            .get("cf-access-jwt-assertion")
            .map(String::as_str)
            .or_else(|| {
                headers
                    .get("cookie")?
                    .split(';')
                    .find_map(|part| part.trim().strip_prefix("CF_Authorization="))
            })
            .ok_or("Cloudflare Access token required")?;
        self.verify(token.trim(), now())
    }

    fn verify(&self, token: &str, now: u64) -> Result<Identity, String> {
        let invalid = |why: &str| format!("Cloudflare Access token rejected: {why}");
        if token.len() > MAX_TOKEN {
            return Err(invalid("too long"));
        }
        let mut parts = token.split('.');
        let (Some(head), Some(body), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid("not a JWT"));
        };
        let json = |part: &str| -> Result<Value, String> {
            base64url(part)
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .ok_or_else(|| invalid("malformed"))
        };
        let header = json(head)?;
        if header["alg"] != "RS256" {
            return Err(invalid("not RS256"));
        }
        let kid = header["kid"].as_str().ok_or_else(|| invalid("no key id"))?;
        let (modulus, exponent) = self
            .key(kid)
            .ok_or_else(|| invalid("signed by a key the team does not list"))?;
        let signature = base64url(signature).ok_or_else(|| invalid("malformed signature"))?;
        let message = &token.as_bytes()[..head.len() + 1 + body.len()];
        ring::signature::RsaPublicKeyComponents {
            n: &modulus,
            e: &exponent,
        }
        .verify(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            message,
            &signature,
        )
        .map_err(|_| invalid("bad signature"))?;
        let claims = json(body)?;
        if claims["iss"].as_str() != Some(&format!("https://{}", self.team)) {
            return Err(invalid("issued by another team"));
        }
        let audience = match &claims["aud"] {
            Value::String(one) => vec![one.as_str()],
            Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        if !audience.contains(&self.audience.as_str()) {
            return Err(invalid("for another application"));
        }
        let expires = seconds(&claims["exp"]).ok_or_else(|| invalid("no expiry"))?;
        if expires.saturating_add(LEEWAY) < now {
            return Err(invalid("expired; reload to sign in again"));
        }
        for claim in ["nbf", "iat"] {
            if seconds(&claims[claim]).is_some_and(|at| at > now.saturating_add(LEEWAY)) {
                return Err(invalid(
                    "not valid yet (is this host's clock behind? compare `date -u`)",
                ));
            }
        }
        Ok(Identity {
            email: claims["email"].as_str().map(String::from),
            until: expires.saturating_add(LEEWAY),
        })
    }
}

/// A NumericDate claim in whole seconds: an integer, or a number with a fraction (which
/// RFC 7519 allows), never negative.
fn seconds(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|at| at.is_finite() && *at >= 0.0)
            .map(|at| at as u64)
    })
}

fn lock(keys: &Mutex<Keys>) -> std::sync::MutexGuard<'_, Keys> {
    keys.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Fetches team `team`'s keys within `timeouts` and keeps them in `keys` and `cache`.
fn fetch(
    team: &str,
    cache: &Path,
    keys: &Mutex<Keys>,
    (connect, total): (Duration, Duration),
) -> Result<(), String> {
    lock(keys).attempted = Some(Instant::now());
    let url = format!("https://{team}/cdn-cgi/access/certs");
    let text = reqwest::blocking::Client::builder()
        .connect_timeout(connect)
        .timeout(total)
        .build()
        .and_then(|client| client.get(&url).send())
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.text())
        .map_err(|error| format!("{url}: {error}"))?;
    keep(cache, keys, &text).map_err(|error| format!("{url} {error}"))
}

/// Keeps a fetched key set (the team's whole set: a key it no longer lists is gone).
fn keep(cache: &Path, keys: &Mutex<Keys>, text: &str) -> Result<(), String> {
    let rsa = parse_keys(text);
    if rsa.is_empty() {
        return Err("listed no RS256 keys".into());
    }
    // A cache: refetched when missing, but never left torn (refreshes can race).
    let _ = spindle_store::durable::write(cache, text.as_bytes());
    let mut keys = lock(keys);
    keys.rsa = rsa;
    keys.fetched = Some(Instant::now());
    Ok(())
}

pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// RS256 keys of a JWKS document, by kid.
fn parse_keys(text: &str) -> BTreeMap<String, (Vec<u8>, Vec<u8>)> {
    let Ok(document) = serde_json::from_str::<Value>(text) else {
        return BTreeMap::new();
    };
    document["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|key| key["kty"] == "RSA" && key["alg"].as_str().is_none_or(|alg| alg == "RS256"))
        .filter_map(|key| {
            Some((
                key["kid"].as_str()?.to_string(),
                (
                    base64url(key["n"].as_str()?)?,
                    base64url(key["e"].as_str()?)?,
                ),
            ))
        })
        .collect()
}

/// Base64url (padding optional).
fn base64url(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    // Leftover bits must be zero padding of a whole number of bytes.
    (bits < 6 && buffer == 0).then_some(out)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A throwaway 2048-bit RSA key (PKCS#1 DER) used only to sign test tokens.
    const TEST_KEY: [&str; 16] = [
        "MIIEowIBAAKCAQEAmzj4BWKV6zxgUgBlRb+zFRW4oikbRKADmOiUYMT0zsfOoAr1tpqU32DwjZY0TaAckulS583411nbsFKXe+gB",
        "lj9IlqpZGTBxJMTZ44Ii3Kxl12pCfPHaKYx/0G41fQHHPtFs2/SA46HPI44msidP2zGr7AGOvJK982Jm8XSdYF9PjaGaZ9xn47jf",
        "MoTgGUCSiYXTwlKAIW/LreiAKP9qBfOou73gaueG4CVasGTo9cOeF+XjsuBUJ7F0e09iTPKj9eN+R6q4r4ERwEWTpl3EVOCbpLwx",
        "p3qmMCT4vm38mW6JqnCTifVjtsYAGJLFgF6XPcnJrM45iwfbhkSMcNVoPQIDAQABAoIBAEDx1pAGPhzEz7YvlS/YM9kMFArBMTgG",
        "zUqCcWaFsbn8zp6Wx1TmG4jU2gEbifm6lon/VqFHRCjyX2FIRTpiCZvwEXJ8hrfr1UMhm9q9XdG/zgKR4oy9cl60+ttd6UJK5q8J",
        "V8c152GWl4DUXqtmmFppT3n48txCd3hoQRMgjCJARyC5d6KS2sYBIU8hBg/PO03EOTxw05UtQoqi/VRJb8Ig+7/ucn296kmHjQ7I",
        "ZQu+LW4M1oy0n2I79oW1OOgVFft5Ogcy8QuDA35oGBr1FWiQGaXm4tZ+mZRcJGdab66xgFoyOYp3Cobvy44WrNLHRaYp8KKiG3++",
        "Bxk6IX9ms18CgYEA2g+AHUxKYSNBIklSZBMWSjTEovobKtaUrI0+J3Qxxiqhpo57hU2VZT05vL/KrWwpzPw+KSWBltuICp/TNpWg",
        "e/cCnDunH7gjjrF9FhLxOPqDu8QoL2G9wNN39LQMyZxG/EhGmq/JP73BGFdw0tlkHA/rZN9QJbOKj+U+h4ce/NsCgYEAtjqjr7MU",
        "wszsRv04UVdQ+ZE3AKX9TyiYECrg+ORkN3Ryz9xcN7N1usbS47giNaLE0mYhmwwWS5g/YjINM3eEqfB2KrqghoJf0pQcZM6InB+2",
        "pMVASj2e6GMkx/7eqpswdV6Een72e0GGWtPOqopyTRU3SgRNZxHUkhJvotxGrscCgYB03iHXke+7J93ctldF80Ta+HT6s/ijeOWt",
        "pPVOx+N1RZvlXC+vPel3nNy44EGLuJCeoAHgo41UgMvkhcv5US/1wGjRutbTFSlZbJ6Rmg1WcEzx8StoWYYNNkjwCR9hXjRdOY89",
        "kEDXvKckmJiLKJ+SuqV6JoUZ76OvgzmEAVMGSQKBgQCu7K8/84nUTN3qbxxmiIjEaG8CAlScbf06V87JXIQO74temMs09OHoPv8l",
        "dRXxoqOIzF0p9npYanc9jYo6g1onvxnPfoKlW/UbsO1onxFnvNGYbXh/S5OEw9cE3FVmM75ajDZjWAjG3GlaOsPB1VBPSjReqq6a",
        "Z/VDmsrq7MQ09QKBgBkNcKzFETxfmSqTWXIOTfJPEkqAvXdu2YSwoKIt82vcyuXKXgJ5nxUe2KGwxoFLniga6jErnMeUEid5w834",
        "MblofI7Kj+EOj5sEbuK/HP3xYxm0n5yptPGMZvdDrK9ld4NpPMF9R6GojdVkKzDJ+i70JCDLm+eMb2aNNAMzORmh",
    ];

    fn encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
            for i in 0..=chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
        }
        out
    }

    fn standard(text: &str) -> Vec<u8> {
        base64url(&text.replace('+', "-").replace('/', "_")).unwrap()
    }

    struct Signer(ring::signature::RsaKeyPair);
    impl Signer {
        fn new() -> Self {
            let der = standard(&TEST_KEY.concat());
            Self(ring::signature::RsaKeyPair::from_der(&der).unwrap())
        }
        fn jwks(&self, kid: &str) -> String {
            use ring::signature::KeyPair;
            let public = self.0.public_key();
            let components: ring::signature::RsaPublicKeyComponents<Vec<u8>> = public.into();
            serde_json::json!({"keys": [{"kid": kid, "kty": "RSA", "alg": "RS256", "use": "sig",
                "n": encode(&components.n), "e": encode(&components.e)}]})
            .to_string()
        }
        fn token(&self, header: Value, claims: Value) -> String {
            let message = format!(
                "{}.{}",
                encode(header.to_string().as_bytes()),
                encode(claims.to_string().as_bytes())
            );
            let mut signature = vec![0; self.0.public().modulus_len()];
            self.0
                .sign(
                    &ring::signature::RSA_PKCS1_SHA256,
                    &ring::rand::SystemRandom::new(),
                    message.as_bytes(),
                    &mut signature,
                )
                .unwrap();
            format!("{message}.{}", encode(&signature))
        }
    }

    const TEAM: &str = "team-1234.cloudflareaccess.com";
    const AUD: &str = "f98bfb8b5d1f3b3baadb23806c29a1029e85ba5f86d3fa34c1242d525b1f971a";

    fn access(jwks: &str) -> Access {
        let keys = Keys {
            rsa: parse_keys(jwks),
            fetched: Some(Instant::now()),
            attempted: Some(Instant::now()),
        };
        Access {
            team: TEAM.into(),
            audience: AUD.into(),
            cache: PathBuf::from("/nonexistent/access-keys.json"),
            keys: Arc::new(Mutex::new(keys)),
        }
    }

    fn claims(now: u64) -> Value {
        serde_json::json!({
            "aud": [AUD], "email": "me@example.com", "exp": now + 3600, "iat": now - 10,
            "nbf": now - 10, "iss": format!("https://{TEAM}"), "type": "app",
            "identity_nonce": "n", "sub": "s", "country": "US"
        })
    }

    /// An Access that knows the test key, and a token it verifies that expires at `exp`
    /// (for the host's own tests).
    #[cfg(unix)]
    pub(in super::super) fn signed(exp: u64) -> (Access, String) {
        let signer = Signer::new();
        let header = serde_json::json!({"alg": "RS256", "kid": "k1", "typ": "JWT"});
        let mut claims = claims(super::now());
        claims["exp"] = serde_json::json!(exp);
        (access(&signer.jwks("k1")), signer.token(header, claims))
    }

    #[test]
    fn cloudflare_shaped_tokens_verify_and_every_mismatch_is_refused() {
        let signer = Signer::new();
        let access = access(&signer.jwks("k1"));
        let header = serde_json::json!({"alg": "RS256", "kid": "k1", "typ": "JWT"});
        let now = 1_800_000_000;
        let good = signer.token(header.clone(), claims(now));
        assert_eq!(
            access.verify(&good, now).unwrap(),
            Identity {
                email: Some("me@example.com".into()),
                until: now + 3600 + LEEWAY,
            }
        );
        // The header and the cookie both carry it.
        let current = signer.token(header.clone(), claims(super::now()));
        let cookie = BTreeMap::from([(
            "cookie".to_string(),
            format!("spindle=abc; CF_Authorization={current}"),
        )]);
        assert!(access.check(&cookie).is_ok());
        let assertion = BTreeMap::from([("cf-access-jwt-assertion".to_string(), current)]);
        assert!(access.check(&assertion).is_ok());
        assert_eq!(
            access.check(&BTreeMap::new()).unwrap_err(),
            "Cloudflare Access token required"
        );
        let refused = |token: String, why: &str| {
            let error = access.verify(&token, now).unwrap_err();
            assert!(error.contains(why), "{error} (expected {why})");
        };
        let mut other = claims(now);
        other["aud"] = serde_json::json!(["0".repeat(64)]);
        refused(signer.token(header.clone(), other), "another application");
        let mut single = claims(now);
        single["aud"] = serde_json::json!(AUD);
        assert!(
            access
                .verify(&signer.token(header.clone(), single), now)
                .is_ok()
        );
        // NumericDate claims may carry a fraction (RFC 7519).
        let mut fraction = claims(now);
        fraction["exp"] = serde_json::json!(now as f64 + 3600.5);
        fraction["iat"] = serde_json::json!(now as f64 - 10.25);
        assert!(
            access
                .verify(&signer.token(header.clone(), fraction), now)
                .is_ok()
        );
        let mut negative = claims(now);
        negative["exp"] = serde_json::json!(-1.5);
        refused(signer.token(header.clone(), negative), "no expiry");
        let mut team = claims(now);
        team["iss"] = serde_json::json!("https://evil.cloudflareaccess.com");
        refused(signer.token(header.clone(), team), "another team");
        let mut old = claims(now);
        old["exp"] = serde_json::json!(now - LEEWAY - 1);
        refused(signer.token(header.clone(), old), "expired");
        let mut early = claims(now);
        early["nbf"] = serde_json::json!(now + LEEWAY + 1);
        refused(signer.token(header.clone(), early), "not valid yet");
        refused(
            signer.token(
                serde_json::json!({"alg": "HS256", "kid": "k1"}),
                claims(now),
            ),
            "not RS256",
        );
        refused(
            signer.token(serde_json::json!({"alg": "none", "kid": "k1"}), claims(now)),
            "not RS256",
        );
        // A forged body under a real signature, and a token cut short.
        let (signed, signature) = good.rsplit_once('.').unwrap();
        let (head, _) = signed.split_once('.').unwrap();
        let mut forged = claims(now);
        forged["email"] = serde_json::json!("attacker@example.com");
        refused(
            format!(
                "{head}.{}.{signature}",
                encode(forged.to_string().as_bytes())
            ),
            "bad signature",
        );
        refused(signed.to_string(), "not a JWT");
        refused("a.b.c.d".into(), "not a JWT");
        // An unknown key id is refused (refreshing is rate-limited, so no fetch here).
        refused(
            signer.token(
                serde_json::json!({"alg": "RS256", "kid": "k2"}),
                claims(now),
            ),
            "does not list",
        );
    }

    #[test]
    fn a_refreshed_set_drops_a_key_the_team_no_longer_lists() {
        let signer = Signer::new();
        let dir = std::env::temp_dir().join(format!("spindle-access-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut access = access(&signer.jwks("k1"));
        access.cache = dir.join("access-keys.json");
        let cached = |access: &Access| {
            let text = std::fs::read_to_string(&access.cache).unwrap();
            parse_keys(&text).into_keys().collect::<Vec<_>>()
        };
        let due = |access: &Access| {
            let mut keys = access.lock();
            keys.fetched = None;
            keys.attempted = None;
        };
        // What `refresh` does, given Cloudflare's answer (no fetch here, and the attempt
        // marked, so the lookups after it do not fetch either).
        let refresh = |access: &Access, answer: Result<String, &str>| {
            access.lock().attempted = Some(Instant::now());
            answer
                .map_err(String::from)
                .and_then(|text| access.keep(&text))
        };
        let never = || -> Result<(), String> { panic!("a known key is never fetched inline") };
        // Cloudflare unreachable: the keys already known still verify.
        due(&access);
        let offline = access.key_refreshed_by("k1", never, || {
            let _ = refresh(&access, Err("offline"));
        });
        assert!(offline.is_some());
        // Stale: the known key verifies this request while a refresh brings the set up
        // to date behind it; a key the refreshed set no longer lists is refused after.
        due(&access);
        let rotated = access.key_refreshed_by("k1", never, || {
            let _ = refresh(&access, Ok(signer.jwks("k2")));
        });
        assert!(
            rotated.is_some(),
            "the stale set still verified this request"
        );
        assert!(
            access.key("k1").is_none(),
            "refused once the set is current"
        );
        assert!(access.key("k2").is_some());
        assert_eq!(cached(&access), ["k2"], "and after a restart");
        // A set with no RS256 keys is not kept.
        due(&access);
        let empty = access.key_refreshed_by("k2", never, || {
            let _ = refresh(&access, Ok(r#"{"keys": []}"#.into()));
        });
        assert!(empty.is_some());
        assert_eq!(cached(&access), ["k2"]);
        // A key not yet known is fetched while its request waits, once a minute at most:
        // a second unknown key in that minute starts no fetch of either kind.
        due(&access);
        let fetched = access.key_refreshed_by(
            "k3",
            || refresh(&access, Ok(signer.jwks("k3"))),
            || panic!("an unknown key is not fetched behind"),
        );
        assert!(fetched.is_some());
        assert!(
            access
                .key_refreshed_by("k4", never, || panic!("no second fetch"))
                .is_none()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn kept_keys_verify_at_start_with_no_fetch_before_the_host_listens() {
        let signer = Signer::new();
        let root =
            std::env::temp_dir().join(format!("spindle-access-start-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("access-keys.json"), signer.jwks("k1")).unwrap();
        // A team no fetch could reach in time: `new` tries none, as keys are kept.
        let access = Access::new(TEAM, AUD, &root).unwrap();
        assert!(
            access.lock().attempted.is_none(),
            "no fetch before listening"
        );
        // Once a fetch was tried (by `new`, with none kept), the refresh after listening
        // does not try again at once (a try would mark its own time first).
        let tried = Instant::now();
        access.lock().attempted = Some(tried);
        access.refresh_kept();
        assert_eq!(access.lock().attempted, Some(tried), "no second try");
        // And a token signed by a kept key verifies meanwhile, with no fetch.
        let header = serde_json::json!({"alg": "RS256", "kid": "k1", "typ": "JWT"});
        let now = now();
        let token = signer.token(header, claims(now));
        assert!(access.verify(&token, now).is_ok(), "the kept keys verify");
        assert_eq!(access.lock().attempted, Some(tried), "no fetch to verify");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn configuration_and_base64_are_strict() {
        let root = std::env::temp_dir();
        assert!(Access::new("example.com", AUD, &root).is_err());
        assert!(Access::new(TEAM, "abc", &root).is_err());
        assert_eq!(base64url("AQAB").unwrap(), vec![1, 0, 1]);
        assert_eq!(base64url("AQAB==").unwrap(), vec![1, 0, 1]);
        assert!(base64url("AQ+B").is_none());
        assert!(base64url("AQ").is_some());
        assert!(base64url("AR").is_none(), "nonzero padding bits");
        assert!(parse_keys("not json").is_empty());
    }
}
