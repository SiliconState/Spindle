//! One-click connect: a client (Spindle for Browser) gets a browser token without one
//! ever passing through a URL. The client opens `GET /connect?client=NAME&challenge=C&
//! state=S` in a tab; a paired screen there (pairing first if it is not) shows who asks
//! and what it may do, and Allow or Deny goes to `POST /api/connect` (cookie and
//! same-origin rules, as every `/api` command). Allow answers with
//! `/connect/done?code=CODE&state=S`, which the client watches for; it trades the code
//! with its verifier at `POST /v1/connect/token` (PKCE S256: C is
//! base64url(SHA-256(verifier)), unpadded) for a browser token named NAME.
//!
//! A code is 256 random bits, kept only as its hash, in memory (a host restart loses
//! it: the client connects again), bound to NAME and C, good for 120 s and for one
//! trade: a wrong verifier burns it. Minting replaces NAME's earlier token in the same
//! transaction, so there is never a moment with two or none.
use super::v1::Failure;
use super::{Host, Refused, auth, err, hash, http, lock, now_ms, random};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// How long a code can be traded.
const CODE_LIFE: Duration = Duration::from_secs(120);
/// Codes outstanding at once (minting another retires the oldest).
const MAX_CODES: usize = 16;
/// A browser token's device id: `api:browser:NAME`.
pub(super) const BROWSER_PREFIX: &str = "api:browser:";

/// Outstanding codes by their SHA-256 (failed trades are counted by source in
/// `Host::failures`).
#[derive(Default)]
pub(super) struct Codes {
    codes: HashMap<String, Code>,
}
struct Code {
    client: String,
    challenge: String,
    at: Instant,
}

impl Codes {
    fn prune(&mut self) {
        self.codes.retain(|_, code| code.at.elapsed() < CODE_LIFE);
    }
    /// Ages every code by `by` (tests: expiry without waiting). Only the host tests use
    /// it, and they run on Unix: a Windows test build would see it unused.
    #[cfg(all(test, unix))]
    pub(super) fn age(&mut self, by: Duration) {
        for code in self.codes.values_mut() {
            code.at = code.at.checked_sub(by).unwrap_or(code.at);
        }
    }
}

/// A client's name: 1-64 characters, none a control character, no surrounding space.
pub(super) fn client_ok(name: &str) -> bool {
    (1..=64).contains(&name.chars().count())
        && name.trim() == name
        && !name.chars().any(char::is_control)
}

/// A PKCE S256 challenge: 43 base64url characters (an unpadded SHA-256).
fn challenge_ok(challenge: &str) -> bool {
    challenge.len() == 43
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The client's `state`: 1-128 of `A-Z a-z 0-9 - . _ ~` (sent back as it came).
fn state_ok(state: &str) -> bool {
    (1..=128).contains(&state.len())
        && state
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
}

/// A PKCE verifier: 43-128 of `A-Z a-z 0-9 - . _ ~`.
fn verifier_ok(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
}

/// base64url without padding.
pub(super) fn base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, byte)| n | u32::from(*byte) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// Text for HTML (element content and quoted attributes).
fn escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\'' => "&#39;".into(),
            c => c.to_string(),
        })
        .collect()
}

/// The query of a connect link, checked.
fn ask(query: &std::collections::BTreeMap<String, String>) -> Option<(&str, &str, &str)> {
    let client = query.get("client").filter(|c| client_ok(c))?;
    let challenge = query.get("challenge").filter(|c| challenge_ok(c))?;
    let state = query.get("state").filter(|s| state_ok(s))?;
    Some((client, challenge, state))
}

