//! Solo-mining building blocks (P3): pure cores + a minimal HTTP transport.
//!
//! Two transport-agnostic pieces a `NodeWorkSource` needs to mine directly to a
//! csd-node (no pool/bridge):
//!   - [`parse_node_template`] — turn a `GET /work/get` body into the vendored
//!     [`WorkTemplate`] (tolerating both the flat compute-substrate node shape
//!     and the multi-slot envelope build).
//!   - [`solution_to_submission`] — turn a found [`Solution`] into the node's
//!     [`WorkSubmission`] (`{id,nonce,extranonce,time}`) for `POST /work/submit`.
//!
//! The HTTP poller + the `WorkSource` impl that drive these are wired separately
//! (they need a socket); these functions are pure and unit-tested.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde::Deserialize;

use crate::consensus_types::{WorkSubmission, WorkTemplate};

/// The multi-slot node build wraps templates in `{ "templates": [T?, …] }`; the
/// flat compute-substrate build returns a bare `WorkTemplate`. We only need to
/// *deserialize* this fallback shape.
#[derive(Deserialize)]
struct WorkEnvelope {
    templates: Vec<Option<WorkTemplate>>,
}

/// Parse a csd-node `GET /work/get` response body into a [`WorkTemplate`].
///
/// Tries the **flat** shape first (the compute-substrate node returns a bare
/// `WorkTemplate` whose fields are identical to the vendored type), then falls
/// back to the **envelope** shape (`{"templates":[T?,…]}`, the multi-slot build)
/// and takes the first populated slot. A non-JSON / 503 / empty-slot body is an
/// `Err` (the caller idles rather than mining stale/garbage work).
pub fn parse_node_template(body: &str) -> Result<WorkTemplate, String> {
    // Flat shape (the common case). serde ignores unknown fields, so a richer
    // node response still parses as long as the required fields are present.
    if let Ok(t) = serde_json::from_str::<WorkTemplate>(body) {
        return Ok(t);
    }
    // Envelope fallback (multi-slot node build).
    match serde_json::from_str::<WorkEnvelope>(body) {
        Ok(env) => env
            .templates
            .into_iter()
            .flatten()
            .next()
            .ok_or_else(|| "node /work/get envelope had no populated slot".to_string()),
        Err(e) => Err(format!("unrecognized /work/get response: {e}")),
    }
}

/// A solved work unit. Carries BOTH pool and solo submit fields so neither path
/// does a lossy round-trip: the pool default uses `{job_id, xn2, time, nonce}`,
/// the solo override uses `{template_id, extranonce, time, nonce}`.
#[derive(Clone, Debug)]
pub struct Solution {
    /// Node template id (solo submit → `WorkSubmission.id`).
    pub template_id: u64,
    /// Stratum job id (pool submit) + local logging/staleness.
    pub job_id: String,
    /// Rolled xn2 high-half (pool submit via `build_submit`).
    pub xn2: u32,
    /// Full 8-byte extranonce (solo submit).
    pub extranonce: u64,
    pub time: u64,
    pub nonce: u32,
}

/// Map a [`Solution`] to the node's [`WorkSubmission`] — the exact
/// `{id,nonce,extranonce,time}` payload `POST /work/submit` expects.
pub fn solution_to_submission(sol: &Solution) -> WorkSubmission {
    WorkSubmission {
        id: sol.template_id,
        nonce: sol.nonce,
        extranonce: sol.extranonce,
        time: sol.time,
    }
}

// ---------------------------------------------------------------------------
// Minimal HTTP/1.1 client (plain HTTP only) for the solo node transport.
// The csd-node is `http://host:port` (no TLS). G6's HTTPS POST is a separate
// path (it needs a TLS dep); this client is solo-only and dependency-free.
// ---------------------------------------------------------------------------

/// Parsed `http://host[:port][/path]`.
struct NodeUrl<'a> {
    host: &'a str,
    port: u16,
    path: &'a str,
}

/// Parse a plain-HTTP node URL. Rejects non-`http://` (the node transport is
/// cleartext by design; HTTPS would need a TLS dep). Default port 80, path `/`.
fn parse_http_url(url: &str) -> Result<NodeUrl<'_>, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("node url must start with http:// (got {url:?})"))?;
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
    Ok(NodeUrl { host, port, path })
}

/// Parse an HTTP response's status code + body (split on the blank line). The
/// node returns Content-Length JSON (not chunked), so a `Connection: close`
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

