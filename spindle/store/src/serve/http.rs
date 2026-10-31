use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

pub(super) struct Request {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    /// The body read so far: after `read`, only bytes that arrived with the head;
    /// after `read_body`, all `length` bytes.
    pub body: Vec<u8>,
    length: usize,
}

/// Reads the request head (the body is read by `read_body`, once the request has
/// been authorized) within an overall deadline of 10 s, however a client spaces its
/// bytes: until its head is read, a connection (one of the host's 48) has passed no check.
pub(super) fn read(stream: &mut TcpStream) -> Result<Request, String> {
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    read_head(stream, Duration::from_secs(10))
}

fn read_head(stream: &mut TcpStream, total: Duration) -> Result<Request, String> {
    parse(
        stream,
        until(Instant::now() + total, "request head too slow"),
    )
}

/// Reads the rest of the body within an overall deadline: 10 s, or 2 minutes for an
/// attachment upload (8 MiB over a slow link). The buffer grows as bytes arrive.
pub(super) fn read_body(stream: &mut TcpStream, request: &mut Request) -> Result<(), String> {
    let total = if upload(&request.path) || request.path == "/v1/run" {
        Duration::from_secs(120)
    } else {
        Duration::from_secs(10)
    };
    finish(
        stream,
        request,
        until(Instant::now() + total, "request body too slow"),
    )
}

/// Before each read: a timeout for what is left until `deadline` (at most 10 s for one
/// read), or `slow` once nothing is.
fn until(
    deadline: Instant,
    slow: &'static str,
) -> impl FnMut(&mut TcpStream) -> Result<(), String> {
    move |stream| {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(slow.into());
        }
        stream
            .set_read_timeout(Some(left.min(Duration::from_secs(10))))
            .map_err(|e| e.to_string())
    }
}

fn finish<S: Read>(
    stream: &mut S,
    request: &mut Request,
    mut before_read: impl FnMut(&mut S) -> Result<(), String>,
) -> Result<(), String> {
    let mut chunk = vec![0u8; 64 * 1024];
    while request.body.len() < request.length {
        before_read(stream)?;
        let want = (request.length - request.body.len()).min(chunk.len());
        match stream.read(&mut chunk[..want]) {
            Ok(0) => return Err("incomplete body".into()),
            Ok(count) => request.body.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err("incomplete body".into()),
        }
    }
    Ok(())
}

const MAX_HEAD: usize = 16384;

fn upload(path: &str) -> bool {
    (path.starts_with("/api/sessions/") || path.starts_with("/v1/sessions/"))
        && path.ends_with("/upload")
}

/// `/v1` bodies that carry an app's data: an app tool's answer (up to 4 MiB of text,
/// and files), a state change (up to 1 MiB), or a command that may hold tool
/// definitions and state.
fn app_data(path: &str) -> bool {
    path == "/v1/commands"
        || (path.starts_with("/v1/sessions/")
            && (path.ends_with("/app_tool_result") || path.ends_with("/state")))
}

/// Reads up to the end of the head in chunks, with the same limits: 16 KiB of headers,
/// 64 KiB of body (8 MiB for an attachment upload or an app's data), no GET body. Bytes past the head
/// are the start of the body (never more than its declared length).
fn parse<S: Read>(
    stream: &mut S,
    mut before_read: impl FnMut(&mut S) -> Result<(), String>,
) -> Result<Request, String> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let end = loop {
        before_read(stream)?;
        let count = match stream.read(&mut chunk) {
            Ok(0) => return Err("incomplete request".into()),
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("incomplete request".into()),
        };
        let start = bytes.len();
        bytes.extend_from_slice(&chunk[..count]);
        let from = start.saturating_sub(3);
        if let Some(at) = bytes[from..].windows(4).position(|w| w == b"\r\n\r\n") {
            break from + at + 4;
        }
        if bytes.len() >= MAX_HEAD {
            return Err("headers too large".into());
        }
    };
    if end > MAX_HEAD {
        return Err("headers too large".into());
    }
    let text = std::str::from_utf8(&bytes[..end]).map_err(|_| "invalid headers")?;
    let mut lines = text.split("\r\n");
    let first: Vec<_> = lines.next().unwrap_or_default().split(' ').collect();
    if first.len() != 3
        || first[2] != "HTTP/1.1"
        || !matches!(first[0], "GET" | "POST")
        || !first[1].starts_with('/')
    {
        return Err("invalid request line".into());
    }
    let method = first[0].to_string();
    let (path, query) = first[1].split_once('?').unwrap_or((first[1], ""));
    let path = decode(path)?;
    let mut params = BTreeMap::new();
    // Query parts are form-encoded: `+` is a space (as Python's urlencode and
    // URLSearchParams send it); a literal plus arrives as %2B.
    let form = |part: &str| decode(&part.replace('+', " "));
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if params.insert(form(k)?, form(v)?).is_some() {
            return Err("duplicate query".into());
        }
    }
    let mut headers = BTreeMap::new();
    for line in lines.filter(|s| !s.is_empty()) {
        let (name, value) = line.split_once(':').ok_or("invalid header")?;
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.bytes().any(|b| b < 32 && b != 9)
        {
            return Err("invalid header".into());
        }
        if headers
            .insert(name.to_ascii_lowercase(), value.trim().to_string())
            .is_some()
        {
            return Err("duplicate header".into());
        }
    }
    if !headers.contains_key("host")
        || headers.contains_key("transfer-encoding")
        || headers.contains_key("expect")
    {
        return Err("unsupported framing".into());
    }
    let size = headers
        .get("content-length")
        .map(|v| v.parse::<usize>())
        .transpose()
        .map_err(|_| "invalid length")?
        .unwrap_or(0);
    // Attachment uploads carry a file, an app's answers and state its data; everything
    // else stays small.
    // A remote run carries its workspace (agent::runner::MAX_RUN_TREE, as base64).
    let limit = if path == "/v1/run" {
        64 * 1024 * 1024 + 64 * 1024
    } else if upload(&path) || app_data(&path) {
        8 * 1024 * 1024
    } else {
        64 * 1024
    };
    if size > limit || (method == "GET" && size != 0) {
        return Err("body too large".into());
    }
    let mut body = bytes.split_off(end);
    body.truncate(size);
    Ok(Request {
        method,
        path,
        query: params,
        headers,
        body,
        length: size,
    })
}

