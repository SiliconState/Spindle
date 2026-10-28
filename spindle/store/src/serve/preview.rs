//! Static-site previews. `GET /api/preview/TOKEN/PATH` serves the files of one thread
//! revision, so a workspace's multi-file site (HTML with sibling CSS, scripts, images
//! and fonts) renders in the Files dialog and in its own tab.
//!
//! A preview document must never act with the host's authority. Every preview response
//! carries a policy that sandboxes it (`sandbox allow-scripts allow-forms
//! allow-modals`, never `allow-same-origin`): it runs in an opaque origin, without the host's cookies,
//! storage or service worker, and cannot navigate or frame the host. Its fetch
//! directives name only the token's own prefix, so scripts can load sibling files and
//! nothing else by default. An explicit paired-host grant may add GitHub API connections
//! and avatars for that token only; this can disclose page data to those sites. No
//! host API path is granted, and forms never submit anywhere.
//!
//! A sandboxed document's requests carry no SameSite cookie, so each one is authorized
//! by a token in its path instead: 256 random bits, bound to one thread and revision,
//! read-only, valid for `TTL`, at most `MAX_GRANTS` outstanding, held only in memory
//! (a restart revokes them all) and dropped with its thread. Minting one takes a paired
//! device and the same-origin header like every other POST. Through the public origin
//! the Cloudflare Access check still applies to every preview request: the edge passes
//! a sandboxed document's subrequests only if they carry the `CF_Authorization` cookie,
//! which a browser sends from an opaque origin when the Access application's cookie is
//! SameSite=None (Cloudflare's default); see README.
use super::{Host, Refused, err, http, lock, random};
use sha2::{Digest, Sha256};
use spindle_store::view::View;
use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(super) const PREFIX: &str = "/api/preview/";
/// How long a token reads its revision; the Files dialog asks for a new one after.
const TTL: Duration = Duration::from_secs(30 * 60);
/// Tokens outstanding at once; minting another retires the oldest.
const MAX_GRANTS: usize = 32;

struct Grant {
    /// SHA-256 of the token: the token itself is never kept.
    key: [u8; 32],
    session: String,
    revision: u64,
    expires: Instant,
    github: bool,
}

#[derive(Default)]
pub(super) struct Grants(Mutex<VecDeque<Grant>>);

