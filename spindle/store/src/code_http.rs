//! Operator HTTP grants for code mode. The host sends each request: the sandbox still
//! has no network, never sees a grant's credentials and cannot leave a grant's URL
//! prefix (no userinfo, exact scheme/host/port, path prefix on a segment boundary,
//! redirects followed only inside the same grant). An embedded app's grants are checked
//! the same way; the app sends the request (`answered` reads what it brought back).
use serde_json::{Map, Value, json};
#[cfg(feature = "agent")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "agent")]
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, String>;

const MAX_GRANTS: usize = 64;
const MAX_BODY: usize = 1024 * 1024;
/// A response body is truncated at this many bytes at most (its base64 form still fits
/// one journaled child reply).
const MAX_RESPONSE: u64 = 640 * 1024;
const DEFAULT_RESPONSE: u64 = 256 * 1024;
const MAX_TIMEOUT: u64 = 600;
const DEFAULT_TIMEOUT: u64 = 30;
#[cfg(feature = "agent")]
const MAX_REDIRECTS: usize = 5;
const MAX_HEADERS: usize = 32;
/// A reply's room for one result: the journal's limit less its envelope.
const REPLY_ROOM: usize = crate::code_mode::MAX_REPLY - 16 * 1024;
/// Response headers past this many (escaped) bytes are left out.
const HEADER_ROOM: usize = 64 * 1024;
const METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];
/// Framing and routing headers the host owns.
const RESERVED: [&str; 11] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "upgrade",
    "te",
    "trailer",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "expect",
];
/// Headers that would make a server treat a request as another method than the grant
/// allows; a script may not send them.
const METHOD_OVERRIDES: [&str; 3] = [
    "x-http-method-override",
    "x-http-method",
    "x-method-override",
];

