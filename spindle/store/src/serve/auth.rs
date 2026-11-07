//! Device pairing. The pairing key is never a session credential: pairing exchanges it
//! for a random per-device token, the cookie carries only that token, and host.sqlite
//! keeps only its SHA-256 with a label and created/last-seen times, so one screen can
//! be revoked (Settings > Host, or Forget pairing) without re-pairing the others.
use super::{Host, Result, err, hash, lock, now_ms, random};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_DEVICES: i64 = 32;
/// Cookie lifetime over HTTPS; an active device gets its cookie re-sent once a day.
const MAX_AGE: u64 = 30 * 24 * 3600;
const REISSUE: u64 = 24 * 3600 * 1000;

/// Wrong secrets one source may send per minute (a pairing key, a connect code and its
/// verifier), before `429 rate_limited`.
pub(super) const MAX_FAILURES: usize = 10;
/// Sources whose failures are remembered (the oldest-seen are forgotten past this).
const MAX_SOURCES: usize = 1024;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// Recent failed attempts by source, so one client guessing cannot lock the others out
/// (as a host-wide count would). Every secret checked here has 256 bits; the limit is a
/// second guard, not what keeps them.
#[derive(Default)]
pub(super) struct Failures(Mutex<BTreeMap<String, VecDeque<Instant>>>);

impl Failures {
    fn prune(sources: &mut BTreeMap<String, VecDeque<Instant>>) {
        sources.retain(|_, times| {
            while times
                .front()
                .is_some_and(|at| at.elapsed() >= FAILURE_WINDOW)
            {
                times.pop_front();
            }
            !times.is_empty()
        });
    }
    /// Whether `source` has used up its failures for now.
    pub(super) fn blocked(&self, source: &str) -> bool {
        let mut sources = super::lock(&self.0);
        Self::prune(&mut sources);
        sources
            .get(source)
            .is_some_and(|times| times.len() >= MAX_FAILURES)
    }
    /// Counts one failure of `source`.
    pub(super) fn failed(&self, source: &str) {
        let mut sources = super::lock(&self.0);
        Self::prune(&mut sources);
        if !sources.contains_key(source) && sources.len() >= MAX_SOURCES {
            let oldest = sources
                .iter()
                .min_by_key(|(_, times)| times.back().copied())
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                sources.remove(&oldest);
            }
        }
        sources
            .entry(source.to_string())
            .or_default()
            .push_back(Instant::now());
    }
    #[cfg(test)]
    pub(super) fn age(&self, by: Duration) {
        for times in super::lock(&self.0).values_mut() {
            for at in times.iter_mut() {
                *at = at.checked_sub(by).unwrap_or(*at);
            }
        }
    }
}

/// Who a request comes from, for `Failures`: through the tunnel, the address Cloudflare
/// saw (`Cf-Connecting-Ip`, which its edge sets whatever the client sent); otherwise
/// this computer. A process on this computer can name any source (a web page cannot:
/// the header needs a preflight, which the host never answers), so the limit slows
/// guesses through the tunnel; the secrets' 256 bits are what keep them.
pub(super) fn source(headers: &BTreeMap<String, String>) -> String {
    headers
        .get("cf-connecting-ip")
        .map(|ip| format!("ip:{}", ip.chars().take(64).collect::<String>()))
        .unwrap_or_else(|| "local".into())
}

/// Constant-time equality for secrets whose length is public.
pub(super) fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(super) fn cookie(header: Option<&String>) -> Option<&str> {
    header?
        .split(';')
        .find_map(|part| part.trim().strip_prefix("spindle="))
        .filter(|value| !value.is_empty())
}

/// The device cookie: kept 30 days over HTTPS (the public origin). A loopback cookie
/// is sent to every port of localhost, other servers' too, so it lasts only until the
/// browser closes.
pub(super) fn set_cookie(token: &str, secure: bool) -> String {
    let lasts = if secure {
        format!("; Max-Age={MAX_AGE}; Secure")
    } else {
        String::new()
    };
    format!("Set-Cookie: spindle={token}; HttpOnly; SameSite=Strict; Path=/{lasts}\r\n")
}

pub(super) fn clear_cookie(secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("Set-Cookie: spindle=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{secure}\r\n")
}

