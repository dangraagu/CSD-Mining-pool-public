//! The `--stats-port` HTTP server (P3 D2): an xmrig `/1/summary`-compatible
//! telemetry endpoint over **std `TcpListener`** (no HTTP framework, no new
//! dep). It serves the pure [`crate::stats::summary_json`] payload, fed by a
//! live [`StatsHandle`] (rolling hashrate windows + uptime) and a per-request
//! [`HealthSnapshot`] read from the live work source.
//!
//! Threat model: this is an **untrusted listener**. It binds localhost by
//! default (the caller passes `127.0.0.1`; never `0.0.0.0` unless the operator
//! explicitly opts in), caps the request read so a never-terminating line can't
//! exhaust memory, never panics on malformed input (junk ⇒ 400), and — when a
//! password is configured — gates `/1/summary` behind a length-checked,
//! byte-by-byte token compare (no early-return timing oracle). `/healthz` is
//! ALWAYS open so external liveness probes work without a secret.
//!
//! Routing/serialization reuse the already-tested pure helpers in
//! [`crate::stats`]; this module is just the socket shell around them.

use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::stats::{self, HashrateWindows, StatsRoute};
use crate::stratum::client::HealthSnapshot;

/// How often the accept loop wakes to re-check `stop` (and the per-connection
/// read timeout). Short so a `stop` request makes the thread exit promptly.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Hard cap on the bytes we read from one request before giving up with `400`.
/// A metrics GET is tiny; anything past this is junk or an attack, never a
/// legitimate request — so we bound the read rather than trust the client to
/// terminate the line.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// Live, shared mining telemetry the stats server reads each request: the
/// rolling hashrate windows (fed by the loop) plus the process start instant
/// (for uptime). Wrap in `Arc` and clone into both the loop and the server.
///
/// "Now" for the server is a real monotonic-ish millisecond clock derived from
/// [`SystemTime`] — the *windowing* itself is already unit-tested in
/// [`crate::stats`] with injected times, so the server only needs a real now.
pub struct StatsHandle {
    hashrate: Mutex<HashrateWindows>,
    /// Latest health snapshot, pushed by the mining loop and served verbatim.
    /// Keeping it here (rather than calling the live work source per request)
    /// means the server needs only the handle, not a reference to the client.
    health: Mutex<HealthSnapshot>,
    /// Process/server start, in UNIX-epoch milliseconds.
    started_ms: u64,
}

impl StatsHandle {
    /// A fresh handle whose clock starts now.
    pub fn new() -> Self {
        StatsHandle {
            hashrate: Mutex::new(HashrateWindows::new()),
            health: Mutex::new(HealthSnapshot::default()),
            started_ms: now_ms(),
        }
    }

    /// Record a combined-hashrate sample (GH/s) at the current wall clock. The
    /// loop calls this at its existing 10s hashrate computation site.
    pub fn record(&self, ghs: f64) {
        if let Ok(mut w) = self.hashrate.lock() {
            w.record(ghs, now_ms());
        }
    }

    /// The `[10s, 60s, 15m]` hashrate windows (GH/s) as of now. A poisoned lock
    /// degrades to zeros rather than panicking the request thread.
    pub fn windows(&self) -> [f64; 3] {
        match self.hashrate.lock() {
            Ok(w) => w.windows(now_ms()),
            Err(_) => [0.0, 0.0, 0.0],
        }
    }

    /// Whole seconds since the handle was created (clamped at 0 if the wall
    /// clock ever goes backwards).
    pub fn uptime_s(&self) -> u64 {
        now_ms().saturating_sub(self.started_ms) / 1000
    }

    /// Push the latest health snapshot — the mining loop calls this; the server
    /// serves whatever was last pushed. A poisoned lock is silently skipped
    /// (telemetry must never take the loop down).
    pub fn set_health(&self, h: HealthSnapshot) {
        if let Ok(mut slot) = self.health.lock() {
            *slot = h;
        }
    }

