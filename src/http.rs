//! A blocking HTTP/1.1 subset: `GET` and `HEAD`, one thread per connection.
//!
//! An async runtime would cost more than it buys here. The box serves at most
//! a handful of simultaneous clients, and threads with small stacks beat a
//! reactor on binary size, RSS and build complexity at that scale.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Caps on what we will read before deciding a request is hostile or broken.
const MAX_REQUEST_LINE: usize = 4096;
const MAX_HEADERS: usize = 64;
const MAX_HEADER_BYTES: usize = 8192;

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Path with percent-escapes decoded, query removed.
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub peer: String,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn query_value(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Path split on `/` with empty segments removed.
    pub fn segments(&self) -> Vec<&str> {
        self.path.split('/').filter(|s| !s.is_empty()).collect()
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        // `+` means space only in query strings; callers decode those separately.
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (
                percent_decode(&k.replace('+', " ")),
                percent_decode(&v.replace('+', " ")),
            )
        })
        .collect()
}

/// Read one request. `Ok(None)` means the peer closed cleanly.
pub fn read_request<R: BufRead>(r: &mut R, peer: String) -> io::Result<Option<Request>> {
    let mut line = String::new();
    let n = read_line_limited(r, &mut line, MAX_REQUEST_LINE)?;
    if n == 0 {
        return Ok(None);
    }
    let line = line.trim_end();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_ascii_uppercase();
    let target = parts.next().unwrap_or("/");
    if method.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty request line"));
    }

    let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let request = Request {
        method,
        path: percent_decode(raw_path),
        query: parse_query(raw_query),
        headers: read_headers(r)?,
        peer,
    };
    Ok(Some(request))
}

fn read_headers<R: BufRead>(r: &mut R) -> io::Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    let mut total = 0usize;
    loop {
        let mut line = String::new();
        let n = read_line_limited(r, &mut line, MAX_REQUEST_LINE)?;
        if n == 0 {
            break;
        }
        total += n;
        if total > MAX_HEADER_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "headers too large"));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "too many headers"));
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(headers)
}

fn read_line_limited<R: BufRead>(r: &mut R, out: &mut String, limit: usize) -> io::Result<usize> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if buf.len() > limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
        match r.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                buf.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    let n = buf.len();
    out.push_str(&String::from_utf8_lossy(&buf));
    Ok(n)
}

pub fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// Write a complete, length-delimited response.
pub fn respond(
    w: &mut impl Write,
    code: u16,
    content_type: &str,
    body: &[u8],
    extra: &[(&str, String)],
    head_only: bool,
) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\nCache-Control: no-store\r\n",
        status_text(code),
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    if !head_only {
        w.write_all(body)?;
    }
    w.flush()
}

pub fn respond_json(
    w: &mut impl Write,
    code: u16,
    body: &str,
    head_only: bool,
) -> io::Result<()> {
    respond(w, code, "application/json; charset=utf-8", body.as_bytes(), &[], head_only)
}

/// Mirakurun's error body shape.
pub fn respond_error(w: &mut impl Write, code: u16, reason: &str, head_only: bool) -> io::Result<()> {
    let body = crate::json::object(|o| {
        o.num("code", code as i64).str("reason", reason);
    });
    respond_json(w, code, &body, head_only)
}

/// Begin an open-ended `video/MP2T` body.
///
/// The body is framed by connection close rather than chunked encoding: it is
/// valid HTTP/1.1 for a response, it is what every TS consumer expects, and it
/// keeps the per-chunk framing cost off a 1990-BogoMIPS CPU.
pub fn respond_stream_headers(
    w: &mut impl Write,
    extra: &[(&str, String)],
) -> io::Result<()> {
    let mut head = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: video/MP2T\r\nConnection: close\r\n\
         Cache-Control: no-store\r\n",
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.flush()
}

pub type Handler = Arc<dyn Fn(&Request, &mut TcpStream) + Send + Sync>;

