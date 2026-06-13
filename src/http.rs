//! Minimal blocking HTTP/1.1 GET client (plain HTTP, no TLS), dependency-free
//! (std `TcpStream` only). Used by the `hiveos-stats` subcommand to read the
//! local miner's `/1/summary` JSON over loopback. Pure + unit-tested against a
//! localhost mock.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Parsed `http://host[:port][/path]`.
struct HttpUrl<'a> {
    host: &'a str,
    port: u16,
    path: &'a str,
}

/// Parse a plain-HTTP URL. Rejects non-`http://` (this client is cleartext-only;
/// HTTPS would need a TLS dep). Default port 80, path `/`.
fn parse_http_url(url: &str) -> Result<HttpUrl<'_>, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("url must start with http:// (got {url:?})"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h,
            p.parse::<u16>()
                .map_err(|_| format!("bad port in {authority:?}"))?,
        ),
        None => (authority, 80),
    };
    if host.is_empty() {
        return Err(format!("empty host in {url:?}"));
    }
    Ok(HttpUrl { host, port, path })
}

/// Parse an HTTP response's status code + body (split on the blank line). The
/// server returns Content-Length JSON (not chunked), so a `Connection: close`
/// read-to-EOF yields the body verbatim.
fn parse_http_response(raw: &[u8]) -> Result<(u16, String), String> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_ref(), ""));
    let status_line = head.lines().next().ok_or("empty HTTP response")?;
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed status line: {status_line:?}"))?;
    Ok((code, body.to_string()))
}

/// `GET url` → (status, body). One blocking HTTP/1.1 request (`Connection: close`)
/// over a std `TcpStream`.
pub fn http_get(url: &str) -> Result<(u16, String), String> {
    let u = parse_http_url(url)?;
    let mut stream = TcpStream::connect((u.host, u.port))
        .map_err(|e| format!("connect {}:{}: {e}", u.host, u.port))?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(10))).ok();
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
        u.path, u.host
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    stream.flush().ok();
    let mut resp = Vec::new();
    stream
        .read_to_end(&mut resp)
        .map_err(|e| format!("read: {e}"))?;
    parse_http_response(&resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_http_url_extracts_host_port_path() {
        let u = parse_http_url("http://127.0.0.1:8799/1/summary").unwrap();
        assert_eq!(u.host, "127.0.0.1");
        assert_eq!(u.port, 8799);
        assert_eq!(u.path, "/1/summary");
        let u2 = parse_http_url("http://node.local/summary").unwrap();
        assert_eq!(u2.port, 80);
        assert_eq!(u2.path, "/summary");
        assert_eq!(parse_http_url("http://host:1234").unwrap().path, "/");
        assert!(parse_http_url("https://x/y").is_err()); // no TLS in this client
        assert!(parse_http_url("http://:80/x").is_err()); // empty host
        assert!(parse_http_url("http://h:notaport/x").is_err());
    }

    #[test]
    fn parse_http_response_extracts_code_and_body() {
        let (c, b) =
            parse_http_response(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"id\":1}")
                .unwrap();
        assert_eq!(c, 200);
        assert_eq!(b, "{\"id\":1}");
        let (c2, b2) = parse_http_response(b"HTTP/1.1 503 Service Unavailable\r\n\r\n").unwrap();
        assert_eq!(c2, 503);
        assert_eq!(b2, "");
        assert!(parse_http_response(b"garbage").is_err());
    }

    #[test]
    fn http_get_against_localhost_mock() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf); // consume the request, ignore it
                let _ = sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\n{\"work\":true}",
                );
            }
        });
        let url = format!("http://127.0.0.1:{}/summary", addr.port());
        let (code, body) = http_get(&url).expect("get");
        assert_eq!(code, 200);
        assert_eq!(body, "{\"work\":true}");
        server.join().unwrap();
    }
}