    /// The last-pushed health snapshot (default/empty until the loop pushes one).
    pub fn health(&self) -> HealthSnapshot {
        self.health.lock().map(|h| h.clone()).unwrap_or_default()
    }
}

impl Default for StatsHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Current wall clock in UNIX-epoch milliseconds (saturating to 0 before the
/// epoch — never panics).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Supply live [`HealthSnapshot`]s to the server without coupling it to the
/// concrete work source. A boxed closure (rather than `Arc<dyn WorkSource>`):
/// the server only needs `health()`, and a closure keeps it from depending on
/// the whole — `Send`-bounded — trait object. The caller passes e.g.
/// `Box::new(move || work.health())`.
pub type HealthFn = Box<dyn Fn() -> HealthSnapshot + Send>;

/// Spawn the stats HTTP server on `bind`, serving until `stop` is set.
///
/// Returns the listener-bound [`JoinHandle`]; the bound port is whatever `bind`
/// resolved to (pass `127.0.0.1:0` in tests for an ephemeral port and read it
/// back via the listener before moving it in — see tests). Errors only if the
/// initial `bind` fails (so the caller can surface a clear "stats port in use").
///
/// - `health` yields a fresh [`HealthSnapshot`] per `/1/summary` request.
/// - `worker` is the miner/worker id echoed in the summary.
/// - `password`: when `Some`, `/1/summary` requires it (Bearer header or
///   `?token=`); `/healthz` stays open regardless.
pub fn spawn(
    bind: SocketAddr,
    handle: Arc<StatsHandle>,
    health: HealthFn,
    worker: String,
    password: Option<String>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    let listener = TcpListener::bind(bind)?;

    let handle = std::thread::Builder::new()
        .name("stats-http".to_string())
        .spawn(move || {
            serve_loop(listener, handle, health, worker, password, stop);
        })?;
    Ok(handle)
}

/// The accept loop: poll `stop`, accept one connection at a time, serve it, move
/// on. Single-threaded on purpose — a metrics endpoint sees trivial traffic and
/// this keeps the surface tiny.
fn serve_loop(
    listener: TcpListener,
    handle: Arc<StatsHandle>,
    health: HealthFn,
    worker: String,
    password: Option<String>,
    stop: Arc<AtomicBool>,
) {
    // `accept` has no timeout, so make the listener non-blocking and poll: this
    // lets the loop wake every POLL_INTERVAL to re-check `stop` and exit
    // promptly instead of blocking forever in `accept`.
    listener.set_nonblocking(true).ok();

    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                // Best-effort per-connection handling; one bad client must never
                // take the loop down.
                serve_one(stream, &handle, &health, &worker, password.as_deref());
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No pending connection — nap, then re-check `stop`.
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(_) => {
                // Transient accept error — back off briefly and keep serving.
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }
}

/// Serve a single connection: read the request line (read-capped), route it,
/// write one response, close. Never panics on malformed input.
fn serve_one(
    mut stream: TcpStream,
    handle: &StatsHandle,
    health: &HealthFn,
    worker: &str,
    password: Option<&str>,
) {
    // Bound how long a stuck client can hold the (single) accept slot.
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok();

    // Read the request HEAD (request line + headers, up to the blank line),
    // read-capped. `None` ⇒ unreadable / oversized / no line → 400, no panic.
    let head = match read_request_head(&mut stream) {
        Some(h) => h,
        None => {
            let _ = write_response(&mut stream, &stats::http_response(400, "text/plain", "bad request"));
            return;
        }
    };
    let request_line = head.lines().next().unwrap_or("");

    // `request_target` returns None for non-GET or a malformed line → 400.
    let target = match stats::request_target(request_line) {
        Some(t) => t,
        None => {
            let _ = write_response(&mut stream, &stats::http_response(400, "text/plain", "bad request"));
            return;
        }
    };

    let resp = match stats::route(target) {
        StatsRoute::Summary => {
            if authorized(target, &head, password) {
                let health = (health)();
                let windows = handle.windows();
                let uptime = handle.uptime_s();
                let body = stats::summary_json(worker, &health, windows, uptime).to_string();
                stats::http_response(200, "application/json", &body)
            } else {
                stats::http_response(401, "text/plain", "unauthorized")
            }
        }
        StatsRoute::Health => {
            // Liveness — NEVER auth-gated.
            let body = format!("{{\"ok\":true,\"uptime\":{}}}", handle.uptime_s());
            stats::http_response(200, "application/json", &body)
        }
        StatsRoute::NotFound => stats::http_response(404, "text/plain", "not found"),
    };

    let _ = write_response(&mut stream, &resp);
}

/// Read the request HEAD — the request line plus headers, up to the blank line
/// that ends the head — capped at [`MAX_REQUEST_BYTES`]. Returns `None` if the
/// read fails, the connection closes with no data, or the cap is hit before the
/// head terminates (an over-long/never-terminating request → caller emits 400).
///
/// We don't read any body (GET telemetry has none); the head is enough to route
/// (first line) and authenticate (`Authorization` header / `?token=` query).
fn read_request_head(stream: &mut TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream);
    // Take-limit the underlying read so a flood can't allocate unboundedly.
    let mut limited = (&mut reader).take(MAX_REQUEST_BYTES as u64);
    let mut raw: Vec<u8> = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        match limited.read(&mut byte) {
            Ok(0) => break, // EOF
            Ok(_) => {
                raw.push(byte[0]);
                // End of head: a blank line ("\r\n\r\n" or "\n\n").
                if raw.ends_with(b"\r\n\r\n") || raw.ends_with(b"\n\n") {
                    return Some(String::from_utf8_lossy(&raw).to_string());
                }
                if raw.len() >= MAX_REQUEST_BYTES {
                    // Hit the cap before the head terminated → malformed.
                    return None;
                }
            }
            Err(_) => {
                // A read timeout/error after we already have a usable request
                // line is still routable; otherwise it's a failed request.
                break;
            }
        }
    }
    // EOF/timeout without an explicit blank line: accept whatever we have iff it
    // contains at least one line (some clients send "GET ...\n" then half-close).
    if raw.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&raw).to_string())
    }
}

