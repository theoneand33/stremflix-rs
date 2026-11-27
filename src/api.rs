//! Local HTTP API for other apps on this machine: Continue Watching, search, and play (see API.md).
//! No authentication by design; it binds loopback by default. Each call goes to the UI thread through `on_call`.

use crate::addon::{sanitize_text, valid_id};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_ADDR: &str = "127.0.0.1:11471";
/// Longest a request waits on the app: search waits on every addon (15 s timeout each, in parallel).
pub const WAIT: Duration = Duration::from_secs(20);
const MAX_HEAD: u64 = 16 * 1024;
const MAX_QUERY: usize = 100;
const MAX_NAME: usize = 300;
const MAX_PATH: usize = 4096;
const TIMEOUT: Duration = Duration::from_secs(5);

/// A JSON body, or an HTTP status and error message.
pub type Reply = Result<Value, (u16, String)>;

#[derive(Debug, PartialEq)]
pub enum Call {
    /// `GET /api/continue`
    Continue,
    /// `GET /api/search?q=<query>`
    Search(String),
    /// `POST /api/play?type=<type>&id=<id>[&name=<title>]`
    Play { kind: String, id: String, name: Option<String> },
}

/// Bind `addr` and answer each connection on its own thread with `on_call`. Returns the bound address.
pub fn serve(addr: &str, on_call: impl Fn(Call) -> Reply + Send + Sync + 'static) -> std::io::Result<SocketAddr> {
    let l = TcpListener::bind(addr)?;
    let local = l.local_addr()?;
    let on_call = Arc::new(on_call);
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let on_call = on_call.clone();
            std::thread::spawn(move || handle(c, &*on_call));
        }
    });
    Ok(local)
}

fn handle(c: TcpStream, on_call: &dyn Fn(Call) -> Reply) {
    let _ = c.set_read_timeout(Some(TIMEOUT));
    let mut r = BufReader::new((&c).take(MAX_HEAD));
    let mut first = String::new();
    let _ = r.read_line(&mut first);
    // Read the headers and any body, so closing doesn't reset the connection before the reply arrives.
    let (mut line, mut body, mut host_ok) = (String::new(), 0u64, true);
    while r.read_line(&mut line).is_ok_and(|n| n > 0) && !line.trim().is_empty() {
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                body = v.trim().parse().unwrap_or(0);
            } else if k.eq_ignore_ascii_case("host") {
                host_ok = local_host(v.trim());
            }
        }
        line.clear();
    }
    let _ = std::io::copy(&mut r.take(body), &mut std::io::sink());
    let mut parts = first.split_whitespace();
    let reply = if host_ok { route(parts.next().unwrap_or(""), parts.next().unwrap_or("")).and_then(on_call) } else { Err((400, "Host must be an IP address or localhost".into())) };
    let (status, body) = match reply {
        Ok(v) => (200, v),
        Err((s, e)) => (s, json!({ "error": e })),
    };
    let body = body.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Service Unavailable",
    };
    let _ = write!(&c, "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
}

/// A `Host` header naming an IP address or `localhost`. Any other name is a web page reached by DNS rebinding.
fn local_host(h: &str) -> bool {
    match url::Url::parse(&format!("http://{h}/")).ok().and_then(|u| u.host().map(|h| h.to_owned())) {
        Some(url::Host::Domain(d)) => d == "localhost",
        Some(_) => true,
        None => false,
    }
}

