//! Cloudflare Access. With a team configured, a request that arrived through the
//! public origin (the tunnel) must carry an Access token: the `Cf-Access-Jwt-Assertion`
//! header cloudflared adds (or the `CF_Authorization` cookie), an RS256 JWT signed by
//! one of the team's current keys, issued by the team, for this application's AUD tag,
//! and unexpired. It is a second lock beside device pairing: a stolen device cookie
//! alone does not get through the tunnel, and neither does any request if the Access
//! application is removed or the hostname is routed here without it. Loopback requests
//! (Host 127.0.0.1/localhost) are not affected; they still need a paired device.
//!
//! The team's keys are fetched from `https://TEAM/cdn-cgi/access/certs`, kept in
//! `access-keys.json` so a restart does not depend on reaching Cloudflare, refreshed
//! hourly and when a token names a key not yet known (at most once a minute).
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Clock difference tolerated for exp/nbf/iat: generous, because a laptop's (or WSL's)
/// clock can drift by minutes after sleep, and Access tokens live for hours.
const LEEWAY: u64 = 300;
const REFRESH: Duration = Duration::from_secs(3600);
const RETRY: Duration = Duration::from_secs(60);
const MAX_TOKEN: usize = 8192;

pub(super) struct Access {
    /// The team domain, e.g. `example.cloudflareaccess.com`.
    team: String,
    /// The application's AUD tag.
    audience: String,
    cache: PathBuf,
    keys: Mutex<Keys>,
}

#[derive(Default)]
struct Keys {
    /// kid -> (modulus, exponent), big-endian.
    rsa: BTreeMap<String, (Vec<u8>, Vec<u8>)>,
    fetched: Option<Instant>,
    attempted: Option<Instant>,
}

/// Who Access let through.
#[derive(Debug, PartialEq)]
pub(super) struct Identity {
    pub email: Option<String>,
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
            keys: Mutex::new(Keys::default()),
        };
        if let Ok(text) = std::fs::read_to_string(&access.cache) {
            access.lock().rsa = parse_keys(&text);
        }
        if let Err(error) = access.refresh()
            && access.lock().rsa.is_empty()
        {
            eprintln!("spindle serve: Cloudflare Access keys unavailable for now: {error}");
        }
        Ok(access)
    }

    pub(super) fn team(&self) -> &str {
        &self.team
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Keys> {
        self.keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Fetches the team's keys (never while holding the lock) and keeps them.
    fn refresh(&self) -> Result<(), String> {
        self.lock().attempted = Some(Instant::now());
        let url = format!("https://{}/cdn-cgi/access/certs", self.team);
        let text = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .and_then(|client| client.get(&url).send())
            .and_then(|response| response.error_for_status())
            .and_then(|response| response.text())
            .map_err(|error| format!("{url}: {error}"))?;
        self.keep(&text).map_err(|error| format!("{url} {error}"))
    }

    /// Keeps a fetched key set (the team's whole set: a key it no longer lists is gone).
    fn keep(&self, text: &str) -> Result<(), String> {
        let rsa = parse_keys(text);
        if rsa.is_empty() {
            return Err("listed no RS256 keys".into());
        }
        // A cache: refetched when missing, but never left torn (refreshes can race).
        let _ = spindle_store::durable::write(&self.cache, text.as_bytes());
        let mut keys = self.lock();
        keys.rsa = rsa;
        keys.fetched = Some(Instant::now());
        Ok(())
    }

    /// The key for `kid`, refreshing the set when it is stale or does not have it.
    fn key(&self, kid: &str) -> Option<(Vec<u8>, Vec<u8>)> {
        self.key_refreshed_by(kid, || self.refresh())
    }

    /// `key`, with `refresh` the fetch (a test's stand-in for Cloudflare).
    fn key_refreshed_by(
        &self,
        kid: &str,
        refresh: impl FnOnce() -> Result<(), String>,
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        let (known, stale, may_retry) = {
            let keys = self.lock();
            (
                keys.rsa.get(kid).cloned(),
                keys.fetched.is_none_or(|at| at.elapsed() > REFRESH),
                keys.attempted.is_none_or(|at| at.elapsed() > RETRY),
            )
        };
        if (known.is_none() || stale) && may_retry {
            if let Err(error) = refresh() {
                eprintln!("spindle serve: Cloudflare Access keys not refreshed: {error}");
            }
            // The set as it is now: the team's current keys after a refresh (never the
            // one it dropped), the keys it had after a failed one.
            return self.lock().rsa.get(kid).cloned();
        }
        known
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
        let expires = claims["exp"].as_u64().ok_or_else(|| invalid("no expiry"))?;
        if expires.saturating_add(LEEWAY) < now {
            return Err(invalid("expired; reload to sign in again"));
        }
        for claim in ["nbf", "iat"] {
            if claims[claim]
                .as_u64()
                .is_some_and(|at| at > now.saturating_add(LEEWAY))
            {
                return Err(invalid(
                    "not valid yet (is this host's clock behind? compare `date -u`)",
                ));
            }
        }
        Ok(Identity {
            email: claims["email"].as_str().map(String::from),
        })
    }
}

fn now() -> u64 {
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
mod tests {
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
            keys: Mutex::new(keys),
        }
    }

    fn claims(now: u64) -> Value {
        serde_json::json!({
            "aud": [AUD], "email": "me@example.com", "exp": now + 3600, "iat": now - 10,
            "nbf": now - 10, "iss": format!("https://{TEAM}"), "type": "app",
            "identity_nonce": "n", "sub": "s", "country": "US"
        })
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
                email: Some("me@example.com".into())
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
        // Cloudflare unreachable: the keys already known still verify.
        due(&access);
        let offline = access.key_refreshed_by("k1", || refresh(&access, Err("offline")));
        assert!(offline.is_some());
        // A refresh that succeeds with only k2: k1 is refused on that very lookup.
        due(&access);
        let rotated = access.key_refreshed_by("k1", || refresh(&access, Ok(signer.jwks("k2"))));
        assert!(rotated.is_none());
        assert!(access.key("k1").is_none(), "and after");
        assert!(access.key("k2").is_some());
        assert_eq!(cached(&access), ["k2"], "and after a restart");
        // A set with no RS256 keys is not kept.
        due(&access);
        let empty =
            access.key_refreshed_by("k2", || refresh(&access, Ok(r#"{"keys": []}"#.into())));
        assert!(empty.is_some());
        assert_eq!(cached(&access), ["k2"]);
        std::fs::remove_dir_all(&dir).unwrap();
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