/// `GET /connect`: the approval page; a screen that is not paired pairs on it first.
pub(super) fn page(
    host: &Host,
    stream: &mut TcpStream,
    request: &http::Request,
    public: bool,
) -> std::result::Result<(), Refused> {
    let Some((client, challenge, state)) = ask(&request.query) else {
        let page = include_str!("../../ui/connect-done.html")
            .replace("{{TITLE}}", "This connect link is not valid.")
            .replace(
                "{{TEXT}}",
                "Start again from the app that asked to connect. You can close this tab.",
            );
        http::respond(stream, 400, "text/html; charset=utf-8", page.as_bytes(), "").map_err(err)?;
        return Ok(());
    };
    // A screen's cookie, not a token's: a runner or browser token approves nothing.
    let mut extra = String::new();
    let paired = match auth::cookie(request.headers.get("cookie")) {
        Some(token) => host
            .authenticate(token, public, &mut extra)?
            .is_some_and(|device| !super::v1::scoped(&device)),
        None => false,
    };
    let page = include_str!("../../ui/connect.html")
        .replace("{{CLIENT}}", &escape(client))
        .replace("{{CHALLENGE}}", challenge)
        .replace("{{STATE}}", state)
        .replace("{{PAIR}}", if paired { "hidden" } else { "" })
        .replace("{{ASK}}", if paired { "" } else { "hidden" });
    http::respond(
        stream,
        200,
        "text/html; charset=utf-8",
        page.as_bytes(),
        &extra,
    )
    .map_err(err)?;
    Ok(())
}

/// `GET /connect/done`: where an answer lands. It shows no secret; the client reads
/// the code from the address.
pub(super) fn done(
    stream: &mut TcpStream,
    request: &http::Request,
) -> std::result::Result<(), Refused> {
    let (title, text) = if request.query.contains_key("code") {
        ("Connected.", "You can close this tab.")
    } else {
        (
            "Not connected.",
            "Nothing was shared. You can close this tab.",
        )
    };
    let page = include_str!("../../ui/connect-done.html")
        .replace("{{TITLE}}", title)
        .replace("{{TEXT}}", text);
    http::respond(stream, 200, "text/html; charset=utf-8", page.as_bytes(), "").map_err(err)?;
    Ok(())
}

/// `POST /api/connect` (a paired screen): Allow mints a code for the client, Deny
/// says so; either way the answer is where the tab goes next. Allow for a name that is
/// connected already is refused (409) until it comes with `replace`.
pub(super) fn answer(host: &Host, data: &Value) -> std::result::Result<Value, Refused> {
    let text = |key: &str| data[key].as_str().unwrap_or_default();
    let (client, challenge, state) = (text("client"), text("challenge"), text("state"));
    if !client_ok(client) || !challenge_ok(challenge) || !state_ok(state) {
        return Err(super::bad("not a valid connect request"));
    }
    if data["allow"] != true {
        return Ok(json!({"redirect": format!("/connect/done?error=denied&state={state}")}));
    }
    // The name is all that tells clients apart: allowing one already connected hands
    // the asker that connection's threads. Said first; Allow again (`replace`) confirms.
    if data["replace"] != true {
        let db = lock(&host.db);
        let id = format!("{BROWSER_PREFIX}{client}");
        let connected: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM devices WHERE id=?1)",
                [&id],
                |row| row.get(0),
            )
            .map_err(err)?;
        if connected {
            let threads: i64 = db
                .query_row(
                    "SELECT count(*) FROM owners WHERE client=?1",
                    [&id],
                    |row| row.get(0),
                )
                .map_err(err)?;
            return Err(Refused(
                409,
                format!(
                    "{client} is already connected. Allowing replaces that connection: the app asking now gets its {threads} thread{} here, and the one connected now loses access. Press Replace only if both are the same app.",
                    if threads == 1 { "" } else { "s" }
                ),
            ));
        }
    }
    let code = random()?;
    let mut codes = lock(&host.connect);
    codes.prune();
    while codes.codes.len() >= MAX_CODES {
        let oldest = codes
            .codes
            .iter()
            .min_by_key(|(_, code)| code.at)
            .map(|(key, _)| key.clone());
        match oldest {
            Some(key) => codes.codes.remove(&key),
            None => break,
        };
    }
    codes.codes.insert(
        hash(code.as_bytes()),
        Code {
            client: client.into(),
            challenge: challenge.into(),
            at: Instant::now(),
        },
    );
    Ok(json!({"redirect": format!("/connect/done?code={code}&state={state}")}))
}

