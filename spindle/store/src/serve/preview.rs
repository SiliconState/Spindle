//! Static-site previews. `GET /api/preview/TOKEN/PATH` serves the files of one thread
//! revision, so a workspace's multi-file site (HTML with sibling CSS, scripts, images
//! and fonts) renders in the Files dialog and in its own tab.
//!
//! A preview document must never act with the host's authority. Every preview response
//! carries a policy that sandboxes it (never `allow-same-origin`): it runs in an opaque
//! origin, without the host's cookies, storage or service worker, and cannot navigate
//! or frame the host. Its fetch directives name only the token's own prefix, so scripts
//! can load sibling files and nothing else by default. A policy cannot keep a script from
//! sending what the page shows out through WebRTC, a resource hint or a navigation (the
//! browser looks the host up before refusing it), so every response also carries a
//! Connection-Allowlist (`allowlist`) naming the token's prefix (and GitHub, for a GitHub
//! grant; nothing at all without a grant, and for `check`): Chrome (152 and later) makes
//! no connection for the page, its frames or its workers, of any kind, that the list does
//! not name. Its scripts run only under a token minted to let them
//! (`allow-scripts allow-forms allow-modals`): the UI asks for one without the user where
//! the browser holds a preview to its allowlist (it asks `check`), and otherwise only
//! once the user lets that version of the page run. Without one the page shows as it is
//! without scripts (its `<noscript>` too), and its meta refreshes are taken out. An
//! explicit paired-host grant may add GitHub API connections and avatars for that token
//! only (letting scripts run too); this can disclose page data to those sites. No host
//! API path is granted, and forms never submit anywhere.
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
    reach: Reach,
}

/// What a token's pages may do: run their scripts (on the one page the user let, or on
/// every page of the revision), and reach GitHub too. A `contained` grant runs them only
/// where the browser keeps a running page offline (the UI asks for one where `check` says
/// so): its pages carry `GUARD`, and nothing but its pages runs scripts.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Reach {
    pub(super) scripts: Scripts,
    pub(super) github: bool,
    pub(super) contained: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) enum Scripts {
    #[default]
    None,
    /// That page only: a page it frames or links to shows without its scripts.
    Page(String),
    All,
}