pub(super) fn decode(text: &str) -> Result<String, String> {
    let mut out = Vec::new();
    let mut bytes = text.bytes();
    while let Some(b) = bytes.next() {
        out.push(if b == b'%' {
            let a = bytes
                .next()
                .and_then(|b| (b as char).to_digit(16))
                .ok_or("invalid escape")?;
            let b = bytes
                .next()
                .and_then(|b| (b as char).to_digit(16))
                .ok_or("invalid escape")?;
            (a * 16 + b) as u8
        } else {
            b
        });
    }
    if out.iter().any(|b| *b < 32 || *b == 127) {
        return Err("invalid path".into());
    }
    String::from_utf8(out).map_err(|_| "invalid utf8".into())
}

/// The page's policy: only its own scripts, styles and connections; frames only from
/// itself (HTML previews, which carry their own sandboxing policy).
const PAGE_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; manifest-src 'self'; worker-src 'self'; frame-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

/// A workspace file shown alone in the preview frame (`render=1`: an SVG, or a page when
/// no preview link can be had): its inline styles show and nothing runs, in a sandbox
/// with an opaque origin (no cookies, storage or host API), with no network and nothing
/// loaded from anywhere but itself.
pub(super) const PREVIEW_POLICY: &str = "sandbox; default-src 'none'; style-src 'unsafe-inline'; script-src 'none'; img-src data:; font-src data:; frame-ancestors 'self'; base-uri 'none'; form-action 'none'";

pub(super) fn headers(
    stream: &mut TcpStream,
    status: u16,
    mime: &str,
    extra: &str,
    length: Option<usize>,
) -> std::io::Result<()> {
    headers_with(stream, status, mime, extra, length, PAGE_POLICY)
}

pub(super) fn headers_with(
    stream: &mut TcpStream,
    status: u16,
    mime: &str,
    extra: &str,
    length: Option<usize>,
    policy: &str,
) -> std::io::Result<()> {
    head(stream, status, mime, extra, length, policy, "same-origin")
}

/// A Connection-Allowlist that names nothing: a page under it (in Chrome 152 and later)
/// connects to nothing at all, WebRTC, resource hints and navigations included.
pub(super) const NOWHERE: &str = "()";

/// A static-site preview's headers (`preview.rs`): its own sandboxing policy, what it may
/// connect to (`allowlist`), and readable by the opaque origin of the sandboxed document
/// that asked for it (module scripts, fonts and fetches are CORS requests from `null`).
/// The token in the path is the authority; no credentials are involved.
pub(super) fn preview_headers(
    stream: &mut TcpStream,
    status: u16,
    mime: &str,
    extra: &str,
    length: Option<usize>,
    policy: &str,
    allowlist: &str,
) -> std::io::Result<()> {
    // No name looked up ahead of a fetch: a preconnect hint reaches its host even in a
    // sandbox without scripts (where the allowlist does not hold).
    let extra = format!(
        "Access-Control-Allow-Origin: *\r\nX-DNS-Prefetch-Control: off\r\nConnection-Allowlist: {allowlist}\r\n{extra}"
    );
    head(stream, status, mime, &extra, length, policy, "cross-origin")
}

fn head(
    stream: &mut TcpStream,
    status: u16,
    mime: &str,
    extra: &str,
    length: Option<usize>,
    policy: &str,
    resource: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {mime}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nCross-Origin-Resource-Policy: {resource}\r\nContent-Security-Policy: {policy}\r\nConnection: close\r\n{extra}"
    )?;
    if let Some(length) = length {
        write!(stream, "Content-Length: {length}\r\n")?;
    }
    write!(stream, "\r\n")
}