/// `POST /v1/connect/token` (no credential): a code and its verifier for the client's
/// browser token, minted (or replaced) now. `source` (auth.rs) is who asks: ten failed
/// trades a minute from one source, and it waits.
pub(super) fn trade(host: &Host, body: &[u8], source: &str) -> std::result::Result<Value, Failure> {
    let data: Value = serde_json::from_slice(body)
        .map_err(|_| Failure::new(400, "bad_request", "the body is JSON: {code, verifier}"))?;
    let (Some(code), Some(verifier)) = (data["code"].as_str(), data["verifier"].as_str()) else {
        return Err(Failure::new(
            400,
            "bad_request",
            "the body is JSON: {code, verifier}",
        ));
    };
    let refused = || {
        Failure::new(
            400,
            "bad_code",
            "unknown, used or expired code, or the wrong verifier; connect again",
        )
    };
    let found = {
        if host.failures.blocked(source) {
            return Err(Failure::new(
                429,
                "rate_limited",
                "too many failed connects; wait a minute",
            ));
        }
        let mut codes = lock(&host.connect);
        codes.prune();
        // Single use: taken out before anything is checked.
        let found = codes.codes.remove(&hash(code.as_bytes()));
        let good = found.as_ref().is_some_and(|found| {
            verifier_ok(verifier)
                && auth::same(
                    base64url(&Sha256::digest(verifier.as_bytes())).as_bytes(),
                    found.challenge.as_bytes(),
                )
        });
        if !good {
            drop(codes);
            host.failures.failed(source);
            return Err(refused());
        }
        found.expect("checked above")
    };
    let token = mint(&mut lock(&host.db), &found.client)
        .map_err(|why| Failure::new(409, "conflict", why))?;
    Ok(json!({"token": token, "client": found.client}))
}

/// Mints the browser token `name` (`api:browser:NAME`), revoking that name's earlier
/// one in the same transaction. Threads it brought stay its (ownership is by name).
/// A name is one token of any kind, and at most 16 tokens exist.
pub(super) fn mint(db: &mut Connection, name: &str) -> std::result::Result<String, String> {
    if !client_ok(name) {
        return Err("a browser token's name is 1-64 characters, no control characters".into());
    }
    let token = random()?;
    let id = format!("{BROWSER_PREFIX}{name}");
    let tx = db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(err)?;
    let other: i64 = tx
        .query_row(
            "SELECT count(*) FROM devices WHERE id IN (?1,?2)",
            [format!("api:{name}"), format!("api:run:{name}")],
            |row| row.get(0),
        )
        .map_err(err)?;
    if other != 0 {
        return Err(format!(
            "a token named `{name}` of another kind exists; revoke it first"
        ));
    }
    let replaced = tx
        .execute("DELETE FROM devices WHERE id=?1", [&id])
        .map_err(err)?;
    let count: i64 = tx
        .query_row(
            "SELECT count(*) FROM devices WHERE id LIKE 'api:%'",
            [],
            |row| row.get(0),
        )
        .map_err(err)?;
    if replaced == 0 && count >= super::v1::MAX_TOKENS {
        return Err(format!(
            "at most {} API tokens; revoke one first",
            super::v1::MAX_TOKENS
        ));
    }
    let now = now_ms() as i64;
    tx.execute(
        "INSERT INTO devices VALUES(?1,?2,?3,?4,?4,?4)",
        params![
            id,
            hash(token.as_bytes()),
            format!("Browser token {name}"),
            now
        ],
    )
    .map_err(err)?;
    tx.commit().map_err(err)?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_and_encodings() {
        // RFC 7636, appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            base64url(&Sha256::digest(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert!(client_ok("Spindle for Browser (Edge)") && client_ok("é"));
        for bad in ["", " lead", "trail ", "tab\there", &"x".repeat(65)] {
            assert!(!client_ok(bad), "{bad:?}");
        }
        assert!(challenge_ok("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
        assert!(!challenge_ok("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-c="));
        assert!(state_ok("a.b_c~d-e") && !state_ok("a b") && !state_ok("a&b"));
        assert!(verifier_ok(verifier) && !verifier_ok("short"));
        assert_eq!(
            escape(r#"<b a="1">'&'</b>"#),
            "&lt;b a=&quot;1&quot;&gt;&#39;&amp;&#39;&lt;/b&gt;"
        );
    }
}