/// Whether a `/1/summary` request is allowed. With no configured password,
/// always allowed. Otherwise the token must appear EITHER as
/// `Authorization: Bearer <tok>` (a header line in `head`) OR as `?token=<tok>`
/// in the request target's query — compared with a constant-time-ish byte
/// equality so the token isn't a trivial timing oracle.
fn authorized(target: &str, head: &str, password: Option<&str>) -> bool {
    let expected = match password {
        None => return true, // open when unconfigured
        Some(p) => p,
    };

    // 1. `?token=<tok>` on the request target.
    if let Some(tok) = query_token(target) {
        if ct_eq(tok.as_bytes(), expected.as_bytes()) {
            return true;
        }
    }
    // 2. `Authorization: Bearer <tok>` header (case-insensitive header name).
    if let Some(tok) = bearer_token(head) {
        if ct_eq(tok.as_bytes(), expected.as_bytes()) {
            return true;
        }
    }
    false
}

/// Extract a `Bearer` token from an `Authorization` header in the request head.
/// Header-name match is case-insensitive; the scheme must be `Bearer`.
fn bearer_token(head: &str) -> Option<String> {
    for line in head.lines() {
        let mut kv = line.splitn(2, ':');
        let name = kv.next()?.trim();
        if name.eq_ignore_ascii_case("authorization") {
            let value = kv.next().unwrap_or("").trim();
            if let Some(tok) = value.strip_prefix("Bearer ") {
                return Some(tok.trim().to_string());
            }
        }
    }
    None
}

/// Extract the `token` query parameter value from a request target like
/// `/1/summary?token=abc&x=1`. Returns `None` if absent.
fn query_token(target: &str) -> Option<String> {
    let query = target.split('?').nth(1)?;
    for pair in query.split('&') {
        let mut kv = pair.splitn(2, '=');
        if kv.next() == Some("token") {
            return kv.next().map(|v| v.to_string());
        }
    }
    None
}