/// A short human label from the User-Agent, such as "iPhone · Safari".
pub(super) fn label(agent: Option<&str>) -> String {
    let agent = agent.unwrap_or_default();
    let find = |table: &[(&str, &'static str)]| {
        table
            .iter()
            .find(|(token, _)| agent.contains(token))
            .map(|(_, name)| *name)
    };
    let platform = find(&[
        ("iPhone", "iPhone"),
        ("iPad", "iPad"),
        ("Android", "Android"),
        ("CrOS", "ChromeOS"),
        ("Macintosh", "Mac"),
        ("Windows", "Windows"),
        ("Linux", "Linux"),
    ])
    .unwrap_or("Browser");
    match find(&[
        ("EdgiOS", "Edge"),
        ("Edg/", "Edge"),
        ("CriOS", "Chrome"),
        ("FxiOS", "Firefox"),
        ("Firefox/", "Firefox"),
        ("Chrome/", "Chrome"),
        ("Safari/", "Safari"),
    ]) {
        Some(browser) => format!("{platform} · {browser}"),
        None => platform.into(),
    }
}

impl Host {
    /// Mints a device token for a screen that presented the pairing key.
    pub(super) fn pair(&self, label: &str) -> Result<(String, String)> {
        let token = random()?;
        let id = random()?[..16].to_string();
        let now = now_ms() as i64;
        let db = lock(&self.db);
        db.execute(
            "INSERT INTO devices VALUES(?1,?2,?3,?4,?4,?4)",
            params![id, hash(token.as_bytes()), label, now],
        )
        .map_err(err)?;
        db.execute(
            // Named API tokens (`spindle token`, ids `api:NAME`) are never evicted.
            "DELETE FROM devices WHERE id NOT LIKE 'api:%' AND id NOT IN (SELECT id FROM devices WHERE id NOT LIKE 'api:%' ORDER BY seen DESC LIMIT ?1)",
            [MAX_DEVICES],
        )
        .map_err(err)?;
        Ok((id, token))
    }

    /// The device behind a cookie or bearer token, or None. Only a device token
    /// authenticates: the pairing key itself never does (before device tokens existed,
    /// until 2026-10-22, a cookie held the key; every such cookie lapsed within 30 days,
    /// and a screen that still sends one is asked to pair).
    pub(super) fn authenticate(
        &self,
        token: &str,
        secure: bool,
        extra: &mut String,
    ) -> Result<Option<String>> {
        let Some((id, reissue)) = self.device(token)? else {
            return Ok(None);
        };
        if reissue {
            *extra = set_cookie(token, secure);
        }
        Ok(Some(id))
    }

    /// Looks a token up by its hash. Last-seen is written at most once a minute; the
    /// boolean asks the caller to re-send the cookie (once a day) so it never lapses
    /// while the device is in use.
    fn device(&self, token: &str) -> Result<Option<(String, bool)>> {
        let db = lock(&self.db);
        let row: Option<(String, i64, i64)> = db
            .query_row(
                "SELECT id,seen,issued FROM devices WHERE hash=?1",
                [hash(token.as_bytes())],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(err)?;
        let Some((id, seen, issued)) = row else {
            return Ok(None);
        };
        let now = now_ms() as i64;
        let reissue = now - issued > REISSUE as i64;
        if now - seen > 60_000 || reissue {
            db.execute(
                "UPDATE devices SET seen=?2,issued=CASE WHEN ?3 THEN ?2 ELSE issued END WHERE id=?1",
                params![id, now, reissue],
            )
            .map_err(err)?;
        }
        Ok(Some((id, reissue)))
    }

    /// Whether a device is still paired (event streams re-check while they last).
    pub(super) fn paired(&self, id: &str) -> bool {
        lock(&self.db)
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM devices WHERE id=?1)",
                [id],
                |r| r.get(0),
            )
            .unwrap_or(false)
    }

    pub(super) fn devices(&self, current: &str) -> Result<Value> {
        let db = lock(&self.db);
        let mut statement = db
            .prepare("SELECT id,label,created,seen FROM devices ORDER BY seen DESC")
            .map_err(err)?;
        let rows = statement
            .query_map([], |r| {
                let id: String = r.get(0)?;
                Ok(json!({"id":id,"label":r.get::<_,String>(1)?,"created":r.get::<_,i64>(2)?,"seen":r.get::<_,i64>(3)?,"current":id == current}))
            })
            .map_err(err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(err)?;
        Ok(json!({ "devices": rows }))
    }

    pub(super) fn revoke(&self, id: &str) -> Result<bool> {
        Ok(lock(&self.db)
            .execute("DELETE FROM devices WHERE id=?1", [id])
            .map_err(err)?
            == 1)
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;

    #[test]
    fn failures_are_counted_per_source_and_expire() {
        let failures = Failures::default();
        for _ in 0..MAX_FAILURES {
            assert!(!failures.blocked("ip:a"));
            failures.failed("ip:a");
        }
        assert!(failures.blocked("ip:a"));
        assert!(
            !failures.blocked("ip:b"),
            "another source is not locked out"
        );
        assert!(!failures.blocked("local"));
        failures.age(FAILURE_WINDOW);
        assert!(!failures.blocked("ip:a"), "a minute later it may try again");
        // Remembered sources stay bounded.
        for n in 0..MAX_SOURCES + 10 {
            failures.failed(&format!("ip:{n}"));
        }
        assert!(lock(&failures.0).len() <= MAX_SOURCES);
        let through = BTreeMap::from([("cf-connecting-ip".to_string(), "203.0.113.9".to_string())]);
        assert_eq!(source(&through), "ip:203.0.113.9");
        assert_eq!(source(&BTreeMap::new()), "local");
    }
}

// The host these tests open runs only on Unix.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn devices_pair_and_revoke_and_the_key_is_no_credential() {
        assert!(same(b"abc", b"abc") && !same(b"abc", b"abd") && !same(b"abc", b"ab"));
        assert_eq!(
            cookie(Some(&"a=1; spindle=tok; b=2".to_string())),
            Some("tok")
        );
        assert_eq!(cookie(Some(&"spindle=".to_string())), None);
        // Over HTTPS a device stays paired for a month; on loopback, until the browser
        // closes (the cookie reaches every localhost port).
        let kept = set_cookie("tok", true);
        assert!(
            kept.contains("Max-Age=2592000") && kept.contains("Secure"),
            "{kept}"
        );
        let local = set_cookie("tok", false);
        assert!(
            !local.contains("Max-Age") && !local.contains("Expires"),
            "{local}"
        );
        assert!(local.contains("spindle=tok; HttpOnly; SameSite=Strict; Path=/"));
        assert_eq!(
            label(Some(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0) AppleWebKit Version/18.0 Mobile Safari/604.1"
            )),
            "iPhone · Safari"
        );
        let root =
            std::env::temp_dir().join(format!("spindle-host-devices-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let host = Host::open(root.clone(), None, None).unwrap();
        let mut extra = String::new();
        assert_eq!(
            host.authenticate("nonsense", false, &mut extra).unwrap(),
            None
        );
        let (phone, token) = host.pair("iPhone · Safari").unwrap();
        assert_eq!(
            host.authenticate(&token, false, &mut extra).unwrap(),
            Some(phone.clone())
        );
        assert!(extra.is_empty(), "a fresh cookie is not re-sent");
        let stored: String = lock(&host.db)
            .query_row("SELECT hash FROM devices WHERE id=?1", [&phone], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored, hash(token.as_bytes()), "only the hash is stored");
        // The pairing key itself is no screen's credential (a cookie from before device
        // tokens held it): such a screen pairs.
        let key = host.token.clone();
        let mut none = String::new();
        assert_eq!(host.authenticate(&key, true, &mut none).unwrap(), None);
        assert!(none.is_empty(), "and no token is minted for it");
        let (other, _) = host.pair("Linux · Chrome").unwrap();
        let listed = host.devices(&phone).unwrap();
        assert_eq!(listed["devices"].as_array().unwrap().len(), 2);
        assert!(
            listed["devices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["current"] == true && d["id"] == phone.as_str())
        );
        assert!(host.revoke(&phone).unwrap());
        assert!(!host.paired(&phone));
        assert_eq!(
            host.authenticate(&token, false, &mut String::new())
                .unwrap(),
            None,
            "a revoked screen must pair again"
        );
        assert!(host.paired(&other));
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
}