/// Accept loop. Blocks until the listener fails.
pub fn serve(listener: TcpListener, max_conns: usize, handler: Handler) -> io::Result<()> {
    let live = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("mkra: accept failed: {e}");
                continue;
            }
        };

        if live.load(Ordering::Relaxed) >= max_conns {
            let _ = respond_error(&mut stream, 503, "too many connections", false);
            continue;
        }

        let handler = Arc::clone(&handler);
        // The slot is released by Drop rather than by a trailing statement:
        // if serve_one ever unwinds, a decrement written after the call would
        // be skipped and the server would permanently lose a connection slot,
        // 503ing everything once max_conns panics have accumulated.
        let slot = Slot::take(Arc::clone(&live));
        let spawned = std::thread::Builder::new()
            .name("mkra-conn".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                let _slot = slot;
                serve_one(&mut stream, &handler);
            });
        if spawned.is_err() {
            eprintln!("mkra: could not spawn connection thread");
        }
    }
    Ok(())
}

/// A live-connection slot that is given back however the thread ends.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Slot(counter)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn serve_one(stream: &mut TcpStream, handler: &Handler) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".into());
    // Header read must not hang forever; the body write has no timeout because
    // a slow client is handled by the ring's drop policy instead.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_nodelay(true);

    let request = {
        let mut reader = BufReader::new(match stream.try_clone() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("mkra: cannot clone socket: {e}");
                return;
            }
        });
        read_request(&mut reader, peer)
    };

    match request {
        Ok(Some(req)) => handler(&req, stream),
        Ok(None) => {}
        Err(e) => {
            let _ = respond_error(stream, 400, &e.to_string(), false);
        }
    }
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(raw: &str) -> Request {
        let mut c = Cursor::new(raw.as_bytes().to_vec());
        read_request(&mut c, "test".into()).unwrap().unwrap()
    }

    #[test]
    fn parses_method_path_and_headers() {
        let r = parse(
            "GET /api/channels HTTP/1.1\r\nHost: x\r\nX-Mirakurun-Priority: 5\r\n\r\n",
        );
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/api/channels");
        assert_eq!(r.header("host"), Some("x"));
        assert_eq!(r.header("X-MIRAKURUN-PRIORITY"), Some("5"));
        assert_eq!(r.segments(), vec!["api", "channels"]);
    }

    #[test]
    fn splits_and_decodes_the_query_string() {
        let r = parse("GET /api/services?name=NHK%20BS&decode=1 HTTP/1.1\r\n\r\n");
        assert_eq!(r.path, "/api/services");
        assert_eq!(r.query_value("name"), Some("NHK BS"));
        assert_eq!(r.query_value("decode"), Some("1"));
        assert_eq!(r.query_value("missing"), None);
    }

    #[test]
    fn decodes_percent_escapes_in_the_path() {
        let r = parse("GET /api/channels/BS/BS17%5F0/stream HTTP/1.1\r\n\r\n");
        assert_eq!(r.path, "/api/channels/BS/BS17_0/stream");
    }

    #[test]
    fn plus_is_a_space_only_in_the_query() {
        let r = parse("GET /a+b?k=a+b HTTP/1.1\r\n\r\n");
        assert_eq!(r.path, "/a+b");
        assert_eq!(r.query_value("k"), Some("a b"));
    }

    #[test]
    fn closed_connection_is_not_an_error() {
        let mut c = Cursor::new(Vec::new());
        assert!(read_request(&mut c, "t".into()).unwrap().is_none());
    }

    #[test]
    fn oversized_headers_are_rejected() {
        let big = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(MAX_HEADER_BYTES + 10));
        let mut c = Cursor::new(big.into_bytes());
        assert!(read_request(&mut c, "t".into()).is_err());
    }

    #[test]
    fn head_response_omits_the_body_but_keeps_the_length() {
        let mut out = Vec::new();
        respond(&mut out, 200, "application/json", b"{\"a\":1}", &[], true).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("Content-Length: 7"));
        assert!(s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn error_body_matches_mirakurun_shape() {
        let mut out = Vec::new();
        respond_error(&mut out, 404, "not found", false).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("HTTP/1.1 404 Not Found"));
        assert!(s.ends_with(r#"{"code":404,"reason":"not found"}"#));
    }

    #[test]
    fn stream_headers_declare_mp2t_and_close_framing() {
        let mut out = Vec::new();
        respond_stream_headers(&mut out, &[("X-Mirakurun-Tuner-User-ID", "u1".into())]).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("Content-Type: video/MP2T"));
        assert!(s.contains("Connection: close"));
        assert!(s.contains("X-Mirakurun-Tuner-User-ID: u1"));
        assert!(!s.contains("Content-Length"));
    }
}