/// The call a request line asks for; query values are percent-decoded and validated.
pub fn route(method: &str, target: &str) -> Result<Call, (u16, String)> {
    let bad = |e: &str| Err((400, e.to_string()));
    let Some(u) = target.starts_with('/').then(|| url::Url::parse(&format!("http://localhost{target}")).ok()).flatten() else { return bad("bad request target") };
    let q = |k: &str| u.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
    match (method, u.path()) {
        ("GET", "/api/continue") => Ok(Call::Continue),
        ("GET", "/api/search") => match q("q").map(|s| sanitize_text(&s, MAX_QUERY)).filter(|s| !s.is_empty()) {
            Some(s) => Ok(Call::Search(s)),
            None => bad("missing q"),
        },
        ("POST", "/api/play") => match (q("type"), q("id")) {
            // A local file's id is its path, so it may contain `/`.
            (Some(kind), Some(id)) if valid_id(&kind) && (valid_id(&id) || kind == crate::key::FILE && !id.chars().any(char::is_control) && id.len() <= MAX_PATH) => {
                Ok(Call::Play { kind, id, name: q("name").map(|s| sanitize_text(&s, MAX_NAME)).filter(|s| !s.is_empty()) })
            }
            _ => bad("missing or invalid type or id"),
        },
        (_, "/api/continue" | "/api/search" | "/api/play") => Err((405, "method not allowed".into())),
        _ => Err((404, "not found".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        assert_eq!(route("GET", "/api/continue"), Ok(Call::Continue));
        assert_eq!(route("GET", "/api/search?q=the%20matrix"), Ok(Call::Search("the matrix".into())));
        assert_eq!(route("GET", "/api/search?q=+a%07+"), Ok(Call::Search("a".into())));
        assert_eq!(route("POST", "/api/play?type=series&id=tt1%3A1%3A2"), Ok(Call::Play { kind: "series".into(), id: "tt1:1:2".into(), name: None }));
        assert_eq!(route("POST", "/api/play?type=movie&id=tt1&name=A%20B"), Ok(Call::Play { kind: "movie".into(), id: "tt1".into(), name: Some("A B".into()) }));
        assert_eq!(route("POST", "/api/play?type=movie&id=a%2Fb").unwrap_err().0, 400);
        assert_eq!(route("POST", "/api/play?type=file&id=%2Fv%2Fa.mkv"), Ok(Call::Play { kind: "file".into(), id: "/v/a.mkv".into(), name: None }));
        assert_eq!(route("POST", "/api/play?type=file&id=%2Fv%0A").unwrap_err().0, 400);
        assert_eq!(route("POST", "/api/play?type=movie").unwrap_err().0, 400);
        assert_eq!(route("GET", "/api/search?q=%20").unwrap_err().0, 400);
        assert_eq!(route("GET", "/api/play?type=movie&id=tt1").unwrap_err().0, 405);
        assert_eq!(route("GET", "/nope").unwrap_err().0, 404);
        assert_eq!(route("GET", "http://evil/api/continue").unwrap_err().0, 400);
    }

    #[test]
    fn serves_json_over_http() {
        let addr = serve("127.0.0.1:0", |c| match c {
            Call::Continue => Ok(json!([{"id": "tt1"}])),
            _ => Err((503, "busy".into())),
        })
        .unwrap();
        let get = |req: &str| {
            let mut c = TcpStream::connect(addr).unwrap();
            c.write_all(req.as_bytes()).unwrap();
            let mut s = String::new();
            c.read_to_string(&mut s).unwrap();
            s
        };
        let ok = get("GET /api/continue HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n");
        assert!(ok.starts_with("HTTP/1.1 200 OK\r\n") && ok.ends_with(r#"[{"id":"tt1"}]"#), "{ok}");
        let err = get("POST /api/play?type=movie&id=tt1 HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}");
        assert!(err.starts_with("HTTP/1.1 503 ") && err.ends_with(r#"{"error":"busy"}"#), "{err}");
        assert!(get("GET /api/continue HTTP/1.1\r\nHost: localhost\r\n\r\n").starts_with("HTTP/1.1 200 "));
        assert!(get("GET /api/continue HTTP/1.1\r\nHost: [::1]:9\r\n\r\n").starts_with("HTTP/1.1 200 "));
        assert!(get("GET /api/continue HTTP/1.1\r\nHost: evil.example:11471\r\n\r\n").starts_with("HTTP/1.1 400 "));
    }
}