/// Length-checked, byte-by-byte equality that does not early-return on the first
/// mismatching byte (avoids a trivial timing oracle on the token). Not a
/// hardened crypto comparator, but removes the obvious early-exit signal.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Write a full response, ignoring a broken pipe (client may have hung up).
fn write_response(stream: &mut TcpStream, resp: &str) -> std::io::Result<()> {
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    /// A minimal local [`HealthSnapshot`] source for the server tests — does NOT
    /// depend on loop_stratum's test-only `MockWorkSource`.
    fn stub_health() -> HealthSnapshot {
        HealthSnapshot {
            accepted: 7,
            rejected: 2,
            stale: 1,
            submitted: 10,
            job_age_s: Some(5),
            endpoint: "pool.example:3333".to_string(),
        }
    }

    /// Spin up a server on an ephemeral localhost port; return the chosen
    /// `SocketAddr`, the join handle, the `stop` flag, and the `StatsHandle`.
    /// We learn a free port by binding `127.0.0.1:0`, reading the addr, dropping
    /// the probe, then handing that exact addr to `spawn`. (`http_get_raw`
    /// retries the connect, tolerating the brief gap before the server rebinds.)
    fn start_server(
        password: Option<String>,
    ) -> (SocketAddr, JoinHandle<()>, Arc<AtomicBool>, Arc<StatsHandle>) {
        // Find a free port by binding & dropping, then hand the concrete addr to
        // spawn. (On the loopback this is reliable in practice for tests.)
        let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let handle = Arc::new(StatsHandle::new());
        // Seed a hashrate sample so windows are non-trivial.
        handle.record(2.5);
        let stop = Arc::new(AtomicBool::new(false));
        let join = spawn(
            addr,
            handle.clone(),
            Box::new(stub_health),
            "csd1worker".to_string(),
            password,
            stop.clone(),
        )
        .expect("spawn stats server");
        (addr, join, stop, handle)
    }

    /// Connect, send a raw request, return the full response as a String.
    fn http_get_raw(addr: SocketAddr, raw_request: &str) -> String {
        // Retry the connect briefly: the server thread may not have bound yet.
        let mut stream = None;
        for _ in 0..50 {
            match TcpStream::connect(addr) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        let mut stream = stream.expect("connect to stats server");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .ok();
        // Tolerate a write error: for the oversized-request case the server
        // legitimately reads its cap, replies 400, and CLOSES while we're still
        // writing — a broken-pipe here is expected, not a test failure. We still
        // read whatever response landed before the close.
        let _ = stream.write_all(raw_request.as_bytes());
        let _ = stream.flush();
        let mut buf = Vec::new();
        let _ = stream.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).to_string()
    }

    /// Split an HTTP response into (status_line, body).
    fn split_response(resp: &str) -> (String, String) {
        let mut parts = resp.splitn(2, "\r\n\r\n");
        let head = parts.next().unwrap_or("").to_string();
        let body = parts.next().unwrap_or("").to_string();
        let status = head.lines().next().unwrap_or("").to_string();
        (status, body)
    }

    fn stop_and_join(stop: Arc<AtomicBool>, join: JoinHandle<()>) {
        stop.store(true, Ordering::Relaxed);
        join.join().expect("server thread joined");
    }

    #[test]
    fn summary_returns_200_xmrig_json_no_password() {
        let (addr, join, stop, _h) = start_server(None);
        let resp = http_get_raw(addr, "GET /1/summary HTTP/1.1\r\nHost: x\r\n\r\n");
        let (status, body) = split_response(&resp);
        assert!(status.starts_with("HTTP/1.1 200"), "status: {status}");

        let v: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
        let total = v["hashrate"]["total"].as_array().expect("hashrate.total array");
        assert_eq!(total.len(), 3, "three hashrate windows");
        // shares_good == stub.accepted
        assert_eq!(v["results"]["shares_good"], 7);
        // connection.pool == stub.endpoint
        assert_eq!(v["connection"]["pool"], "pool.example:3333");
        assert_eq!(v["worker_id"], "csd1worker");

        stop_and_join(stop, join);
    }

    #[test]
    fn healthz_returns_200_always_even_with_password() {
        let (addr, join, stop, _h) = start_server(Some("s3cr3t".to_string()));
        let resp = http_get_raw(addr, "GET /healthz HTTP/1.1\r\n\r\n");
        let (status, body) = split_response(&resp);
        assert!(status.starts_with("HTTP/1.1 200"), "status: {status}");
        let v: serde_json::Value = serde_json::from_str(&body).expect("health body JSON");
        assert_eq!(v["ok"], true);
        stop_and_join(stop, join);
    }

    #[test]
    fn unknown_path_returns_404() {
        let (addr, join, stop, _h) = start_server(None);
        let resp = http_get_raw(addr, "GET /admin HTTP/1.1\r\n\r\n");
        let (status, _body) = split_response(&resp);
        assert!(status.starts_with("HTTP/1.1 404"), "status: {status}");
        stop_and_join(stop, join);
    }

    #[test]
    fn password_server_rejects_without_token_401() {
        let (addr, join, stop, _h) = start_server(Some("s3cr3t".to_string()));
        let resp = http_get_raw(addr, "GET /1/summary HTTP/1.1\r\n\r\n");
        let (status, _body) = split_response(&resp);
        assert!(status.starts_with("HTTP/1.1 401"), "status: {status}");
        stop_and_join(stop, join);
    }

    #[test]
    fn password_server_accepts_query_token_200() {
        let (addr, join, stop, _h) = start_server(Some("s3cr3t".to_string()));
        let resp = http_get_raw(addr, "GET /1/summary?token=s3cr3t HTTP/1.1\r\n\r\n");
        let (status, body) = split_response(&resp);
        assert!(status.starts_with("HTTP/1.1 200"), "status: {status}, body: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).expect("body JSON");
        assert_eq!(v["results"]["shares_good"], 7);
        stop_and_join(stop, join);
    }

    #[test]
    fn password_server_accepts_bearer_header_200() {
        let (addr, join, stop, _h) = start_server(Some("s3cr3t".to_string()));
        let resp = http_get_raw(
            addr,
            "GET /1/summary HTTP/1.1\r\nAuthorization: Bearer s3cr3t\r\n\r\n",
        );
        let (status, body) = split_response(&resp);
        assert!(status.starts_with("HTTP/1.1 200"), "status: {status}, body: {body}");
        stop_and_join(stop, join);
    }

    #[test]
    fn password_server_rejects_wrong_token_401() {
        let (addr, join, stop, _h) = start_server(Some("s3cr3t".to_string()));
        let resp = http_get_raw(addr, "GET /1/summary?token=wrong HTTP/1.1\r\n\r\n");
        let (status, _body) = split_response(&resp);
        assert!(status.starts_with("HTTP/1.1 401"), "status: {status}");
        stop_and_join(stop, join);
    }

    #[test]
    fn junk_request_line_returns_400_and_server_survives() {
        let (addr, join, stop, _h) = start_server(None);
        // A non-GET / garbage line → 400, no panic.
        let resp = http_get_raw(addr, "GARBAGE nonsense not-http\r\n\r\n");
        let (status, _body) = split_response(&resp);
        assert!(status.starts_with("HTTP/1.1 400"), "status: {status}");

        // The server must still answer the NEXT request (didn't crash).
        let resp2 = http_get_raw(addr, "GET /healthz HTTP/1.1\r\n\r\n");
        let (status2, _b2) = split_response(&resp2);
        assert!(status2.starts_with("HTTP/1.1 200"), "follow-up status: {status2}");

        stop_and_join(stop, join);
    }

    #[test]
    fn oversized_request_line_returns_400_no_panic() {
        let (addr, join, stop, _h) = start_server(None);
        // An over-long request line with no newline (way past the 8KB cap).
        let mut huge = String::from("GET /");
        huge.push_str(&"A".repeat(MAX_REQUEST_BYTES + 1024));
        // No CRLF terminator on purpose.
        let resp = http_get_raw(addr, &huge);
        let (status, _body) = split_response(&resp);
        assert!(
            status.starts_with("HTTP/1.1 400"),
            "oversized should be 400, got: {status}"
        );

        // Still alive afterwards.
        let resp2 = http_get_raw(addr, "GET /healthz HTTP/1.1\r\n\r\n");
        assert!(split_response(&resp2).0.starts_with("HTTP/1.1 200"));

        stop_and_join(stop, join);
    }

    #[test]
    fn setting_stop_makes_thread_exit_quickly() {
        let (_addr, join, stop, _h) = start_server(None);
        stop.store(true, Ordering::Relaxed);
        // Join within a short budget: the accept loop polls `stop` every 250ms.
        let start = std::time::Instant::now();
        join.join().expect("thread should exit on stop");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "thread took too long to stop: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn ct_eq_matches_only_equal_byte_strings() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd")); // length mismatch
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn query_token_extracts_token_param() {
        assert_eq!(query_token("/1/summary?token=abc"), Some("abc".to_string()));
        assert_eq!(
            query_token("/1/summary?x=1&token=zzz&y=2"),
            Some("zzz".to_string())
        );
        assert_eq!(query_token("/1/summary"), None);
        assert_eq!(query_token("/1/summary?notoken=1"), None);
    }

    #[test]
    fn stats_handle_health_round_trips() {
        let h = StatsHandle::new();
        // Empty until the loop pushes a snapshot.
        assert_eq!(h.health().accepted, 0);
        assert_eq!(h.health().endpoint, "");
        h.set_health(HealthSnapshot {
            accepted: 11,
            rejected: 2,
            stale: 1,
            submitted: 14,
            job_age_s: Some(9),
            endpoint: "pool.x:3333".to_string(),
        });
        let got = h.health();
        assert_eq!(got.accepted, 11);
        assert_eq!(got.submitted, 14);
        assert_eq!(got.endpoint, "pool.x:3333");
    }

    /// End-to-end D2: wire the server exactly as `main` does — the health closure
    /// reads from the SAME handle the loop pushes into (`move || handle.health()`)
    /// — then assert a recorded hashrate sample and a pushed health snapshot both
    /// show through `/1/summary`.
    #[test]
    fn summary_reflects_recorded_hashrate_and_pushed_health() {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let handle = Arc::new(StatsHandle::new());
        handle.record(3.0); // 3 GH/s, in the 10s window
        handle.set_health(HealthSnapshot {
            accepted: 42,
            rejected: 0,
            stale: 0,
            submitted: 42,
            job_age_s: Some(1),
            endpoint: "pool.live:3333".to_string(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let health_src = handle.clone();
        let join = spawn(
            addr,
            handle.clone(),
            Box::new(move || health_src.health()),
            "csd1live".to_string(),
            None,
            stop.clone(),
        )
        .expect("spawn");

        let resp = http_get_raw(addr, "GET /1/summary HTTP/1.1\r\n\r\n");
        let (status, body) = split_response(&resp);
        assert!(status.starts_with("HTTP/1.1 200"), "status: {status}");
        let v: serde_json::Value = serde_json::from_str(&body).expect("json");
        // Health pushed via set_health shows through.
        assert_eq!(v["results"]["shares_good"], 42);
        assert_eq!(v["connection"]["pool"], "pool.live:3333");
        assert_eq!(v["worker_id"], "csd1live");
        // 3 GH/s recorded → 3e9 H/s in the 10s window (xmrig reports H/s).
        let total = v["hashrate"]["total"].as_array().unwrap();
        let s10 = total[0].as_f64().unwrap();
        assert!((s10 - 3.0e9).abs() < 1.0, "10s hashrate {s10} != 3e9");

        stop_and_join(stop, join);
    }
}