/// One blocking HTTP/1.1 request (`Connection: close`) over std `TcpStream`.
fn http_request(method: &str, url: &str, json_body: Option<&str>) -> Result<(u16, String), String> {
    let u = parse_http_url(url)?;
    let mut stream = TcpStream::connect((u.host, u.port))
        .map_err(|e| format!("connect {}:{}: {e}", u.host, u.port))?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(10))).ok();
    let body = json_body.unwrap_or("");
    let req = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        u.path,
        u.host,
        body.len(),
        body
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

/// `GET url` → (status, body). For solo's `/work/get`.
pub fn http_get(url: &str) -> Result<(u16, String), String> {
    http_request("GET", url, None)
}

/// `POST url` with a JSON body → (status, body). For solo's `/work/submit`.
pub fn http_post_json(url: &str, body: &str) -> Result<(u16, String), String> {
    http_request("POST", url, Some(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative template: low (network-style) target, raw prev, 8-byte
    /// extranonce, full nonce range — the shape `/work/get` returns.
    fn sample_template() -> WorkTemplate {
        let mut target = [0u8; 32];
        target[3] = 0x0f;
        target[4] = 0xff; // leading zeros then a small number = a real target
        WorkTemplate {
            id: 7,
            version: 1,
            prev: [0x11u8; 32],
            time: 1_700_000_123,
            bits: 0x1e00ffff,
            target,
            coinbase_prefix: vec![0xde, 0xad],
            extranonce_size: 8,
            coinbase_suffix: vec![0xbe, 0xef],
            merkle_branch: vec![],
            nonce_start: 0,
            nonce_end: u32::MAX,
            height: 12345,
        }
    }

    #[test]
    fn parse_node_template_flat_round_trips() {
        let t = sample_template();
        let body = serde_json::to_string(&t).unwrap();
        let parsed = parse_node_template(&body).expect("flat parse");
        // WorkTemplate has no PartialEq (it's a vendored wire type) — compare via
        // its JSON value, which IS comparable and covers every field.
        assert_eq!(
            serde_json::to_value(&t).unwrap(),
            serde_json::to_value(&parsed).unwrap()
        );
    }

    #[test]
    fn parse_node_template_accepts_envelope_slot_a() {
        let t = sample_template();
        let env = serde_json::json!({
            "templates": [serde_json::to_value(&t).unwrap(), null, null, null]
        });
        let parsed = parse_node_template(&env.to_string()).expect("envelope parse");
        assert_eq!(
            serde_json::to_value(&t).unwrap(),
            serde_json::to_value(&parsed).unwrap()
        );
    }

    #[test]
    fn parse_node_template_rejects_empty_envelope_and_junk() {
        // All slots null → no work.
        let empty = serde_json::json!({ "templates": [null, null] });
        assert!(parse_node_template(&empty.to_string()).is_err());
        // Non-JSON and a 503-style text body → error (caller idles).
        assert!(parse_node_template("not json at all").is_err());
        assert!(parse_node_template("503 Service Unavailable").is_err());
        assert!(parse_node_template("").is_err());
    }

    #[test]
    fn solution_to_submission_has_node_worksubmission_shape() {
        let sol = Solution {
            template_id: 42,
            job_id: "job-1".to_string(),
            xn2: 7,
            extranonce: 0x0102_0304_0506_0708,
            time: 1_700_000_000,
            nonce: 0xDEAD_BEEF,
        };
        let sub = solution_to_submission(&sol);
        let v = serde_json::to_value(&sub).unwrap();
        let obj = v.as_object().expect("submission is a JSON object");
        // EXACTLY {id,nonce,extranonce,time} — no block_hex, no job_id leak.
        assert_eq!(obj.len(), 4, "WorkSubmission must have 4 fields, got {obj:?}");
        assert_eq!(v["id"], 42);
        assert_eq!(v["nonce"], 0xDEAD_BEEFu32);
        assert_eq!(v["extranonce"], 0x0102_0304_0506_0708u64);
        assert_eq!(v["time"], 1_700_000_000u64);
        assert!(v.get("job_id").is_none());
        assert!(v.get("block_hex").is_none());
    }

    #[test]
    fn parse_http_url_extracts_host_port_path() {
        let u = parse_http_url("http://127.0.0.1:8799/work/get").unwrap();
        assert_eq!(u.host, "127.0.0.1");
        assert_eq!(u.port, 8799);
        assert_eq!(u.path, "/work/get");
        let u2 = parse_http_url("http://node.local/work/submit").unwrap();
        assert_eq!(u2.port, 80);
        assert_eq!(u2.path, "/work/submit");
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
        let url = format!("http://127.0.0.1:{}/work/get", addr.port());
        let (code, body) = http_get(&url).expect("get");
        assert_eq!(code, 200);
        assert_eq!(body, "{\"work\":true}");
        server.join().unwrap();
    }
}