fn grants(policy: &Value) -> &[Value] {
    policy["http"].as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn token(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// A value the HTTP client will send as is: no control bytes but tab (checked here, not
/// at send time, where a failure could not be told from one after sending).
fn header_value(value: &str) -> bool {
    // What the HTTP client's header values accept: visible bytes and tab.
    value.len() <= 8 * 1024
        && value
            .bytes()
            .all(|b| (b >= b' ' && b != 0x7f) || b == b'\t')
}

/// Parses an absolute http(s) URL into the form it is compared and sent in.
fn normalize(raw: &str) -> Result<url::Url> {
    let mut url = url::Url::parse(raw).map_err(|e| format!("invalid URL {raw:?}: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("URL scheme {} is not http or https", url.scheme()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs with user information are refused".into());
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("URL host required".into());
    }
    // Encoded dots, slashes, backslashes and NULs could be decoded into a different
    // path by the server than the one matched here.
    let lower = url.path().to_ascii_lowercase();
    if ["%2e", "%2f", "%5c", "%00"]
        .iter()
        .any(|bad| lower.contains(bad))
    {
        return Err("URL paths may not encode '.', '/', '\\' or NUL".into());
    }
    // Some servers drop a `;` path parameter before resolving dot segments, so `..;`
    // would climb out of the prefix matched here.
    if lower.split('/').any(|segment| {
        segment.starts_with('.') && (segment.contains(';') || segment.contains("%3b"))
    }) {
        return Err("URL path segments starting with '.' may not hold ';'".into());
    }
    url.set_fragment(None);
    Ok(url)
}

fn covers(grant: &url::Url, url: &url::Url) -> bool {
    let prefix = grant.path();
    grant.scheme() == url.scheme()
        && grant.host_str() == url.host_str()
        && grant.port_or_known_default() == url.port_or_known_default()
        && if prefix.ends_with('/') {
            url.path().starts_with(prefix)
        } else {
            url.path() == prefix || url.path().starts_with(&format!("{prefix}/"))
        }
}

fn methods(grant: &Value) -> Vec<String> {
    match grant["methods"].as_array() {
        Some(list) => list
            .iter()
            .filter_map(|m| m.as_str().map(str::to_ascii_uppercase))
            .collect(),
        None => vec!["GET".into()],
    }
}

/// "name (GET, POST https://host/prefix/)" for every grant, for refusals.
pub(crate) fn listing(policy: &Value) -> String {
    let list: Vec<String> = grants(policy)
        .iter()
        .map(|grant| {
            format!(
                "{} ({} {})",
                grant["name"].as_str().unwrap_or("?"),
                methods(grant).join(", "),
                grant["url"].as_str().unwrap_or("?")
            )
        })
        .collect();
    if list.is_empty() {
        "no HTTP endpoints are granted".into()
    } else {
        format!("HTTP grants: {}", list.join("; "))
    }
}

/// Checks the policy's `http` list when the policy is loaded.
pub(crate) fn validate(policy: &Value) -> Result<()> {
    let Some(list) = policy.get("http") else {
        return Ok(());
    };
    let list = list.as_array().ok_or("http grants must be an array")?;
    if list.len() > MAX_GRANTS {
        return Err(format!("at most {MAX_GRANTS} http grants"));
    }
    let mut names = std::collections::BTreeSet::new();
    for grant in list {
        let object = grant.as_object().ok_or("an http grant must be an object")?;
        if let Some(key) = object.keys().find(|key| {
            !matches!(
                key.as_str(),
                "name"
                    | "description"
                    | "url"
                    | "methods"
                    | "headers"
                    | "risk"
                    | "timeout_seconds"
                    | "max_response_bytes"
            )
        }) {
            return Err(format!("http grant has unknown field {key}"));
        }
        let name = grant["name"]
            .as_str()
            .filter(|name| token(name) && name.len() <= 64)
            .ok_or("http grant name must be 1-64 token characters")?;
        if !names.insert(name) {
            return Err(format!("duplicate http grant {name}"));
        }
        let url = normalize(grant["url"].as_str().ok_or("http grant url required")?)
            .map_err(|e| format!("http grant {name}: {e}"))?;
        if url.query().is_some() {
            return Err(format!(
                "http grant {name}: the url prefix may not hold a query"
            ));
        }
        if !matches!(grant["risk"].as_str(), Some("read" | "write" | "danger")) {
            return Err(format!(
                "http grant {name}: risk must be read, write or danger"
            ));
        }
        if let Some(list) = grant.get("methods") {
            let list = list
                .as_array()
                .filter(|list| !list.is_empty())
                .ok_or(format!(
                    "http grant {name}: methods must be a nonempty array"
                ))?;
            if list
                .iter()
                .any(|m| m.as_str().is_none_or(|m| !METHODS.contains(&m)))
            {
                return Err(format!(
                    "http grant {name}: methods must be among {}",
                    METHODS.join(", ")
                ));
            }
        }
        for (key, bound) in [
            ("timeout_seconds", MAX_TIMEOUT),
            ("max_response_bytes", MAX_RESPONSE),
        ] {
            if grant
                .get(key)
                .is_some_and(|v| v.as_u64().is_none_or(|v| v == 0 || v > bound))
            {
                return Err(format!("http grant {name}: {key} must be 1..={bound}"));
            }
        }
        if let Some(headers) = grant.get("headers") {
            let headers = headers
                .as_object()
                .ok_or(format!("http grant {name}: headers must be an object"))?;
            if headers.len() > MAX_HEADERS {
                return Err(format!("http grant {name}: at most {MAX_HEADERS} headers"));
            }
            for (header, value) in headers {
                if !token(header) || RESERVED.contains(&header.to_ascii_lowercase().as_str()) {
                    return Err(format!("http grant {name}: header {header} is not allowed"));
                }
                let valid = match value {
                    Value::String(text) => header_value(text),
                    Value::Object(secret) => {
                        let source = secret.keys().filter(|k| *k != "prefix").collect::<Vec<_>>();
                        source.len() == 1
                            && match source[0].as_str() {
                                "env" => secret["env"].as_str().is_some_and(|v| {
                                    !v.is_empty()
                                        && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                                }),
                                "credential" => secret["credential"].as_str().is_some_and(token),
                                _ => false,
                            }
                            && secret
                                .get("prefix")
                                .is_none_or(|p| p.as_str().is_some_and(header_value))
                    }
                    _ => false,
                };
                if !valid {
                    return Err(format!(
                        "http grant {name}: header {header} must be text or {{\"env\":VAR}} / {{\"credential\":PROVIDER}} with an optional prefix"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// The grant covering `url` with `method`, or a refusal naming every grant.
fn find<'a>(policy: &'a Value, method: &str, url: &url::Url) -> Result<&'a Value> {
    let mut covering = None;
    for grant in grants(policy) {
        let Ok(prefix) = normalize(grant["url"].as_str().unwrap_or_default()) else {
            continue;
        };
        if covers(&prefix, url) {
            if methods(grant).iter().any(|m| m == method) {
                return Ok(grant);
            }
            covering = Some(grant);
        }
    }
    Err(match covering {
        Some(grant) => format!(
            "HTTP grant {} does not allow {method}; {}",
            grant["name"].as_str().unwrap_or("?"),
            listing(policy)
        ),
        None => format!("no HTTP grant covers {method} {url}; {}", listing(policy)),
    })
}

/// Validates a script's `http(url, options)` and freezes it (no secret is resolved or
/// recorded here).
pub(crate) fn prepare(policy: &Value, input: &Value) -> Result<Value> {
    let object = input.as_object().ok_or("http input must be an object")?;
    if let Some(key) = object.keys().find(|key| {
        !matches!(
            key.as_str(),
            "url" | "method" | "headers" | "body" | "json" | "timeout"
        )
    }) {
        return Err(format!(
            "http option {key} is not supported (method, headers, body, json, timeout)"
        ));
    }
    let url = normalize(input["url"].as_str().ok_or("http url required")?)?;
    let method = input["method"]
        .as_str()
        .unwrap_or("GET")
        .to_ascii_uppercase();
    if !METHODS.contains(&method.as_str()) {
        return Err(format!("http method must be one of {}", METHODS.join(", ")));
    }
    let grant = find(policy, &method, &url)?;
    let injected: Vec<String> = grant["headers"]
        .as_object()
        .map(|h| h.keys().map(|k| k.to_ascii_lowercase()).collect())
        .unwrap_or_default();
    let mut headers = Map::new();
    if let Some(given) = input.get("headers").filter(|h| !h.is_null()) {
        let given = given.as_object().ok_or("http headers must be an object")?;
        if given.len() > MAX_HEADERS {
            return Err(format!("at most {MAX_HEADERS} http headers"));
        }
        for (name, value) in given {
            let lower = name.to_ascii_lowercase();
            let value = value.as_str().filter(|v| header_value(v)).ok_or(format!(
                "http header {name} must be text without line breaks"
            ))?;
            if !token(name) || RESERVED.contains(&lower.as_str()) {
                return Err(format!("http header {name} is set by the host"));
            }
            if METHOD_OVERRIDES.contains(&lower.as_str()) {
                return Err(format!(
                    "http header {name} would override the method; pass method instead"
                ));
            }
            if injected.contains(&lower) {
                return Err(format!(
                    "http header {name} is supplied by grant {}",
                    grant["name"].as_str().unwrap_or("?")
                ));
            }
            headers.insert(lower, json!(value));
        }
    }
    let body = match (
        input.get("body").filter(|b| !b.is_null()),
        input.get("json"),
    ) {
        (Some(_), Some(_)) => return Err("pass body or json, not both".into()),
        (Some(body), None) => Some(body.as_str().ok_or("http body must be text")?.to_string()),
        (None, Some(value)) => {
            headers
                .entry("content-type")
                .or_insert(json!("application/json"));
            Some(serde_json::to_string(value).map_err(|e| e.to_string())?)
        }
        (None, None) => None,
    };
    if body.as_ref().is_some_and(|b| b.len() > MAX_BODY) {
        return Err(format!("http body exceeds {} KiB", MAX_BODY / 1024));
    }
    let ceiling = grant["timeout_seconds"].as_u64().unwrap_or(DEFAULT_TIMEOUT);
    let timeout = match input.get("timeout").filter(|t| !t.is_null()) {
        None => ceiling,
        Some(t) => t
            .as_f64()
            .filter(|t| *t > 0.0)
            .map(|t| (t.ceil() as u64).min(ceiling))
            .ok_or("http timeout must be a positive number of seconds")?,
    };
    Ok(json!({
        "kind":"http",
        "grant":grant["name"],
        "method":method,
        "url":url.as_str(),
        "headers":headers,
        "body":body,
        "timeout_seconds":timeout,
        "budget":0,
    }))
}

fn fail(error: String, may_have_run: bool, unknown: bool) -> Value {
    let mut value = json!({"ok":false,"may_have_run":may_have_run,"error":error});
    if unknown {
        value["unknown"] = json!(true);
    }
    value
}

/// Header values for one send, and the secret parts among them.
#[cfg(feature = "agent")]
type Injected = (Vec<(String, String)>, Vec<String>);

/// A grant's headers with their secrets resolved; secrets never enter a journal or a
/// reply.
#[cfg(feature = "agent")]
fn secrets(grant: &Value) -> Result<Injected> {
    let mut out = vec![];
    let mut hidden = vec![];
    for (name, value) in grant["headers"].as_object().into_iter().flatten() {
        let resolved = match value {
            Value::String(text) => text.clone(),
            Value::Object(secret) => {
                let raw = if let Some(var) = secret.get("env").and_then(Value::as_str) {
                    std::env::var(var)
                        .ok()
                        .filter(|v| !v.trim().is_empty())
                        .ok_or(format!(
                            "grant header {name}: environment variable {var} is not set"
                        ))?
                } else {
                    let provider = secret
                        .get("credential")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    dext_provider_catalog::provider::resolve_runtime_provider(Some(provider), true)
                        .map(|resolved| resolved.api_key)
                        .ok()
                        .filter(|key| !key.trim().is_empty())
                        .ok_or(format!(
                            "grant header {name}: no Dext credential for {provider} (`spindle login {provider}`)"
                        ))?
                };
                hidden.push(raw.trim().to_string());
                format!(
                    "{}{}",
                    secret.get("prefix").and_then(Value::as_str).unwrap_or(""),
                    raw.trim()
                )
            }
            _ => return Err(format!("grant header {name} is invalid")),
        };
        if !header_value(&resolved) {
            return Err(format!(
                "grant header {name}: its value has control characters or exceeds 8 KiB; nothing was sent"
            ));
        }
        out.push((name.clone(), resolved));
    }
    Ok((out, hidden))
}

/// The forms a secret is commonly echoed in: as is, JSON-escaped (also with `\/`),
/// percent-encoded (either hex case) and base64 (standard and URL-safe, at each of the
/// three byte alignments it can have inside a longer encoding). Longest first.
#[cfg(feature = "agent")]
fn needles(secrets: &[String]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = vec![];
    for secret in secrets.iter().filter(|s| s.len() >= 4) {
        let escaped = serde_json::to_string(secret).unwrap_or_default();
        let escaped = escaped[1..escaped.len().saturating_sub(1).max(1)].to_string();
        let percent = |upper: bool| -> String {
            secret
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                        (b as char).to_string()
                    } else if upper {
                        format!("%{b:02X}")
                    } else {
                        format!("%{b:02x}")
                    }
                })
                .collect()
        };
        let mut forms = vec![
            secret.clone(),
            escaped.clone(),
            escaped.replace('/', "\\/"),
            percent(true),
            percent(false),
        ];
        for shift in 0..3 {
            let mut shifted = vec![0u8; shift];
            shifted.extend_from_slice(secret.as_bytes());
            let encoded = base64(&shifted);
            // Characters that depend only on the secret's own bytes.
            let start = if shift == 0 { 0 } else { 4 };
            let end = ((shift + secret.len()) / 3) * 4;
            if end >= start + 8 {
                let core = &encoded[start..end];
                forms.push(core.to_string());
                forms.push(core.replace('+', "-").replace('/', "_"));
            }
        }
        for form in forms {
            if form.len() >= 4 && !out.iter().any(|known| known == form.as_bytes()) {
                out.push(form.into_bytes());
            }
        }
    }
    out.sort_by_key(|needle| std::cmp::Reverse(needle.len()));
    out
}

/// Replaces every echo of a secret (`needles`) with `[redacted]`.
#[cfg(feature = "agent")]
fn scrub(bytes: &mut Vec<u8>, needles: &[Vec<u8>]) {
    for needle in needles {
        if bytes.len() < needle.len() {
            continue;
        }
        let mut out = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index..].starts_with(needle) {
                out.extend_from_slice(b"[redacted]");
                index += needle.len();
            } else {
                out.push(bytes[index]);
                index += 1;
            }
        }
        *bytes = out;
    }
}

#[cfg(feature = "agent")]
fn scrub_text(text: &str, needles: &[Vec<u8>]) -> String {
    let mut bytes = text.as_bytes().to_vec();
    scrub(&mut bytes, needles);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Bytes `ch` takes in a JSON string (serde_json's escapes).
fn escaped_len(ch: char) -> usize {
    match ch {
        '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
        ch if (ch as u32) < 0x20 => 6,
        ch => ch.len_utf8(),
    }
}

/// The longest prefix of `text` that takes at most `room` bytes once escaped.
fn escaped_prefix(text: &str, room: usize) -> &str {
    let mut used = 0;
    for (index, ch) in text.char_indices() {
        used += escaped_len(ch);
        if used > room {
            return &text[..index];
        }
    }
    text
}

fn json_size(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

/// Puts the body into `data` so the whole reply stays inside one journaled result: JSON
/// that parses goes in once (as `json`), anything that would not fit is cut and marked
/// `truncated`. A response that arrived is never turned into an unknown outcome.
fn fit_body(data: &mut Value, bytes: Vec<u8>, json_body: bool) {
    match String::from_utf8(bytes) {
        Ok(text) => {
            if json_body
                && data["truncated"] == false
                && let Ok(parsed) = serde_json::from_str::<Value>(&text)
            {
                data["json"] = parsed;
                if json_size(data) <= REPLY_ROOM {
                    return;
                }
                data.as_object_mut().map(|object| object.remove("json"));
            }
            let room = REPLY_ROOM.saturating_sub(json_size(data) + 64);
            let kept = escaped_prefix(&text, room);
            if kept.len() < text.len() {
                data["truncated"] = json!(true);
            }
            data["body"] = json!(kept);
        }
        Err(error) => {
            let mut bytes = error.into_bytes();
            let room = REPLY_ROOM.saturating_sub(json_size(data) + 64);
            if bytes.len().div_ceil(3) * 4 > room {
                bytes.truncate(room / 4 * 3);
                data["truncated"] = json!(true);
            }
            data["body_base64"] = json!(base64(&bytes));
        }
    }
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(TABLE[((n >> shift) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The most of a response's body a grant's requests bring back.
#[cfg(feature = "embedded")]
pub(crate) fn response_limit(policy: &Value, grant: &str) -> u64 {
    grants(policy)
        .iter()
        .find(|known| known["name"] == grant)
        .and_then(|known| known["max_response_bytes"].as_u64())
        .unwrap_or(DEFAULT_RESPONSE)
}

/// A prepared request's result from what an embedded app brought back, worded as
/// `execute`'s: the response (`status`, `headers`, `body_base64`, `truncated`), or
/// `error` with `sent` (whether it may have left; yes unless false) and `answered` (the
/// server did answer, as with a redirect the app does not follow: a known outcome).
#[cfg(feature = "embedded")]
pub(crate) fn answered(prepared: &Value, answer: &Value) -> Value {
    if let Some(error) = answer["error"].as_str() {
        let sent = answer["sent"] != false;
        return fail(error.into(), sent, sent && answer["answered"] != true);
    }
    // A reply that does not read as a response: whether the request ran is not known.
    let Some(status) = answer["status"]
        .as_u64()
        .filter(|s| (100..=999).contains(s))
    else {
        return fail("the app's answer has no response status".into(), true, true);
    };
    let Some(mut bytes) = answer["body_base64"]
        .as_str()
        .map_or(Some(vec![]), crate::protocol::base64_decode)
    else {
        return fail("the app's response body is not base64".into(), true, true);
    };
    let limit = prepared["max_response_bytes"]
        .as_u64()
        .unwrap_or(DEFAULT_RESPONSE) as usize;
    let truncated = answer["truncated"] == true || bytes.len() > limit;
    bytes.truncate(limit);
    let mut headers = Map::new();
    let mut header_bytes = 0;
    let mut headers_cut = false;
    for (name, value) in answer["headers"].as_object().into_iter().flatten().take(64) {
        let (Some(text), true) = (value.as_str(), token(name)) else {
            continue;
        };
        let mut end = text.len().min(8 * 1024);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let text = &text[..end];
        let size = name.len() + text.chars().map(escaped_len).sum::<usize>() + 8;
        if header_bytes + size > HEADER_ROOM {
            headers_cut = true;
            continue;
        }
        header_bytes += size;
        headers.insert(name.to_ascii_lowercase(), json!(text));
    }
    let json_body = headers
        .get("content-type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.contains("json"));
    let mut data = json!({
        "status": status,
        "url": prepared["url"],
        "headers": headers,
        "truncated": truncated,
        "redirects": 0,
    });
    if headers_cut {
        data["headers_truncated"] = json!(true);
    }
    if let Some(note) = answer["body_error"].as_str() {
        data["body_error"] = json!(note.chars().take(400).collect::<String>());
    }
    fit_body(&mut data, bytes, json_body);
    json!({"ok": true, "data": data})
}

#[cfg(feature = "agent")]
enum Stopped {
    Timeout,
    Interrupted,
}

/// Polls `future` until it finishes, the deadline passes or the script is stopped.
#[cfg(feature = "agent")]
async fn bounded<F: std::future::Future>(
    future: F,
    interrupt: &AtomicBool,
    deadline: Instant,
) -> std::result::Result<F::Output, Stopped> {
    let mut future = std::pin::pin!(future);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Stopped::Timeout);
        }
        match tokio::time::timeout(left.min(Duration::from_millis(50)), &mut future).await {
            Ok(output) => return Ok(output),
            Err(_) if interrupt.load(Ordering::SeqCst) => return Err(Stopped::Interrupted),
            Err(_) => {}
        }
    }
}

/// Sends a prepared request. Refusals before anything left the host are known and
/// say so; a failure after the request may have reached the server is an unknown
/// outcome (it blocks further external effects in the script).
#[cfg(feature = "agent")]
pub(crate) fn execute(
    policy: &Value,
    prepared: &Value,
    interrupt: &AtomicBool,
    remaining: Duration,
) -> Value {
    type Started = (Value, url::Url, Injected);
    let start = || -> Result<Started> {
        let url = normalize(prepared["url"].as_str().unwrap_or_default())?;
        let method = prepared["method"].as_str().unwrap_or("GET");
        let grant = find(policy, method, &url)?;
        if grant["name"] != prepared["grant"] {
            return Err("the HTTP grant changed; operator review required".into());
        }
        Ok((grant.clone(), url, secrets(grant)?))
    };
    let (grant, url, (injected, hidden)) = match start() {
        Ok(started) => started,
        Err(error) => return fail(error, false, false),
    };
    if interrupt.load(Ordering::SeqCst) || remaining.is_zero() {
        return fail("http call stopped before it was sent".into(), false, false);
    }
    let timeout = Duration::from_secs(
        prepared["timeout_seconds"]
            .as_u64()
            .unwrap_or(DEFAULT_TIMEOUT),
    )
    .min(remaining);
    let limit = grant["max_response_bytes"]
        .as_u64()
        .unwrap_or(DEFAULT_RESPONSE) as usize;
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(error.to_string(), false, false),
    };
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(timeout.min(Duration::from_secs(15)))
        .build()
    {
        Ok(client) => client,
        Err(error) => return fail(error.to_string(), false, false),
    };
    let needles = needles(&hidden);
    // A secret is scrubbed whole before anything is cut, so a cut never leaves part of
    // one: bodies are read this far past the limit first.
    let lookahead = needles.first().map_or(0, Vec::len);
    let deadline = Instant::now() + timeout;
    let failed = |error: String, may_have_run: bool, unknown: bool| {
        fail(scrub_text(&error, &needles), may_have_run, unknown)
    };
    runtime.block_on(async {
        let mut method = reqwest::Method::from_bytes(
            prepared["method"].as_str().unwrap_or("GET").as_bytes(),
        )
        .unwrap_or(reqwest::Method::GET);
        let mut body = prepared["body"].as_str().map(str::to_string);
        let mut url = url;
        let mut hops = 0;
        loop {
            let mut request = client.request(method.clone(), url.clone());
            for (name, value) in prepared["headers"].as_object().into_iter().flatten() {
                if body.is_none() && name == "content-type" {
                    continue;
                }
                request = request.header(name.as_str(), value.as_str().unwrap_or_default());
            }
            for (name, value) in &injected {
                request = request.header(name.as_str(), value.as_str());
            }
            if let Some(body) = &body {
                request = request.body(body.clone());
            }
            let sent = hops > 0;
            let response = match bounded(request.send(), interrupt, deadline).await {
                Ok(Ok(response)) => response,
                // Nothing of this request reached the server when it could not be built
                // or connected; after a redirect the request before it did run.
                Ok(Err(error)) if error.is_connect() || error.is_builder() => {
                    return if sent {
                        failed(format!("http redirect not followed: {} (the request before it ran)", without_url(&error)), true, false)
                    } else {
                        failed(format!("http request not sent: {}", without_url(&error)), false, false)
                    };
                }
                Ok(Err(error)) => {
                    return failed(
                        format!("http request failed after it may have been sent ({}); outcome unknown", without_url(&error)),
                        true,
                        true,
                    );
                }
                Err(Stopped::Timeout) => {
                    return fail("http request timed out; outcome unknown".into(), true, true);
                }
                Err(Stopped::Interrupted) => {
                    return fail("http request stopped; outcome unknown".into(), true, true);
                }
            };
            let status = response.status();
            if status.is_redirection()
                && let Some(location) = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
            {
                let next = url
                    .join(location)
                    .map_err(|e| e.to_string())
                    .and_then(|next| normalize(next.as_str()));
                let follow_method = match status.as_u16() {
                    303 => reqwest::Method::GET,
                    301 | 302 if method == reqwest::Method::POST => reqwest::Method::GET,
                    _ => method.clone(),
                };
                let inside = next.as_ref().ok().and_then(|next| {
                    find(policy, follow_method.as_str(), next)
                        .ok()
                        .filter(|g| g["name"] == grant["name"])
                        .map(|_| next.clone())
                });
                match inside {
                    Some(next) if hops < MAX_REDIRECTS => {
                        if follow_method != method {
                            body = None;
                        }
                        method = follow_method;
                        url = next;
                        hops += 1;
                        continue;
                    }
                    Some(_) => {
                        return fail(format!("http redirect limit ({MAX_REDIRECTS}) reached; the last request ran (HTTP {})", status.as_u16()), true, false);
                    }
                    None => {
                        return failed(
                            format!(
                                "HTTP {} redirect to {} leaves grant {}; not followed (the request itself ran); {}",
                                status.as_u16(),
                                next.map(|n| n.to_string()).unwrap_or_else(|e| e),
                                grant["name"].as_str().unwrap_or("?"),
                                listing(policy)
                            ),
                            true,
                            false,
                        );
                    }
                }
            }
            let mut headers = Map::new();
            let mut header_bytes = 0;
            let mut headers_cut = false;
            for (name, value) in response.headers().iter().take(64) {
                let mut bytes = value.as_bytes().to_vec();
                scrub(&mut bytes, &needles);
                bytes.truncate(8 * 1024);
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let size = name.as_str().len() + text.chars().map(escaped_len).sum::<usize>() + 8;
                if header_bytes + size > HEADER_ROOM {
                    headers_cut = true;
                    continue;
                }
                header_bytes += size;
                match headers.get_mut(name.as_str()) {
                    Some(Value::String(existing)) => {
                        existing.push_str(", ");
                        existing.push_str(&text);
                    }
                    _ => {
                        headers.insert(name.as_str().into(), json!(text));
                    }
                }
            }
            let json_body = headers
                .get("content-type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.contains("json"));
            let mut response = response;
            let mut bytes = vec![];
            let mut truncated = false;
            let mut note = None;
            loop {
                match bounded(response.chunk(), interrupt, deadline).await {
                    Ok(Ok(Some(chunk))) => {
                        bytes.extend_from_slice(&chunk);
                        if bytes.len() > limit + lookahead {
                            truncated = true;
                            break;
                        }
                    }
                    Ok(Ok(None)) => break,
                    Ok(Err(error)) => {
                        note = Some(format!("response body failed: {}", without_url(&error)));
                        break;
                    }
                    Err(_) => {
                        note = Some("response body cut off by the timeout or a stop".into());
                        break;
                    }
                }
            }
            scrub(&mut bytes, &needles);
            if bytes.len() > limit {
                bytes.truncate(limit);
                truncated = true;
            }
            let mut data = json!({
                "status":status.as_u16(),
                "url":scrub_text(url.as_str(), &needles),
                "headers":headers,
                "truncated":truncated || note.is_some(),
                "redirects":hops,
            });
            if headers_cut {
                data["headers_truncated"] = json!(true);
            }
            if let Some(note) = note {
                data["body_error"] = json!(scrub_text(&note, &needles));
            }
            fit_body(&mut data, bytes, json_body);
            return json!({"ok":true,"data":data});
        }
    })
}

/// reqwest errors name the URL; the URL is the script's own, but keep messages short.
#[cfg(feature = "agent")]
fn without_url(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    if let Some(url) = error.url() {
        text = text.replace(url.as_str(), "the URL");
    }
    text
}

#[cfg(all(test, feature = "agent"))]
mod tests {
    use super::*;

    fn policy() -> Value {
        json!({"version":1,"models":[],"tools":[],"http":[
            {"name":"api","url":"https://api.example.com/v1/","methods":["GET","POST"],"risk":"write","headers":{"Authorization":{"env":"SPINDLE_TEST_HTTP_TOKEN","prefix":"Bearer "},"Accept":"application/json"}},
            {"name":"exact","url":"http://127.0.0.1:9/hook","risk":"read"}
        ]})
    }

    #[test]
    fn urls_must_match_a_grant_prefix_exactly() {
        let policy = policy();
        validate(&policy).unwrap();
        let ok = |url: &str, method: &str| {
            prepare(&policy, &json!({"url":url,"method":method})).map(|p| p["grant"].clone())
        };
        assert_eq!(
            ok("https://api.example.com/v1/items?q=1", "POST").unwrap(),
            "api"
        );
        assert_eq!(
            ok("https://API.example.com:443/v1/x", "get").unwrap(),
            "api"
        );
        assert_eq!(ok("http://127.0.0.1:9/hook", "GET").unwrap(), "exact");
        assert_eq!(ok("http://127.0.0.1:9/hook/sub", "GET").unwrap(), "exact");
        for (url, method) in [
            ("https://api.example.com/v2/items", "GET"),
            ("https://api.example.com/v1/../admin", "GET"),
            ("https://api.example.com/v1/%2e%2e/admin", "GET"),
            ("https://api.example.com/v1/a%2Fb", "GET"),
            ("https://user:pw@api.example.com/v1/x", "GET"),
            ("https://api.example.com@evil.example/v1/x", "GET"),
            ("https://api.example.com.evil.example/v1/x", "GET"),
            ("http://api.example.com/v1/x", "GET"),
            ("https://api.example.com:8443/v1/x", "GET"),
            ("https://api.example.com/v1/x", "DELETE"),
            ("http://127.0.0.1:9/hooked", "GET"),
            ("http://127.0.0.1:9/hook", "POST"),
            ("file:///etc/passwd", "GET"),
        ] {
            let refused = ok(url, method).unwrap_err();
            assert!(
                refused.contains("api (GET, POST https://api.example.com/v1/)")
                    || refused.contains("refused")
                    || refused.contains("may not encode")
                    || refused.contains("scheme"),
                "{url}: {refused}"
            );
        }
        // `..;` is `..` to servers that drop path parameters first.
        for url in [
            "https://api.example.com/v1/..;/admin",
            "https://api.example.com/v1/..;x=1/admin",
            "https://api.example.com/v1/.;/x",
            "https://api.example.com/v1/..%3B/admin",
        ] {
            let refused = ok(url, "GET").unwrap_err();
            assert!(refused.contains("may not hold ';'"), "{url}: {refused}");
        }
        assert_eq!(
            ok("https://api.example.com/v1/items;v=2/.well", "GET").unwrap(),
            "api"
        );
    }

    #[test]
    fn scripts_cannot_set_injected_or_framing_headers() {
        let policy = policy();
        for headers in [
            json!({"authorization":"Bearer mine"}),
            json!({"Host":"evil"}),
            json!({"X-Bad":"a\r\nb"}),
            json!({"X-Bad":"a\u{1}b"}),
            json!({"X-Bad":"a\u{7f}b"}),
            json!({"X-HTTP-Method-Override":"DELETE"}),
            json!({"x-http-method":"DELETE"}),
            json!({"X-Method-Override":"PUT"}),
        ] {
            assert!(
                prepare(
                    &policy,
                    &json!({"url":"https://api.example.com/v1/x","headers":headers})
                )
                .is_err(),
                "{headers}"
            );
        }
        let prepared = prepare(
            &policy,
            &json!({"url":"https://api.example.com/v1/x","method":"POST","json":{"a":1},"headers":{"X-Trace":"t"}}),
        )
        .unwrap();
        assert_eq!(prepared["headers"]["content-type"], "application/json");
        assert_eq!(prepared["body"], "{\"a\":1}");
        assert!(!prepared.to_string().contains("SPINDLE_TEST_HTTP_TOKEN"));
    }

    #[test]
    fn grants_are_validated_when_the_policy_loads() {
        for bad in [
            json!({"http":[{"name":"a","url":"https://x/","risk":"read","headers":{"Host":"y"}}]}),
            json!({"http":[{"name":"a","url":"https://u@x/","risk":"read"}]}),
            json!({"http":[{"name":"a","url":"https://x/?q=1","risk":"read"}]}),
            json!({"http":[{"name":"a","url":"https://x/","risk":"maybe"}]}),
            json!({"http":[{"name":"a","url":"https://x/","risk":"read","methods":["TRACE"]}]}),
            json!({"http":[{"name":"a","url":"https://x/","risk":"read","headers":{"A":{"env":"X","file":"/y"}}}]}),
            json!({"http":[{"name":"a","url":"https://x/","risk":"read"},{"name":"a","url":"https://y/","risk":"read"}]}),
            json!({"http":[{"name":"a","url":"https://x/","risk":"read","timeout_seconds":601}]}),
            json!({"http":[{"name":"a","url":"https://x/","risk":"read","headers":{"A":"b\u{1}c"}}]}),
            json!({"http":[{"name":"a","url":"https://x/","risk":"read","headers":{"A":{"env":"X","prefix":"\u{7f}"}}}]}),
        ] {
            assert!(validate(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn secrets_are_scrubbed_and_base64_is_standard() {
        let mut bytes = b"token=Bearer s3cr3t-value; again s3cr3t-value".to_vec();
        scrub(&mut bytes, &needles(&["s3cr3t-value".into()]));
        assert_eq!(bytes, b"token=Bearer [redacted]; again [redacted]");
        assert_eq!(base64(b"hello!?"), "aGVsbG8hPw==");
    }

    #[test]
    fn transformed_echoes_of_a_secret_are_scrubbed_too() {
        let secret = "tok/en+val ue:12345~";
        let needles = needles(&[secret.into()]);
        let url_safe = |text: String| text.replace('+', "-").replace('/', "_");
        let forms = [
            secret.to_string(),
            secret.replace('/', "\\/"),
            "tok%2Fen%2Bval%20ue%3A12345~".into(),
            "tok%2fen%2bval%20ue%3a12345~".into(),
            base64(secret.as_bytes()),
            url_safe(base64(secret.as_bytes())),
            base64(format!("u:{secret}").as_bytes()),
            base64(format!("user:{secret}").as_bytes()),
            url_safe(base64(format!("bob:{secret}").as_bytes())),
        ];
        for form in &forms {
            let mut bytes = format!("{{\"echo\":\"{form}\"}}").into_bytes();
            scrub(&mut bytes, &needles);
            let text = String::from_utf8(bytes).unwrap();
            assert!(text.contains("[redacted]"), "{form}: {text}");
            // No run of the secret's own encoding is left beside the marker.
            for core in needles.iter() {
                assert!(
                    !text
                        .as_bytes()
                        .windows(core.len())
                        .any(|w| w == core.as_slice()),
                    "{form}: {text}"
                );
            }
        }
        // Short secrets are not hunted in every encoding (too many false matches).
        assert!(super::needles(&["ab".into()]).is_empty());
    }

    #[test]
    fn oversized_bodies_are_cut_to_fit_one_reply_and_marked() {
        let base =
            || json!({"status":200,"url":"http://x/","headers":{},"truncated":false,"redirects":0});
        // Parsed JSON is kept once, without its text.
        let mut data = base();
        fit_body(&mut data, br#"{"a":[1,2,3]}"#.to_vec(), true);
        assert_eq!(
            (data["json"]["a"][2].clone(), data.get("body")),
            (json!(3), None)
        );
        // Escaping six-fold: cut to fit, never past the reply.
        let mut data = base();
        fit_body(&mut data, "\u{1}".repeat(400 * 1024).into_bytes(), false);
        assert_eq!(data["truncated"], true);
        assert!(json_size(&data) <= REPLY_ROOM, "{}", json_size(&data));
        assert!(data["body"].as_str().unwrap().len() > 100 * 1024);
        // JSON too large to keep whole is returned as cut text.
        let big = json!({"items":vec!["\"quoted\""; 120 * 1024]}).to_string();
        let mut data = base();
        fit_body(&mut data, big.clone().into_bytes(), true);
        assert!(data.get("json").is_none() && data["truncated"] == true);
        assert!(big.starts_with(data["body"].as_str().unwrap()));
        assert!(json_size(&data) <= REPLY_ROOM);
        // Binary bodies stay whole base64 groups.
        let mut data = base();
        fit_body(&mut data, vec![0xff; 900 * 1024], false);
        assert_eq!(data["truncated"], true);
        assert_eq!(data["body_base64"].as_str().unwrap().len() % 4, 0);
        assert!(json_size(&data) <= REPLY_ROOM);
    }
}
