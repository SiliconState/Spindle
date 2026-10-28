//! Device pairing. The pairing key is never a session credential: pairing exchanges it
//! for a random per-device token, the cookie carries only that token, and host.sqlite
//! keeps only its SHA-256 with a label and created/last-seen times, so one screen can
//! be revoked (Settings > Host, or Forget pairing) without re-pairing the others.
use super::{Host, Result, err, hash, lock, now_ms, random};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const MAX_DEVICES: i64 = 32;
/// Cookie lifetime; an active device gets its cookie re-sent once a day.
const MAX_AGE: u64 = 30 * 24 * 3600;
const REISSUE: u64 = 24 * 3600 * 1000;

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

pub(super) fn set_cookie(token: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "Set-Cookie: spindle={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={MAX_AGE}{secure}\r\n"
    )
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

    /// The device behind a cookie, or None. A cookie that is the pairing key itself (set
    /// before device tokens existed) is accepted only to mint this screen's own token,
    /// which the reply sets in its place, so the phone does not need pairing again.
    pub(super) fn authenticate(
        &self,
        token: &str,
        agent: Option<&str>,
        secure: bool,
        extra: &mut String,
    ) -> Result<Option<String>> {
        if let Some((id, reissue)) = self.device(token)? {
            if reissue {
                *extra = set_cookie(token, secure);
            }
            return Ok(Some(id));
        }
        if !same(token.as_bytes(), self.token.as_bytes()) {
            return Ok(None);
        }
        let label = label(agent);
        let mut recent = lock(&self.legacy);
        // A page's first requests arrive together; they share one new token.
        let (id, minted) = match &*recent {
            Some((at, seen, id, minted))
                if at.elapsed() < Duration::from_secs(30) && *seen == label =>
            {
                (id.clone(), minted.clone())
            }
            _ => {
                let (id, minted) = self.pair(&format!("{label} (migrated)"))?;
                *recent = Some((Instant::now(), label, id.clone(), minted.clone()));
                (id, minted)
            }
        };
        *extra = set_cookie(&minted, secure);
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

// The host these tests open runs only on Unix.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn devices_pair_migrate_and_revoke() {
        assert!(same(b"abc", b"abc") && !same(b"abc", b"abd") && !same(b"abc", b"ab"));
        assert_eq!(
            cookie(Some(&"a=1; spindle=tok; b=2".to_string())),
            Some("tok")
        );
        assert_eq!(cookie(Some(&"spindle=".to_string())), None);
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
            host.authenticate("nonsense", None, false, &mut extra)
                .unwrap(),
            None
        );
        let (phone, token) = host.pair("iPhone · Safari").unwrap();
        assert_eq!(
            host.authenticate(&token, None, false, &mut extra).unwrap(),
            Some(phone.clone())
        );
        assert!(extra.is_empty(), "a fresh cookie is not re-sent");
        let stored: String = lock(&host.db)
            .query_row("SELECT hash FROM devices WHERE id=?1", [&phone], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored, hash(token.as_bytes()), "only the hash is stored");
        // The legacy cookie (the key itself) mints one token per screen, then that
        // screen authenticates with its own token.
        let key = host.token.clone();
        let migrated = host
            .authenticate(&key, Some("Chrome/1 Linux"), true, &mut extra)
            .unwrap()
            .unwrap();
        assert!(extra.contains("Secure") && !extra.contains(&key));
        let minted = extra
            .split("spindle=")
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let mut again = String::new();
        assert_eq!(
            host.authenticate(&key, Some("Chrome/1 Linux"), true, &mut again)
                .unwrap(),
            Some(migrated.clone()),
            "concurrent first requests share the minted token"
        );
        assert!(again.contains(&minted));
        assert_eq!(
            host.authenticate(&minted, None, true, &mut String::new())
                .unwrap(),
            Some(migrated.clone())
        );
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
            host.authenticate(&token, None, false, &mut String::new())
                .unwrap(),
            None,
            "a revoked screen must pair again"
        );
        assert!(host.paired(&migrated));
        drop(host);
        std::fs::remove_dir_all(root).unwrap();
    }
}