pub(super) fn respond(
    stream: &mut TcpStream,
    status: u16,
    mime: &str,
    bytes: &[u8],
    extra: &str,
) -> std::io::Result<()> {
    headers(stream, status, mime, extra, Some(bytes.len()))?;
    stream.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(mut bytes: &[u8]) -> Result<Request, String> {
        let mut request = parse(&mut bytes, |_| Ok(()))?;
        finish(&mut bytes, &mut request, |_| Ok(()))?;
        Ok(request)
    }
    #[test]
    fn bounded_strict_framing() {
        assert!(
            request(b"POST /x HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}").is_ok()
        );
        for raw in [
            &b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx"[..],
            b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 65537\r\n\r\n",
            b"GET /%00 HTTP/1.1\r\nHost: x\r\n\r\n",
            b"GET /?x=1&x=2 HTTP/1.1\r\nHost: x\r\n\r\n",
        ] {
            assert!(request(raw).is_err());
        }
        // A query's `+` is a space, `%2B` a plus; a path keeps its `+`.
        let parsed = request(b"GET /a+b?path=x+y%2Bz&n%20m=1 HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(parsed.path, "/a+b");
        assert_eq!(parsed.query["path"], "x y+z");
        assert_eq!(parsed.query["n m"], "1");
    }
    /// One byte per read (a slow client) and a single read holding the whole request
    /// parse the same; the body stops at Content-Length and headers stay bounded.
    #[test]
    fn buffered_reads_keep_limits_and_never_read_past_the_body() {
        struct Trickle<'a>(&'a [u8]);
        impl Read for Trickle<'_> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                let Some((first, rest)) = self.0.split_first() else {
                    return Ok(0);
                };
                out[0] = *first;
                self.0 = rest;
                Ok(1)
            }
        }
        let raw = b"POST /api/x?a=1 HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}NEXT";
        let mut trickle = Trickle(raw);
        let mut slow = parse(&mut trickle, |_| Ok(())).unwrap();
        finish(&mut trickle, &mut slow, |_| Ok(())).unwrap();
        for parsed in [slow, request(raw).unwrap()] {
            assert_eq!(parsed.path, "/api/x");
            assert_eq!(parsed.query["a"], "1");
            assert_eq!(parsed.body, b"{}");
        }
        let mut big = b"GET / HTTP/1.1\r\nHost: x\r\nX-Pad: ".to_vec();
        big.resize(MAX_HEAD + 10, b'a');
        assert_eq!(request(&big).err().as_deref(), Some("headers too large"));
        assert!(
            request(b"GET / HTTP/1.1\r\nHost: x\r\n").is_err(),
            "unterminated"
        );
        assert!(
            request(b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\n\r\n{}").is_err(),
            "short body"
        );
        // The head alone allocates nothing for a large declared body.
        let head = parse(&mut &b"POST /api/sessions/s/upload HTTP/1.1\r\nHost: x\r\nContent-Length: 8388608\r\n\r\nab"[..], |_| Ok(())).unwrap();
        assert_eq!(head.body, b"ab");
        assert!(head.body.capacity() < 64 * 1024);
    }
    /// A head trickled a byte at a time, each well inside the per-read timeout, still ends
    /// at the overall deadline (refused as too slow, or as incomplete when the last read
    /// times out at it); a head sent whole within it is read.
    #[test]
    fn a_slow_head_ends_at_its_deadline() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut slow = TcpStream::connect(addr).unwrap();
            slow.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nX-Slow: ")
                .unwrap();
            // Until the host gives up (a write then fails), or 5 s at most.
            for _ in 0..100 {
                std::thread::sleep(Duration::from_millis(50));
                if slow.write_all(b"a").is_err() {
                    break;
                }
            }
            let mut whole = TcpStream::connect(addr).unwrap();
            whole
                .write_all(b"GET /ok HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
            whole
        });
        let (mut slow, _) = listener.accept().unwrap();
        let started = Instant::now();
        let refused = read_head(&mut slow, Duration::from_millis(400));
        let took = started.elapsed();
        let why = refused.err();
        assert!(
            matches!(
                why.as_deref(),
                Some("request head too slow" | "incomplete request")
            ),
            "{why:?}"
        );
        // At about the deadline (a timer may fire a little early), long before the trickle
        // would have ended.
        assert!(
            took >= Duration::from_millis(350) && took < Duration::from_secs(3),
            "{took:?}"
        );
        drop(slow);
        let (mut whole, _) = listener.accept().unwrap();
        assert_eq!(
            read_head(&mut whole, Duration::from_millis(400))
                .unwrap()
                .path,
            "/ok"
        );
        client.join().unwrap();
    }
}