fn key(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn token_ok(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Grants {
    /// A fresh token for `revision` of `session`, and how long it lasts.
    pub(super) fn mint(
        &self,
        session: &str,
        revision: u64,
        github: bool,
    ) -> super::Result<(String, Duration)> {
        let token = random()?;
        let now = Instant::now();
        let mut grants = lock(&self.0);
        grants.retain(|grant| grant.expires > now);
        while grants.len() >= MAX_GRANTS {
            grants.pop_front();
        }
        grants.push_back(Grant {
            key: key(&token),
            session: session.into(),
            revision,
            expires: now + TTL,
            github,
        });
        Ok((token, TTL))
    }

    /// The thread and revision a live token grants.
    fn resolve(&self, token: &str) -> Option<(String, u64, bool)> {
        if !token_ok(token) {
            return None;
        }
        let wanted = key(token);
        let now = Instant::now();
        let mut grants = lock(&self.0);
        grants.retain(|grant| grant.expires > now);
        grants
            .iter()
            .find(|grant| grant.key == wanted)
            .map(|grant| (grant.session.clone(), grant.revision, grant.github))
    }

    /// Retires every token of a deleted thread.
    pub(super) fn forget(&self, session: &str) {
        lock(&self.0).retain(|grant| grant.session != session);
    }

    #[cfg(all(test, unix))]
    pub(super) fn age(&self, by: Duration) {
        for grant in lock(&self.0).iter_mut() {
            grant.expires = grant.expires.checked_sub(by).unwrap_or_else(Instant::now);
        }
    }
}

/// The content type a preview serves a path as (by extension; unknown types are
/// `application/octet-stream`, which `nosniff` keeps from running as a script or style).
pub(super) fn mime(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" | "md" | "csv" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
}

/// Every preview response's policy. `origin` is the host page's origin (the only page
/// that may frame a preview) and `token` the grant whose prefix bounds every load.
pub(super) fn policy(origin: &str, token: &str, github: bool) -> String {
    let own = format!("{origin}{PREFIX}{token}/");
    let connect = if github {
        " https://api.github.com"
    } else {
        ""
    };
    let images = if github {
        " https://avatars.githubusercontent.com"
    } else {
        ""
    };
    format!(
        "sandbox allow-scripts allow-forms allow-modals; default-src 'none'; script-src {own} 'unsafe-inline'; style-src {own} 'unsafe-inline'; img-src {own} data: blob:{images}; font-src {own} data:; media-src {own} data: blob:; connect-src {own}{connect}; frame-src {own}; child-src {own}; worker-src 'none'; manifest-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors {origin}"
    )
}

/// Percent-encodes a workspace path for a Location header (`/` kept).
fn encode(path: &str) -> String {
    path.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// What a preview path names in the revision.
enum Found {
    File(Vec<u8>),
    /// A directory with an index.html, asked for without its trailing slash.
    Directory,
    Missing,
}

fn find(host: &Host, session: &str, revision: u64, path: &str) -> super::Result<Found> {
    let mut view = View::open(&host.dir(session).join("store")).map_err(err)?;
    view.read(|view| {
        let Some((_, Some(root))) = view.at(revision)? else {
            return Ok(Found::Missing);
        };
        let files = view.files(&root)?;
        if let Some(file) = files.iter().find(|file| file.entry.path == path) {
            return view.object(&file.entry.object).map(Found::File);
        }
        let index = format!("{path}/index.html");
        Ok(if files.iter().any(|file| file.entry.path == index) {
            Found::Directory
        } else {
            Found::Missing
        })
    })
    .map_err(err)
}

/// Serves `GET /api/preview/TOKEN/PATH`. No device cookie is read: the token is the
/// whole authority, and an unknown, expired or forged one reads nothing.
pub(super) fn serve(
    host: &Host,
    stream: &mut TcpStream,
    request: &http::Request,
    origin: &str,
) -> std::result::Result<(), Refused> {
    let rest = request.path.strip_prefix(PREFIX).unwrap_or_default();
    let (token, path) = rest.split_once('/').unwrap_or((rest, ""));
    // The origin goes into the policy: only a plain scheme://host[:port].
    let plain = origin.split_once("://").is_some_and(|(scheme, authority)| {
        matches!(scheme, "http" | "https")
            && !authority.is_empty()
            && authority
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b':')
    });
    let grant = host.previews.resolve(token).filter(|_| plain);
    let policy = if let Some((_, _, github)) = &grant {
        policy(origin, token, *github)
    } else {
        "sandbox; default-src 'none'; frame-ancestors 'none'".to_string()
    };
    let reply = |stream: &mut TcpStream, status: u16, mime: &str, body: &[u8], extra: &str| {
        http::preview_headers(stream, status, mime, extra, Some(body.len()), &policy)
            .and_then(|()| stream.write_all(body))
            .map_err(|e| Refused(500, err(e)))
    };
    let missing = |stream: &mut TcpStream| {
        reply(
            stream,
            404,
            "text/plain; charset=utf-8",
            b"Not found. A preview link lasts 30 minutes and ends when the host restarts; open the file again from Files.",
            "",
        )
    };
    if request.method != "GET" {
        return reply(
            stream,
            405,
            "text/plain; charset=utf-8",
            b"Previews are read-only.",
            "",
        );
    }
    let Some((session, revision, _)) = grant else {
        return missing(stream);
    };
    if host.live(&session).is_err() {
        return missing(stream);
    }
    // The token alone: its directory, so relative URLs resolve under it.
    if !rest.contains('/') {
        let location = format!("Location: {PREFIX}{token}/\r\n");
        return reply(stream, 308, "text/plain; charset=utf-8", b"", &location);
    }
    let wanted = if path.is_empty() || path.ends_with('/') {
        format!("{path}index.html")
    } else {
        path.to_string()
    };
    if !spindle_store::workspace::valid_path(&wanted) {
        return missing(stream);
    }
    match find(host, &session, revision, &wanted).map_err(|e| Refused(500, e))? {
        // A page arrives whole (its own stylesheets, scripts and images inlined) with
        // a storage shim: see `bundle`.
        Found::File(bytes) if mime(&wanted).starts_with("text/html") => {
            let html = String::from_utf8_lossy(&bytes);
            let read = |path: &str| match find(host, &session, revision, path) {
                Ok(Found::File(bytes)) => Some(bytes),
                _ => None,
            };
            let page = bundle(&html, &wanted, &format!("{PREFIX}{token}/"), read);
            reply(
                stream,
                200,
                mime(&wanted),
                page.as_bytes(),
                "Content-Disposition: inline\r\n",
            )
        }
        Found::File(bytes) => reply(
            stream,
            200,
            mime(&wanted),
            &bytes,
            "Content-Disposition: inline\r\n",
        ),
        Found::Directory => {
            let location = format!("Location: {PREFIX}{token}/{}/\r\n", encode(&wanted));
            reply(stream, 308, "text/plain; charset=utf-8", b"", &location)
        }
        Found::Missing => missing(stream),
    }
}

/// Runs first in every previewed page. A sandboxed document has an opaque origin, where
/// `localStorage`, `sessionStorage` and `document.cookie` throw a SecurityError; pages
/// that save progress (most generated apps) would die at startup. Stand-ins keep them
/// in memory for the page's life instead. Nothing leaves the sandbox.
const SHIM: &str = "<script>(()=>{const mem=()=>{const m=new Map();return{getItem:k=>m.has(String(k))?m.get(String(k)):null,setItem:(k,v)=>{m.set(String(k),String(v))},removeItem:k=>{m.delete(String(k))},clear:()=>m.clear(),key:i=>[...m.keys()][i]??null,get length(){return m.size}}};for(const name of['localStorage','sessionStorage']){try{window[name].getItem('')}catch{try{Object.defineProperty(window,name,{value:mem(),configurable:true})}catch{}}}try{void document.cookie}catch{let c='';try{Object.defineProperty(document,'cookie',{get:()=>c,set:v=>{c=String(v).split(';')[0]},configurable:true})}catch{}}})();</script>";
/// The most a page's inlined files add, and the most one file may be.
const INLINED: usize = 16 * 1024 * 1024;
const INLINED_FILE: usize = 4 * 1024 * 1024;

/// A previewed page made whole: the shim first, then the page with its own stylesheets
/// (`<link rel=stylesheet href>`), classic scripts (`<script src>` without
/// async/defer/module) and images (`<img src>`) of this revision inlined. A sandboxed
/// page's subrequests carry no cookies, so behind Cloudflare Access (unless its cookie
/// is SameSite=None) they would reach the login page instead of the files; one request
/// for the page is all a preview then needs. URLs inside inlined stylesheets are
/// rewritten to the token's own prefix (`own`). What cannot be inlined (absolute or
/// missing URLs, modules, files past the caps) is left for the browser to load.
pub(super) fn bundle(
    html: &str,
    path: &str,
    own: &str,
    read: impl Fn(&str) -> Option<Vec<u8>>,
) -> String {
    let base = path.rsplit_once('/').map_or("", |(dir, _)| dir);
    let mut budget = INLINED;
    let mut fetch = |href: &str| -> Option<(String, Vec<u8>)> {
        let target = resolve(base, href)?;
        let bytes = read(&target)?;
        if bytes.len() > INLINED_FILE || bytes.len() > budget {
            return None;
        }
        budget -= bytes.len();
        Some((target, bytes))
    };
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() + SHIM.len());
    // The shim goes right after <head> (or <html>, or the doctype: anything before a
    // doctype would put the page in quirks mode), else first of all.
    let doctype = {
        let start = lower.len()
            - lower
                .trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n'])
                .len();
        lower[start..]
            .starts_with("<!doctype")
            .then(|| lower[start..].find('>').map(|end| start + end + 1))
            .flatten()
    };
    let at = ["<head", "<html"]
        .iter()
        .find_map(|tag| {
            let start = lower.find(tag)?;
            Some(start + lower[start..].find('>')? + 1)
        })
        .or(doctype)
        .unwrap_or(0);
    let mut i = 0;
    if at > 0 {
        out.push_str(&html[..at]);
        i = at;
    }
    out.push_str(SHIM);
    while let Some(next) = lower[i..].find('<').map(|at| i + at) {
        out.push_str(&html[i..next]);
        let rest = &lower[next..];
        // Comments and raw-text elements are copied as they are.
        let raw_end = |close: &str| {
            lower[next..]
                .find(close)
                .map_or(html.len(), |end| next + end + close.len())
        };
        if rest.starts_with("<!--") {
            let end = raw_end("-->");
            out.push_str(&html[next..end]);
            i = end;
            continue;
        }
        let Some(tag_end) = tag_close(html, next) else {
            out.push_str(&html[next..]);
            return out;
        };
        let tag = &html[next..tag_end];
        let name = tag[1..]
            .split(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match name.as_str() {
            "script" => {
                let end = raw_end("</script>");
                let attributes = attributes(tag);
                let classic = !["async", "defer", "nomodule"]
                    .iter()
                    .any(|flag| attributes.contains_key(*flag))
                    && attributes
                        .get("type")
                        .is_none_or(|kind| kind.is_empty() || kind.contains("javascript"));
                match attributes
                    .get("src")
                    .filter(|_| classic)
                    .and_then(|src| fetch(src))
                {
                    Some((_, bytes)) => {
                        out.push_str("<script>");
                        out.push_str(&escape(&String::from_utf8_lossy(&bytes), "</script"));
                        out.push_str("</script>");
                    }
                    None => out.push_str(&html[next..end]),
                }
                i = end;
            }
            "style" | "textarea" | "title" => {
                let end = raw_end(&format!("</{name}>"));
                out.push_str(&html[next..end]);
                i = end;
            }
            "link" => {
                let attributes = attributes(tag);
                let sheet = attributes
                    .get("rel")
                    .is_some_and(|rel| rel.split_ascii_whitespace().any(|r| r == "stylesheet"));
                match attributes
                    .get("href")
                    .filter(|_| sheet)
                    .and_then(|href| fetch(href))
                {
                    Some((target, bytes)) => {
                        let css = String::from_utf8_lossy(&bytes);
                        let dir = target.rsplit_once('/').map_or("", |(dir, _)| dir);
                        out.push_str("<style>");
                        out.push_str(&escape(&css_urls(&css, dir, own), "</style"));
                        out.push_str("</style>");
                    }
                    None => out.push_str(tag),
                }
                i = tag_end;
            }
            "img" => {
                let attributes = attributes(tag);
                match attributes.get("src").and_then(|src| {
                    let (target, bytes) = fetch(src)?;
                    let kind = mime(&target);
                    kind.starts_with("image/").then_some((kind, bytes))
                }) {
                    Some((kind, bytes)) => {
                        let data = format!("data:{kind};base64,{}", super::base64(&bytes));
                        out.push_str(&replace_attribute(tag, "src", &data));
                    }
                    None => out.push_str(tag),
                }
                i = tag_end;
            }
            _ => {
                out.push_str(tag);
                i = tag_end;
            }
        }
    }
    out.push_str(&html[i..]);
    out
}

/// The end (one past `>`) of the tag starting at `start`, skipping quoted attributes.
fn tag_close(html: &str, start: usize) -> Option<usize> {
    let bytes = html.as_bytes();
    let mut quote = None;
    for (offset, byte) in bytes[start..].iter().enumerate() {
        match (quote, byte) {
            (None, b'"' | b'\'') => quote = Some(*byte),
            (Some(q), b) if *b == q => quote = None,
            (None, b'>') => return Some(start + offset + 1),
            _ => {}
        }
    }
    None
}

/// A tag's attributes by lowercase name (values unquoted; a bare flag maps to "").
fn attributes(tag: &str) -> BTreeMap<String, String> {
    let inner = tag
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim_end_matches('/');
    let mut chars = inner.char_indices().peekable();
    // Skip the element name.
    while chars.peek().is_some_and(|(_, c)| !c.is_ascii_whitespace()) {
        chars.next();
    }
    let mut out = BTreeMap::new();
    loop {
        while chars.peek().is_some_and(|(_, c)| c.is_ascii_whitespace()) {
            chars.next();
        }
        let Some(&(start, _)) = chars.peek() else {
            return out;
        };
        while chars
            .peek()
            .is_some_and(|(_, c)| !c.is_ascii_whitespace() && *c != '=')
        {
            chars.next();
        }
        let end = chars.peek().map_or(inner.len(), |(at, _)| *at);
        let name = inner[start..end].to_ascii_lowercase();
        while chars.peek().is_some_and(|(_, c)| c.is_ascii_whitespace()) {
            chars.next();
        }
        let value = if chars.peek().is_some_and(|(_, c)| *c == '=') {
            chars.next();
            while chars.peek().is_some_and(|(_, c)| c.is_ascii_whitespace()) {
                chars.next();
            }
            match chars.peek().copied() {
                Some((at, quote @ ('"' | '\''))) => {
                    chars.next();
                    let mut end = inner.len();
                    for (offset, c) in chars.by_ref() {
                        if c == quote {
                            end = offset;
                            break;
                        }
                    }
                    inner[at + 1..end].to_string()
                }
                Some((at, _)) => {
                    let mut end = inner.len();
                    while let Some(&(offset, c)) = chars.peek() {
                        if c.is_ascii_whitespace() {
                            end = offset;
                            break;
                        }
                        chars.next();
                    }
                    inner[at..end].to_string()
                }
                None => String::new(),
            }
        } else {
            String::new()
        };
        if !name.is_empty() {
            out.entry(name).or_insert(value);
        }
    }
}

/// `tag` with attribute `name` set to `value` (double-quoted; the old value dropped).
fn replace_attribute(tag: &str, name: &str, value: &str) -> String {
    let lower = tag.to_ascii_lowercase();
    let mut search = 0;
    while let Some(found) = lower[search..].find(name).map(|at| search + at) {
        let before = lower[..found].chars().last().unwrap_or(' ');
        let after = lower[found + name.len()..].trim_start();
        if before.is_ascii_whitespace() && after.starts_with('=') {
            let eq = found + name.len() + (lower[found + name.len()..].len() - after.len());
            let rest = tag[eq + 1..].trim_start();
            let start = tag.len() - rest.len();
            let end = match rest.chars().next() {
                Some(quote @ ('"' | '\'')) => {
                    rest[1..].find(quote).map_or(tag.len(), |at| start + at + 2)
                }
                _ => rest
                    .find(|c: char| c.is_ascii_whitespace() || c == '>')
                    .map_or(tag.len(), |at| start + at),
            };
            return format!(
                "{} {name}=\"{}\"{}",
                tag[..found].trim_end(),
                value.replace('"', "&quot;"),
                &tag[end..]
            );
        }
        search = found + name.len();
    }
    tag.to_string()
}

/// A same-revision path for a relative URL under `base` (the document's directory):
/// None for absolute, scheme-relative, data: and other-scheme URLs, and for paths that
/// leave the workspace. Query strings and fragments are dropped; %XX is decoded.
fn resolve(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    let href = href.split(['?', '#']).next()?;
    if href.is_empty() || href.starts_with('/') || href.starts_with('\\') || href.contains(':') {
        return None;
    }
    let decoded = super::http::decode(href).ok()?;
    let mut parts: Vec<&str> = base.split('/').filter(|part| !part.is_empty()).collect();
    for part in decoded.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    let path = parts.join("/");
    spindle_store::workspace::valid_path(&path).then_some(path)
}

/// Stylesheet `url(...)`s made absolute under the token's prefix: inlined into the
/// page, the sheet's relative URLs would otherwise resolve from the page's directory.
fn css_urls(css: &str, dir: &str, own: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let lower = css.to_ascii_lowercase();
    let mut i = 0;
    while let Some(found) = lower[i..].find("url(").map(|at| i + at) {
        let open = found + 4;
        let Some(close) = css[open..].find(')').map(|at| open + at) else {
            break;
        };
        let raw = css[open..close].trim();
        let unquoted = raw.trim_matches(|c| c == '"' || c == '\'');
        out.push_str(&css[i..open]);
        match resolve(dir, unquoted) {
            Some(path) => out.push_str(&format!("\"{own}{}\"", encode(&path))),
            None => out.push_str(&css[open..close]),
        }
        out.push(')');
        i = close + 1;
    }
    out.push_str(&css[i..]);
    out
}

/// Text inlined into a raw-text element, with its closing sequence broken up (`<\/`),
/// which reads the same inside JavaScript strings, comments, regexes and CSS strings.
fn escape(text: &str, closing: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(found) = lower[i..].find(closing).map(|at| i + at) {
        out.push_str(&text[i..found]);
        out.push_str("<\\/");
        i = found + 2;
    }
    out.push_str(&text[i..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minting draws from the host's random source, which serve has only on Unix.
    #[cfg(unix)]
    #[test]
    fn tokens_are_scoped_bounded_and_expire() {
        let grants = Grants::default();
        let (token, ttl) = grants.mint("s", 3, false).unwrap();
        assert!(token_ok(&token) && ttl <= Duration::from_secs(3600));
        assert_eq!(grants.resolve(&token), Some(("s".into(), 3, false)));
        // Forged, malformed and near-miss tokens resolve to nothing.
        let mut near = token.clone().into_bytes();
        near[0] = if near[0] == b'0' { b'1' } else { b'0' };
        for forged in [
            String::from_utf8(near).unwrap(),
            "0".repeat(64),
            token[..63].to_string(),
            format!("{token}0"),
            "../".repeat(22),
        ] {
            assert_eq!(grants.resolve(&forged), None, "{forged}");
        }
        // Only MAX_GRANTS stay outstanding: the oldest gives way.
        let later: Vec<String> = (0..MAX_GRANTS)
            .map(|n| grants.mint("t", n as u64, n == 0).unwrap().0)
            .collect();
        assert_eq!(grants.resolve(&token), None, "the oldest was retired");
        assert_eq!(grants.resolve(&later[0]), Some(("t".into(), 0, true)));
        grants.forget("t");
        assert_eq!(
            grants.resolve(&later[1]),
            None,
            "a deleted thread's tokens go"
        );
        let (fresh, _) = grants.mint("s", 4, true).unwrap();
        grants.age(TTL + Duration::from_secs(1));
        assert_eq!(grants.resolve(&fresh), None, "expired");
        assert!(lock(&grants.0).is_empty(), "expired grants are dropped");
    }

    #[test]
    fn a_page_is_bundled_with_its_own_files_and_a_storage_shim() {
        let files = |path: &str| -> Option<Vec<u8>> {
            Some(
                match path {
                    "site/css/style.css" => "body{background:url('../img/bg.png')} .x{}",
                    "site/js/app.js" => "const s='</script>'; localStorage.getItem('k');",
                    "site/img/logo.png" => "PNG",
                    "site/mod.js" => "export const m = 1;",
                    _ => return None,
                }
                .as_bytes()
                .to_vec(),
            )
        };
        let html = r#"<!doctype html><html><head><title>A <b> title</title>
<link rel="stylesheet" href="css/style.css?v=2"><link rel="icon" href="img/logo.png">
</head><body data-x="a>b"><img alt="logo" src="img/logo.png">
<script src="js/app.js"></script><script type="module" src="mod.js"></script>
<script defer src="js/app.js"></script><script src="https://cdn.example/x.js"></script>
<script src="../escape.js"></script><!-- <script src="js/app.js"></script> -->
<script>let inline = 1;</script></body></html>"#;
        let page = bundle(html, "site/index.html", "/api/preview/T/", files);
        // The shim runs before anything else in the page.
        let shim = page.find(SHIM).unwrap();
        assert!(shim > page.find("<head>").unwrap() && shim < page.find("<title>").unwrap());
        // Its own stylesheet and classic script are inlined; URLs inside the sheet
        // point under the token, and `</script` inside the script cannot end it.
        assert!(page.contains(
            "<style>body{background:url(\"/api/preview/T/site/img/bg.png\")} .x{}</style>"
        ));
        assert!(
            page.contains("<script>const s='<\\/script>'; localStorage.getItem('k');</script>")
        );
        assert!(
            page.contains(r#"<img alt="logo" src="data:image/png;base64,UE5H">"#),
            "{page}"
        );
        // Left for the browser: icons, modules, deferred, absolute, escaping and
        // commented-out scripts; inline scripts and quoted '>' stay as they were.
        for kept in [
            r#"<link rel="icon" href="img/logo.png">"#,
            r#"<script type="module" src="mod.js"></script>"#,
            r#"<script defer src="js/app.js"></script>"#,
            r#"<script src="https://cdn.example/x.js"></script>"#,
            r#"<script src="../escape.js"></script>"#,
            r#"<!-- <script src="js/app.js"></script> -->"#,
            "<script>let inline = 1;</script>",
            r#"<body data-x="a>b">"#,
            "<title>A <b> title</title>",
        ] {
            assert!(page.contains(kept), "{kept}: {page}");
        }
        // A page without a head still gets the shim first, after any doctype.
        assert!(bundle("<p>hi</p>", "a.html", "/p/", |_| None).starts_with(SHIM));
        assert!(
            bundle("<!DOCTYPE html><p>hi</p>", "a.html", "/p/", |_| None)
                .starts_with(&format!("<!DOCTYPE html>{SHIM}"))
        );
        // Paths: relative only, never out of the workspace.
        assert_eq!(
            resolve("site", "./a/../b.js?x#y").as_deref(),
            Some("site/b.js")
        );
        assert_eq!(
            resolve("site", "a%20b.css").as_deref(),
            Some("site/a b.css")
        );
        for refused in [
            "/abs.js",
            "//cdn/x.js",
            "data:text/css,x",
            "../../up.js",
            "javascript:x",
        ] {
            assert_eq!(resolve("site", refused), None, "{refused}");
        }
    }

    #[test]
    fn types_and_policy() {
        for (path, expected) in [
            ("index.html", "text/html; charset=utf-8"),
            ("A/PAGE.HTM", "text/html; charset=utf-8"),
            ("site.css", "text/css; charset=utf-8"),
            ("app.js", "text/javascript; charset=utf-8"),
            ("mod.mjs", "text/javascript; charset=utf-8"),
            ("data.json", "application/json"),
            ("logo.svg", "image/svg+xml"),
            ("a.png", "image/png"),
            ("b.jpg", "image/jpeg"),
            ("c.jpeg", "image/jpeg"),
            ("d.gif", "image/gif"),
            ("e.webp", "image/webp"),
            ("favicon.ico", "image/x-icon"),
            ("f.woff2", "font/woff2"),
            ("notes.txt", "text/plain; charset=utf-8"),
            ("v1.2/README", "application/octet-stream"),
            ("script.sh", "application/octet-stream"),
        ] {
            assert_eq!(mime(path), expected, "{path}");
        }
        let policy = policy("https://host.example", "ab", false);
        assert!(policy.starts_with("sandbox allow-scripts allow-forms allow-modals;"));
        assert!(!policy.contains("allow-same-origin") && !policy.contains("allow-top-navigation"));
        for directive in [
            "default-src 'none'",
            "script-src https://host.example/api/preview/ab/ 'unsafe-inline'",
            "connect-src https://host.example/api/preview/ab/;",
            "form-action 'none'",
            "frame-ancestors https://host.example",
            "base-uri 'none'",
        ] {
            assert!(policy.contains(directive), "{directive}: {policy}");
        }
        assert!(!policy.contains("github"));
        let connected = super::policy("https://host.example", "ab", true);
        for directive in [
            "connect-src https://host.example/api/preview/ab/ https://api.github.com;",
            "img-src https://host.example/api/preview/ab/ data: blob: https://avatars.githubusercontent.com;",
            "script-src https://host.example/api/preview/ab/ 'unsafe-inline';",
            "worker-src 'none'",
            "form-action 'none'",
            "object-src 'none'",
        ] {
            assert!(connected.contains(directive), "{connected}");
        }
        assert!(!connected.contains("allow-same-origin") && !connected.contains('*'));
        assert_eq!(encode("a b/ü.html"), "a%20b/%C3%BC.html");
    }
}