impl Reach {
    /// Whether `page` (a path of the revision) runs its scripts under this grant.
    fn runs(&self, page: &str) -> bool {
        let named = match &self.scripts {
            Scripts::None => false,
            Scripts::Page(only) => only == page,
            Scripts::All => true,
        };
        named && (!self.contained || mime(page).starts_with("text/html"))
    }
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
        reach: Reach,
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
            reach,
        });
        Ok((token, TTL))
    }

    /// The thread and revision a live token grants, and what its pages may do.
    fn resolve(&self, token: &str) -> Option<(String, u64, Reach)> {
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
            .map(|grant| (grant.session.clone(), grant.revision, grant.reach.clone()))
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
/// that may frame a preview) and `token` the grant whose prefix bounds every load;
/// `scripts`: this response's page may run them (and with `github`, reach GitHub).
pub(super) fn policy(origin: &str, token: &str, scripts: bool, github: bool) -> String {
    let own = format!("{origin}{PREFIX}{token}/");
    if !scripts {
        return format!(
            "sandbox; default-src 'none'; script-src 'none'; style-src {own} 'unsafe-inline'; img-src {own} data: blob:; font-src {own} data:; media-src {own} data: blob:; connect-src 'none'; frame-src {own}; child-src {own}; worker-src 'none'; manifest-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors {origin}"
        );
    }
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

/// What a token's pages may connect to (their Connection-Allowlist): the token's own
/// files, and with `github`, GitHub's API and avatars. Chrome (152 and later) holds every
/// connection a page makes to it, those its policy cannot reach included (WebRTC,
/// resource hints, navigations), in every frame and worker it opens; other browsers
/// ignore it. `origin` and `token` are checked plain (`serve`).
pub(super) fn allowlist(origin: &str, token: &str, github: bool) -> String {
    let github = if github {
        r#" "https://api.github.com/*" "https://avatars.githubusercontent.com/*""#
    } else {
        ""
    };
    format!(r#"("{origin}{PREFIX}{token}/*"{github})"#)
}

/// Script that sets `held`: whether the browser holds this document to its
/// Connection-Allowlist. A connection made under one is given none of its ICE servers, as
/// under nothing else (Spindle for Browser's preview.js checks the same); asking sends
/// nothing, and without WebRTC there is no telling.
macro_rules! held {
    () => {
        "let held=false;try{const probe=new RTCPeerConnection({iceServers:[{urls:'stun:127.0.0.1:9'}]});held=probe.getConfiguration().iceServers.length===0;probe.close()}catch{}"
    };
}

/// `GET /api/preview/check`: a page that tells the page framing it whether this browser
/// holds a preview to its Connection-Allowlist, so that the UI runs a page's scripts in
/// its preview without asking only where it does. It holds and reads nothing.
pub(super) const CHECK: &str = "check";
const CHECK_PAGE: &str = concat!(
    "<!doctype html><meta charset=utf-8><title>Spindle preview check</title><script>",
    held!(),
    "parent.postMessage({spindleContained:held},'*')</script>"
);

/// Runs before anything else in a page of a `contained` grant: where the browser does not
/// keep it offline (a link opened in another browser, say), the rest of the page is taken
/// as text, so nothing in it runs, and the page says why.
const GUARD: &str = concat!(
    "<script>(()=>{",
    held!(),
    "if(!held)document.write('<p>This page runs only where the browser keeps it offline, which this one cannot.</p><plaintext hidden>')})();</script>"
);

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
        // The manifest alone (decoded once for a page's assets), never every size.
        if let Some(entry) = view.entry(&root, path)? {
            return view.object(&entry.object).map(Found::File);
        }
        Ok(
            if view.entry(&root, &format!("{path}/index.html"))?.is_some() {
                Found::Directory
            } else {
                Found::Missing
            },
        )
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
    if rest == CHECK && plain && request.method == "GET" {
        let policy = format!(
            "sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline'; frame-ancestors {origin}"
        );
        return http::preview_headers(
            stream,
            200,
            "text/html; charset=utf-8",
            "",
            Some(CHECK_PAGE.len()),
            &policy,
            http::NOWHERE,
        )
        .and_then(|()| stream.write_all(CHECK_PAGE.as_bytes()))
        .map_err(|e| Refused(500, err(e)));
    }
    let grant = host.previews.resolve(token).filter(|_| plain);
    // What the path names (a directory's own index.html), and whether its scripts run:
    // on the page the grant lets them on only.
    let wanted = if path.is_empty() || path.ends_with('/') {
        format!("{path}index.html")
    } else {
        path.to_string()
    };
    let scripts = grant
        .as_ref()
        .is_some_and(|(_, _, reach)| reach.runs(&wanted));
    let (policy, allowlist) = if let Some((_, _, reach)) = &grant {
        (
            policy(origin, token, scripts, reach.github),
            allowlist(origin, token, reach.github),
        )
    } else {
        (
            "sandbox; default-src 'none'; frame-ancestors 'none'".to_string(),
            http::NOWHERE.to_string(),
        )
    };
    let reply = |stream: &mut TcpStream, status: u16, mime: &str, body: &[u8], extra: &str| {
        http::preview_headers(
            stream,
            status,
            mime,
            extra,
            Some(body.len()),
            &policy,
            &allowlist,
        )
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
    let Some((session, revision, grant_reach)) = grant else {
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
            let contained = grant_reach.contained;
            let page = bundle(
                &html,
                &wanted,
                &format!("{PREFIX}{token}/"),
                read,
                scripts,
                contained,
            );
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
/// async/defer/module) and images (`<img src>`) of this revision inlined; with
/// `scripts` and `contained`, `GUARD` before all of it (right after a doctype). Without
/// `scripts` (its scripts may not run) there is no shim and no script, read or kept;
/// meta refreshes go either way (a page in a preview does not move it). A sandboxed
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
    scripts: bool,
    contained: bool,
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
    // Where the page proper starts (`lead`): past space, comments and `<?…>`, all a
    // doctype may follow without putting the page in quirks mode; and past its doctype
    // (`doctype`), if it has one there.
    let (lead, doctype) = {
        let mut start = 0;
        loop {
            start = lower.len()
                - lower[start..]
                    .trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n', '\x0c'])
                    .len();
            let rest = &lower[start..];
            let skipped = if rest.starts_with("<!--") {
                // A comment ends at `-->`, or at `--!>` as browsers also read it.
                [
                    rest.find("-->").map(|end| end + 3),
                    rest.find("--!>").map(|end| end + 4),
                ]
                .into_iter()
                .flatten()
                .min()
            } else if rest.starts_with("<?") {
                rest.find('>').map(|end| end + 1)
            } else {
                let doctype = rest
                    .starts_with("<!doctype")
                    .then(|| rest.find('>').map(|end| start + end + 1))
                    .flatten();
                break (start, doctype);
            };
            match skipped {
                Some(length) => start += length,
                None => break (start, None),
            }
        }
    };
    // The shim goes right after <head> (or <html>) of the page proper, else after its
    // doctype, else first of all.
    let from = doctype.unwrap_or(lead);
    let at = ["<head", "<html"]
        .iter()
        .find_map(|tag| {
            let start = from + lower[from..].find(tag)?;
            Some(start + lower[start..].find('>')? + 1)
        })
        .or(doctype)
        .unwrap_or(0);
    let mut i = 0;
    // The guard goes before anything that could run: right after a doctype (only space,
    // comments and `<?…>` come before one), else first of all.
    if scripts && contained {
        i = doctype.unwrap_or(0);
        out.push_str(&html[..i]);
        out.push_str(GUARD);
    }
    if at > i {
        out.push_str(&html[i..at]);
        i = at;
    }
    if scripts {
        out.push_str(SHIM);
    }
    while let Some(next) = lower[i..].find('<').map(|at| i + at) {
        out.push_str(&html[i..next]);
        let rest = &lower[next..];
        // Comments and raw-text elements are copied as they are.
        let raw_end = |close: &str| {
            lower[next..]
                .find(close)
                .map_or(html.len(), |end| next + end + close.len())
        };
        // A raw-text element ends at its end tag however it is spelled (`</SCRIPT >`):
        // where that tag starts, and one past it (the page's end both, without one).
        let raw_bounds = |name: &str| {
            let mut at = next;
            while let Some(found) = lower[at..].find(&format!("</{name}")) {
                let after = at + found + 2 + name.len();
                match lower.as_bytes().get(after) {
                    None => return (html.len(), html.len()),
                    Some(b'>' | b'/' | b' ' | b'\t' | b'\n' | b'\r' | b'\x0c') => {
                        let end = lower[after..]
                            .find('>')
                            .map_or(html.len(), |end| after + end + 1);
                        return (at + found, end);
                    }
                    Some(_) => at = after,
                }
            }
            (html.len(), html.len())
        };
        let raw_close = |name: &str| raw_bounds(name).1;
        if rest.starts_with("<!--") {
            // A comment ends at `-->`, or at `--!>` as browsers also read it.
            let end = raw_end("-->").min(raw_end("--!>"));
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
            // Without its scripts: none kept (an SVG's self-closed one is its tag alone).
            "script" if !scripts => {
                let end = raw_close("script");
                i = if end == html.len() && tag.ends_with("/>") {
                    tag_end
                } else {
                    end
                };
            }
            // No meta refresh, and no page's own say over DNS prefetching (the response
            // turns it off).
            "meta"
                if attributes(tag).get("http-equiv").is_some_and(|value| {
                    let value = value.trim();
                    value.eq_ignore_ascii_case("refresh")
                        || value.eq_ignore_ascii_case("x-dns-prefetch-control")
                }) =>
            {
                i = tag_end;
            }
            "script" => {
                let end = raw_close("script");
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
            // Text in a page, markup inside SVG or MathML (where `<p>` or `<meta>`, say,
            // would break out as a page's own element): no `<` in it is read as a tag
            // either way (`&lt;` and `\3c ` read back as `<` where it is text).
            "style" | "textarea" | "title" => {
                let (close, end) = raw_bounds(&name);
                let close = close.max(tag_end);
                out.push_str(tag);
                let text = &html[tag_end..close];
                out.push_str(&if name == "style" {
                    text.replace('<', "\\3c ")
                } else {
                    text.replace('<', "&lt;")
                });
                out.push_str(&html[close..end]);
                i = end;
            }
            // Hints that reach a host before anything asks (a preconnect looks a name
            // up and connects even in a sandbox without scripts): none here.
            "link"
                if attributes(tag).get("rel").is_some_and(|rel| {
                    rel.split_ascii_whitespace().any(|r| {
                        matches!(
                            r.to_ascii_lowercase().as_str(),
                            "preconnect"
                                | "dns-prefetch"
                                | "prefetch"
                                | "prerender"
                                | "preload"
                                | "modulepreload"
                        )
                    })
                }) =>
            {
                i = tag_end;
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

/// The end (one past `>`) of the tag starting at `start`, skipping quoted attribute
/// values (a quote starts one only right after `=`, as a browser reads a tag: a stray
/// quote elsewhere is part of a name).
fn tag_close(html: &str, start: usize) -> Option<usize> {
    let bytes = html.as_bytes();
    let mut quote = None;
    let mut after_equals = false;
    for (offset, byte) in bytes[start..].iter().enumerate() {
        match (quote, byte) {
            (Some(q), b) if *b == q => quote = None,
            (Some(_), _) => continue,
            (None, b'"' | b'\'') if after_equals => {
                quote = Some(*byte);
                continue;
            }
            (None, b'>') => return Some(start + offset + 1),
            _ => {}
        }
        match byte {
            b'=' => after_equals = true,
            b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' => {}
            _ => after_equals = false,
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
        let (token, ttl) = grants.mint("s", 3, Reach::default()).unwrap();
        assert!(token_ok(&token) && ttl <= Duration::from_secs(3600));
        assert_eq!(
            grants.resolve(&token),
            Some(("s".into(), 3, Reach::default()))
        );
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
        let scripted = Reach {
            scripts: Scripts::Page("site/index.html".into()),
            ..Reach::default()
        };
        assert!(scripted.runs("site/index.html") && !scripted.runs("site/other.html"));
        let later: Vec<String> = (0..MAX_GRANTS)
            .map(|n| {
                let reach = if n == 0 {
                    scripted.clone()
                } else {
                    Reach::default()
                };
                grants.mint("t", n as u64, reach).unwrap().0
            })
            .collect();
        assert_eq!(grants.resolve(&token), None, "the oldest was retired");
        assert_eq!(
            grants.resolve(&later[0]),
            Some(("t".into(), 0, scripted.clone()))
        );
        grants.forget("t");
        assert_eq!(
            grants.resolve(&later[1]),
            None,
            "a deleted thread's tokens go"
        );
        let (fresh, _) = grants.mint("s", 4, scripted.clone()).unwrap();
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
        let page = bundle(
            html,
            "site/index.html",
            "/api/preview/T/",
            files,
            true,
            false,
        );
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
            "<title>A &lt;b> title</title>",
        ] {
            assert!(page.contains(kept), "{kept}: {page}");
        }
        // A page without a head still gets the shim first, after any doctype.
        assert!(bundle("<p>hi</p>", "a.html", "/p/", |_| None, true, false).starts_with(SHIM));
        // A contained grant's page: the guard before anything that could run (after a
        // doctype), the shim after it; none where nothing runs anyway.
        let guarded = bundle(
            "<!doctype html><script>early()</script><html><head><title>t</title>",
            "a.html",
            "/p/",
            |_| None,
            true,
            true,
        );
        assert!(
            guarded.starts_with(&format!("<!doctype html>{GUARD}<script>early()</script>")),
            "{guarded}"
        );
        assert!(
            guarded.contains(&format!("<head>{SHIM}<title>")),
            "{guarded}"
        );
        assert!(GUARD.contains(held!()) && CHECK_PAGE.contains(held!()));
        // Comments and `<?…>` before the doctype are passed over to it (else the guard
        // first would put the page in quirks mode); anything else is not.
        for (page, head) in [
            (
                "<!-- (c) --> \n<?xml version='1.0'?><!DOCTYPE html><p>hi</p>",
                "<!-- (c) --> \n<?xml version='1.0'?><!DOCTYPE html>",
            ),
            ("<!--><!doctype html><p>hi</p>", "<!--><!doctype html>"),
            ("<p>x</p><!doctype html>", ""),
            ("<!-- open <!doctype html>", ""),
            // A `<head>` in a comment is no place for it; a comment can end at `--!>`.
            (
                "<!-- see <head> --><!doctype html><script>evil()</script>",
                "<!-- see <head> --><!doctype html>",
            ),
            (
                "<!-- a --!><script>evil()</script><!-- b --><!doctype html>",
                "",
            ),
        ] {
            let guarded = bundle(page, "a.html", "/p/", |_| None, true, true);
            assert!(guarded.starts_with(&format!("{head}{GUARD}")), "{guarded}");
        }
        // Text that SVG would read as markup (breaking out as the page's own elements)
        // reads as text everywhere, and renders as it did.
        let svg = bundle(
            "<svg><textarea><p><link rel=preconnect href=\"https://leak.example/\"></textarea><style>a::after{content:'<'}<meta http-equiv=refresh content=0></style><title>x<p>y</title></svg>",
            "a.html",
            "/p/",
            |_| None,
            false,
            false,
        );
        assert_eq!(
            svg,
            "<svg><textarea>&lt;p>&lt;link rel=preconnect href=\"https://leak.example/\"></textarea><style>a::after{content:'\\3c '}\\3c meta http-equiv=refresh content=0></style><title>x&lt;p>y</title></svg>"
        );
        assert!(!bundle("<p>hi</p>", "a.html", "/p/", |_| None, false, true).contains(GUARD));
        assert!(
            bundle(
                "<!DOCTYPE html><p>hi</p>",
                "a.html",
                "/p/",
                |_| None,
                true,
                false
            )
            .starts_with(&format!("<!DOCTYPE html>{SHIM}"))
        );
        // Its scripts not let run: no shim and no script, none of their files read; its
        // stylesheet and pictures still put in. A meta refresh goes either way.
        let read = std::cell::RefCell::new(vec![]);
        let quiet = bundle(
            &html.replace(
                "<title>",
                r#"<meta http-equiv="Refresh" content="0;url=https://elsewhere.example/?d=1"><meta charset="utf-8"><title>"#,
            ),
            "site/index.html",
            "/api/preview/T/",
            |path| {
                read.borrow_mut().push(path.to_string());
                files(path)
            },
            false,
            false,
        );
        let outside_comments = quiet.replace(r#"<!-- <script src="js/app.js"></script> -->"#, "");
        assert!(
            !quiet.contains(SHIM) && !outside_comments.contains("<script"),
            "{quiet}"
        );
        assert!(
            !quiet.contains("elsewhere.example") && quiet.contains(r#"<meta charset="utf-8">"#)
        );
        assert!(quiet.contains("<style>body{") && quiet.contains("data:image/png;base64,UE5H"));
        assert!(
            !read.borrow().iter().any(|path| path.ends_with(".js")),
            "{:?}",
            read.borrow()
        );
        let moved = bundle(
            r#"<meta http-equiv=refresh content="1;url=/x"><p>x</p>"#,
            "a.html",
            "/p/",
            |_| None,
            true,
            false,
        );
        assert!(!moved.contains("refresh"), "{moved}");
        // Hints that reach a host by themselves go; raw text ends however its end tag is
        // spelled; a stray quote is no attribute value; `--!>` ends a comment.
        let tricky = bundle(
            concat!(
                r#"<link rel="preconnect" href="https://x.example"><link rel=DNS-Prefetch href=//y.example>"#,
                r#"<link rel="stylesheet preload" href="https://z.example/s.css">"#,
                r#"<script>a()</SCRIPT ><p id=x">kept</p><!-- c --!><script>b()</script>"#,
                r#"<svg><script href="x"/></svg><p>after</p>"#,
            ),
            "a.html",
            "/p/",
            |_| None,
            false,
            false,
        );
        for gone in [
            "preconnect",
            "DNS-Prefetch",
            "preload",
            "<script",
            "a()",
            "b()",
        ] {
            assert!(!tricky.contains(gone), "{gone}: {tricky}");
        }
        for kept in [r#"<p id=x">kept</p>"#, "<p>after</p>", "<!-- c --!>"] {
            assert!(tricky.contains(kept), "{kept}: {tricky}");
        }
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
        // Scripts not let run: a sandbox without them, and no script source at all.
        let quiet = policy("https://host.example", "ab", false, false);
        assert!(
            quiet.starts_with("sandbox; default-src 'none'; script-src 'none';"),
            "{quiet}"
        );
        assert!(quiet.contains("connect-src 'none'") && quiet.contains("form-action 'none'"));
        let policy = policy("https://host.example", "ab", true, false);
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
        let connected = super::policy("https://host.example", "ab", true, true);
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
        // What its pages may connect to: its own files, and GitHub when granted.
        assert_eq!(
            allowlist("https://host.example", "ab", false),
            r#"("https://host.example/api/preview/ab/*")"#
        );
        assert_eq!(
            allowlist("https://host.example", "ab", true),
            r#"("https://host.example/api/preview/ab/*" "https://api.github.com/*" "https://avatars.githubusercontent.com/*")"#
        );
        assert_eq!(encode("a b/ü.html"), "a%20b/%C3%BC.html");
    }
}
