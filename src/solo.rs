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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::consensus_types::{WorkSubmission, WorkTemplate};
use crate::stratum::client::{HealthSnapshot, StratumJob};
use crate::stratum::loop_stratum::{LoopWork, WorkIntake, WorkSource};

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

// ---------------------------------------------------------------------------
// NodeWorkSource — the solo-mining `WorkSource`.
//
// Talks DIRECTLY to a csd-node (no pool/bridge): a background poller GETs
// `/work/get`, and the loop pulls work via `next_work` + submits solved blocks
// via `submit_solution` (POST `/work/submit`). It slots into the SAME
// `run_stratum` loop as the pool `StratumClient` through the `WorkSource` seam.
// ---------------------------------------------------------------------------

/// Monotonic milliseconds since the Unix epoch (saturating; never panics).
/// Mirrors `client.rs::now_unix_ms` (which isn't `pub`) so the poller can stamp
/// `last_template_ms` and `health()` can age it without reaching into the client
/// module's internals.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Solo share accounting for `health()` / the stats endpoint. All `AtomicU64`
/// so the poller, the submit path, and the heartbeat read them lock-free — same
/// spirit as the Stratum client's `SessionStats`.
#[derive(Default)]
struct SoloStats {
    /// Blocks the node ACCEPTED (`{"accepted":true}` on POST `/work/submit`).
    accepted: AtomicU64,
    /// Blocks the node REJECTED (HTTP 200 but `accepted != true`).
    rejected: AtomicU64,
    /// Total `/work/submit` POSTs attempted (incremented before the POST).
    submitted: AtomicU64,
    /// `now_ms()` at the last *new* template (prev/height change). 0 = none yet.
    last_template_ms: AtomicU64,
}

/// A solo-mining [`WorkSource`] backed by a csd-node's `/work/get` + `/work/submit`.
///
/// Construct with [`NodeWorkSource::connect`] (validates the URL, spawns the
/// poller). `Drop` stops + joins the poller. There is no Stratum socket, no pool
/// vardiff, and no notify: `latest_job` is always `None` and `current_difficulty`
/// is a fixed `1.0` — the real gate target rides in `template.target` (the
/// network target the node serves verbatim).
pub struct NodeWorkSource {
    /// `http://host:port` (validated cleartext-only; HTTPS is rejected).
    base_url: String,
    /// Payout addr20 hex — the `?addr=` query AND the coinbase payout.
    addr_hex: String,
    /// Latest template the poller published. `None` = no mineable work yet (just
    /// started, or the node returned 503) ⇒ the loop idles.
    latest: Arc<Mutex<Option<WorkTemplate>>>,
    stats: Arc<SoloStats>,
    shutdown: Arc<AtomicBool>,
    /// The poller thread handle (joined on `Drop`). `None` for a test-only idle
    /// source built without a poller.
    poller: Option<JoinHandle<()>>,
    /// Optional D2 stats sink. `None` until `--stats-port` wires one in via
    /// [`NodeWorkSource::attach_stats`]; mirrors `StratumClient.stats` so solo
    /// gets the same `/1/summary` telemetry as the pool path. When set,
    /// `record_hashrate` pushes the GH/s sample + the live health snapshot.
    stats_sink: Option<Arc<crate::stats_server::StatsHandle>>,
    /// Optional G6 Discord notifier. `None` until `--discord-webhook` wires one
    /// in via [`NodeWorkSource::attach_notifier`]; when set, an ACCEPTED block in
    /// `submit_solution` fires a best-effort block-found post. A solo block is
    /// always a solution, so it fires regardless of `solutions_only`. Off by
    /// default ⇒ zero notify overhead on the unconfigured build.
    notifier: Option<Arc<crate::notify::DiscordNotifier>>,
}

impl NodeWorkSource {
    /// Connect to a csd-node and start polling `/work/get`.
    ///
    /// `node_url` must be a plain-HTTP `http://host:port` (validated up front —
    /// a bad or `https://` URL is an `Err`, no thread is spawned). `addr_hex` is
    /// the payout addr20 (hex, no `0x`) used as the `?addr=` query and the
    /// coinbase payout. The poller runs until `Drop`.
    pub fn connect(node_url: &str, addr_hex: &str) -> anyhow::Result<NodeWorkSource> {
        // Validate the URL shape now (fail fast, before spawning a thread).
        parse_http_url(node_url).map_err(|e| anyhow::anyhow!("invalid --node url: {e}"))?;

        let base_url = node_url.to_string();
        let addr_hex = addr_hex.to_string();
        let latest: Arc<Mutex<Option<WorkTemplate>>> = Arc::new(Mutex::new(None));
        let stats = Arc::new(SoloStats::default());
        let shutdown = Arc::new(AtomicBool::new(false));

        let poller = spawn_poller(
            base_url.clone(),
            addr_hex.clone(),
            Arc::clone(&latest),
            Arc::clone(&stats),
            Arc::clone(&shutdown),
        );

        Ok(NodeWorkSource {
            base_url,
            addr_hex,
            latest,
            stats,
            shutdown,
            poller: Some(poller),
            stats_sink: None,
            notifier: None,
        })
    }

    /// Attach a D2 stats sink. Called once at startup when `--stats-port` is set,
    /// before the mining loop borrows the source (`&mut self`); `None` until then,
    /// so the unconfigured solo build carries no stats overhead. Mirrors
    /// [`crate::stratum::StratumClient::attach_stats`].
    pub fn attach_stats(&mut self, handle: Arc<crate::stats_server::StatsHandle>) {
        self.stats_sink = Some(handle);
    }

    /// Attach a G6 Discord notifier. Called once at startup when
    /// `--discord-webhook` is set, before the mining loop borrows the source
    /// (`&mut self`); `None` until then, so the unconfigured solo build carries
    /// no notify overhead. When set, an ACCEPTED block in `submit_solution` fires
    /// a best-effort (detached, non-blocking) block-found post.
    pub fn attach_notifier(&mut self, n: Arc<crate::notify::DiscordNotifier>) {
        self.notifier = Some(n);
    }

    /// Test-only constructor: an idle source (no poller, `latest = None`) so
    /// `next_work()` yields `Idle` deterministically without a live node.
    #[cfg(test)]
    fn idle_for_test(node_url: &str) -> NodeWorkSource {
        NodeWorkSource {
            base_url: node_url.to_string(),
            addr_hex: "deadbeef".to_string(),
            latest: Arc::new(Mutex::new(None)),
            stats: Arc::new(SoloStats::default()),
            shutdown: Arc::new(AtomicBool::new(true)),
            poller: None,
            stats_sink: None,
            notifier: None,
        }
    }
}

/// Spawn the background poller. Mirrors the spirit of the Stratum client's
/// reader thread, but pulls over HTTP instead of a pushed socket:
///   - `GET /work/get?addr=…`.
///   - **200 + parseable + (prev,height) changed** ⇒ publish the new template +
///     stamp `last_template_ms`. (Mint a new job ONLY on a prev/height change —
///     the same discipline as the bridge poller, so we don't churn jobs while
///     the node re-serves the same tip.)
///   - **503** ⇒ the node says NOT mineable ⇒ clear `latest` (idle; never mine a
///     stale prev).
///   - **any other status / transport `Err`** ⇒ leave `latest` unchanged (mine
///     through a transient blip).
///
/// Sleeps ~1 s between polls, but in ≤200 ms slices so `Drop` (which sets
/// `shutdown`) joins promptly.
fn spawn_poller(
    base_url: String,
    addr_hex: String,
    latest: Arc<Mutex<Option<WorkTemplate>>>,
    stats: Arc<SoloStats>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        // Last (prev, height) we published — so we only re-publish on a change.
        let mut last_seen: Option<([u8; 32], u64)> = None;
        let url = format!("{base_url}/work/get?addr={addr_hex}");

        while !shutdown.load(Ordering::Relaxed) {
            match http_get(&url) {
                Ok((200, body)) => match parse_node_template(&body) {
                    Ok(tmpl) => {
                        let key = (tmpl.prev, tmpl.height);
                        if last_seen != Some(key) {
                            last_seen = Some(key);
                            *latest.lock().unwrap() = Some(tmpl);
                            stats.last_template_ms.store(now_ms(), Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        // A 200 that won't parse is a node/version mismatch, not
                        // a "stop mining" signal — log + keep the last good job.
                        tracing::warn!("solo: /work/get 200 but unparseable: {e}");
                    }
                },
                Ok((503, _)) => {
                    // Node not mineable (no tip / stale / drift ceiling). Idle —
                    // never mine a stale prev. Reset last_seen so the next
                    // mineable template (even the same tip) re-publishes.
                    *latest.lock().unwrap() = None;
                    last_seen = None;
                }
                Ok((code, _)) => {
                    // Some other status — transient; mine through it.
                    tracing::debug!("solo: /work/get unexpected status {code}; keeping last job");
                }
                Err(e) => {
                    tracing::debug!("solo: /work/get transport error: {e}; keeping last job");
                }
            }

            // Sleep ~1s total, but wake every ≤200ms to check shutdown so Drop
            // joins promptly.
            let mut slept = Duration::ZERO;
            let total = Duration::from_secs(1);
            let slice = Duration::from_millis(200);
            while slept < total && !shutdown.load(Ordering::Relaxed) {
                std::thread::sleep(slice);
                slept += slice;
            }
        }
    })
}

impl WorkSource for NodeWorkSource {
    /// Solo has no Stratum notify — there is no pushed job.
    fn latest_job(&self) -> Option<StratumJob> {
        None
    }

    /// No pool vardiff in solo; the network target rides in `template.target`.
    /// A fixed 1.0 keeps the heartbeat's `diff=` field sane.
    fn current_difficulty(&self) -> f64 {
        1.0
    }

    /// The payout addr20 (also the `?addr=` query). Solo mines straight to it.
    fn worker_addr(&self) -> &str {
        &self.addr_hex
    }

    /// Defensive: the loop submits solo finds via `submit_solution` (overridden
    /// below), never via `send_submit`. If something ever routes here it's a bug,
    /// so surface it loudly rather than silently mis-encoding.
    fn send_submit(
        &self,
        _worker: &str,
        _job_id: &str,
        _xn2_hex: &str,
        _ntime_hex: &str,
        _nonce_hex: &str,
    ) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "solo NodeWorkSource submits via submit_solution, not send_submit"
        ))
    }

    /// Poll the latest node template. `Some` ⇒ a job whose `template.target` is
    /// the NETWORK target; solo owns the whole 8-byte extranonce so `xn1_low=0`.
    /// `None` ⇒ idle (no work / node 503).
    fn next_work(&self) -> WorkIntake {
        match self.latest.lock().unwrap().clone() {
            Some(tmpl) => {
                let job_id = format!("node-{}", tmpl.id);
                WorkIntake::Job(LoopWork {
                    template: tmpl,
                    job_id,
                    xn1_low: 0,
                })
            }
            None => WorkIntake::Idle,
        }
    }

    /// POST the solved block to `/work/submit` as `{id,nonce,extranonce,time}`.
    /// Returns `Ok` on HTTP 200 (whether the node accepted or rejected — the
    /// reject is accounted + logged, not an error), `Err` on a non-200 status or
    /// a transport failure. Never panics.
    fn submit_solution(&self, sol: &Solution) -> anyhow::Result<()> {
        self.stats.submitted.fetch_add(1, Ordering::Relaxed);
        let body = serde_json::to_string(&solution_to_submission(sol))?;
        let url = format!("{}/work/submit", self.base_url);
        match http_post_json(&url, &body) {
            Ok((200, resp)) => {
                // Parse the node's verdict; a non-JSON body is treated as a
                // (logged) reject rather than a hard error — the POST did land.
                let v: serde_json::Value = serde_json::from_str(&resp).unwrap_or(serde_json::Value::Null);
                if v.get("accepted").and_then(|a| a.as_bool()) == Some(true) {
                    self.stats.accepted.fetch_add(1, Ordering::Relaxed);
                    // Extract height/hash defensively (never panic on a malformed
                    // 200 body — default to 0 / "" so the post is still sent).
                    let height = v.get("height").and_then(|h| h.as_u64()).unwrap_or(0);
                    let block_hash = v.get("block_hash").and_then(|h| h.as_str()).unwrap_or("");
                    tracing::info!(
                        "solo: BLOCK ACCEPTED height={height} hash={block_hash}"
                    );
                    // G6: a solo block IS a solution, so fire regardless of
                    // `solutions_only`. Best-effort + detached — a webhook hiccup
                    // can never affect this submit (which already landed).
                    if let Some(n) = &self.notifier {
                        n.post(crate::notify::block_found_message(
                            height,
                            block_hash,
                            &self.addr_hex,
                        ));
                    }
                } else {
                    self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        "solo: submit rejected: {}",
                        v.get("error").unwrap_or(&serde_json::Value::Null)
                    );
                }
                Ok(())
            }
            Ok((code, resp)) => Err(anyhow::anyhow!(
                "solo: /work/submit returned HTTP {code}: {resp}"
            )),
            Err(e) => Err(anyhow::anyhow!("solo: /work/submit transport error: {e}")),
        }
    }

    /// Liveness/share snapshot for the INFO heartbeat + stats endpoint, built
    /// from `SoloStats`. `stale` is always 0 (solo has no pool stale concept);
    /// `job_age_s` ages `last_template_ms` (None until the first template).
    fn health(&self) -> HealthSnapshot {
        let last = self.stats.last_template_ms.load(Ordering::Relaxed);
        let job_age_s = if last == 0 {
            None
        } else {
            Some(now_ms().saturating_sub(last) / 1000)
        };
        HealthSnapshot {
            accepted: self.stats.accepted.load(Ordering::Relaxed),
            rejected: self.stats.rejected.load(Ordering::Relaxed),
            stale: 0,
            submitted: self.stats.submitted.load(Ordering::Relaxed),
            job_age_s,
            endpoint: self.base_url.clone(),
        }
    }

    /// Route a combined-hashrate sample (GH/s) into the attached D2 stats sink,
    /// if any — the solo mirror of `StratumClient::record_hashrate_sample`. No-op
    /// when `--stats-port` is off. `self.health()` resolves to the override above
    /// (solo `SoloStats`-backed snapshot, `endpoint = base_url`), so `/1/summary`
    /// shows solo's accepted/rejected/job_age, not the empty default.
    fn record_hashrate(&self, ghs: f64) {
        if let Some(s) = &self.stats_sink {
            s.record(ghs);
            s.set_health(self.health());
        }
    }
}

impl Drop for NodeWorkSource {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.poller.take() {
            let _ = h.join();
        }
    }
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

    // -----------------------------------------------------------------------
    // NodeWorkSource integration tests against a localhost mock csd-node.
    // -----------------------------------------------------------------------

    use crate::stratum::loop_stratum::{WorkIntake, WorkSource};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// A localhost mock csd-node: a `TcpListener` accept loop in a background
    /// thread that routes `GET /work/get…` → 200 + a canned `WorkTemplate`, and
    /// `POST /work/submit` → stash the body + 200 `{"accepted":true,…}`. Handles
    /// MANY connections (the poller hammers GET while the test submits). Stops
    /// when its `shutdown` flag is set (set on drop).
    struct MockNode {
        base_url: String,
        submits: Arc<Mutex<Vec<String>>>,
        shutdown: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl MockNode {
        /// Default mock node: an accepted submit reports `height:1`.
        fn start(template: &WorkTemplate) -> MockNode {
            Self::start_with_height(template, 1)
        }

        /// Like [`start`], but the accepted-submit response reports
        /// `accepted_height` (so a test can assert the block-found notify carries
        /// the height the node returned).
        fn start_with_height(template: &WorkTemplate, accepted_height: u64) -> MockNode {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let base_url = format!("http://127.0.0.1:{}", addr.port());
            let submits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let shutdown = Arc::new(AtomicBool::new(false));
            let tmpl_json = serde_json::to_string(template).unwrap();

            let submits_t = Arc::clone(&submits);
            let shutdown_t = Arc::clone(&shutdown);
            // Non-blocking accept + a short sleep on WouldBlock so the loop wakes
            // to re-check `shutdown` (set on drop) instead of parking forever in
            // a blocking accept after the test's last request.
            listener
                .set_nonblocking(true)
                .expect("listener nonblocking");
            let handle = std::thread::spawn(move || {
                while !shutdown_t.load(Ordering::Relaxed) {
                    let (mut sock, _peer) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(_) => break,
                    };
                    sock.set_read_timeout(Some(Duration::from_millis(500))).ok();
                    sock.set_write_timeout(Some(Duration::from_millis(500))).ok();
                    // Read the full request (head + any body) until we have the
                    // headers, then drain Content-Length bytes for POST.
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 2048];
                    // First read.
                    let n = match sock.read(&mut tmp) {
                        Ok(0) | Err(_) => {
                            continue;
                        }
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    let first_line = text.lines().next().unwrap_or("").to_string();

                    if first_line.starts_with("GET /work/get") {
                        let body = tmpl_json.as_bytes();
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes());
                        let _ = sock.write_all(body);
                    } else if first_line.starts_with("POST /work/submit") {
                        // Make sure we have the whole body: parse Content-Length
                        // and keep reading until we've got it (header + body may
                        // arrive in separate TCP segments).
                        let want: usize = text
                            .split("\r\n")
                            .find_map(|l| {
                                let l = l.to_ascii_lowercase();
                                l.strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        // Bytes of body already in `buf` (after the blank line).
                        let header_end = text.find("\r\n\r\n").map(|i| i + 4);
                        let mut body_bytes = match header_end {
                            Some(he) => buf[he..].to_vec(),
                            None => Vec::new(),
                        };
                        while body_bytes.len() < want {
                            match sock.read(&mut tmp) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => body_bytes.extend_from_slice(&tmp[..n]),
                            }
                        }
                        let body_str = String::from_utf8_lossy(&body_bytes).to_string();
                        submits_t.lock().unwrap().push(body_str);
                        let resp_body =
                            format!("{{\"accepted\":true,\"height\":{accepted_height},\"block_hash\":\"ab\"}}");
                        let resp_body = resp_body.as_bytes();
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            resp_body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes());
                        let _ = sock.write_all(resp_body);
                    } else {
                        let _ = sock.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                }
            });

            MockNode {
                base_url,
                submits,
                shutdown,
                handle: Some(handle),
            }
        }
    }

    impl Drop for MockNode {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    #[test]
    fn node_work_source_poller_populates_next_work() {
        let tmpl = sample_template();
        let node = MockNode::start(&tmpl);
        let src = NodeWorkSource::connect(&node.base_url, "1122334455667788990011223344556677889900")
            .expect("connect");

        // Poll up to ~3s in 10ms steps for the poller to land a template.
        let mut got = None;
        for _ in 0..300 {
            if let WorkIntake::Job(w) = src.next_work() {
                got = Some(w);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let work = got.expect("poller should have populated a job within 3s");
        assert_eq!(work.template.id, tmpl.id, "job carries the canned template id");
        assert_eq!(work.xn1_low, 0, "solo owns the whole extranonce → xn1_low=0");
        assert_eq!(work.job_id, format!("node-{}", tmpl.id));
    }

    #[test]
    fn node_work_source_submit_posts_worksubmission() {
        let tmpl = sample_template();
        let node = MockNode::start(&tmpl);
        let src = NodeWorkSource::connect(&node.base_url, "1122334455667788990011223344556677889900")
            .expect("connect");

        // Wait for a template so the source is live (not strictly required for
        // submit, but mirrors real flow).
        for _ in 0..300 {
            if matches!(src.next_work(), WorkIntake::Job(_)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let sol = Solution {
            template_id: tmpl.id,
            job_id: format!("node-{}", tmpl.id),
            xn2: 0,
            extranonce: 0x0102_0304_0506_0708,
            time: 1_700_000_123,
            nonce: 0x00C0_FFEE,
        };
        src.submit_solution(&sol).expect("submit returns Ok on HTTP 200 accepted");

        // The mock recorded a POST body that parses to {id,nonce,extranonce,time}.
        // Give the body a moment in case of scheduling (it's synchronous, but be
        // defensive against the accept loop's slice timing).
        let mut recorded = None;
        for _ in 0..100 {
            if let Some(b) = node.submits.lock().unwrap().first().cloned() {
                recorded = Some(b);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let body = recorded.expect("mock recorded a /work/submit POST body");
        let v: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
        assert_eq!(v["id"], tmpl.id);
        assert_eq!(v["nonce"], 0x00C0_FFEEu32);
        assert_eq!(v["extranonce"], 0x0102_0304_0506_0708u64);
        assert_eq!(v["time"], 1_700_000_123u64);

        let h = src.health();
        assert_eq!(h.accepted, 1, "an accepted submit bumps accepted");
        assert_eq!(h.submitted, 1, "submit bumps submitted");
        assert_eq!(h.rejected, 0);
        assert_eq!(h.endpoint, node.base_url);
    }

    #[test]
    fn node_work_source_next_work_idle_before_template() {
        // A source whose `latest` is None yields Idle (no poller needed).
        let src = NodeWorkSource::idle_for_test("http://127.0.0.1:1");
        assert!(matches!(src.next_work(), WorkIntake::Idle));
        // current_difficulty is the no-vardiff sentinel; worker_addr echoes addr.
        assert_eq!(src.current_difficulty(), 1.0);
        assert_eq!(src.worker_addr(), "deadbeef");
    }

    #[test]
    fn node_work_source_record_hashrate_routes_into_attached_stats() {
        use crate::stats_server::StatsHandle;

        // An idle source (no poller / live node needed) with an attached D2 sink.
        let mut src = NodeWorkSource::idle_for_test("http://127.0.0.1:1");
        let handle = Arc::new(StatsHandle::new());
        src.attach_stats(Arc::clone(&handle));

        // The WorkSource override must push the GH/s sample + solo health into the
        // handle (the same plumbing the pool path gets via StratumClient).
        WorkSource::record_hashrate(&src, 2.0);

        // StatsHandle::windows() returns GH/s verbatim (the *1e9 → H/s scaling
        // happens later, in stats::summary_json), so a single 2.0 GH/s sample
        // means the freshest (10s) window reads ~2.0 GH/s.
        let w = handle.windows();
        assert!(
            (w[0] - 2.0).abs() < 1e-9,
            "10s window should be ~2.0 GH/s, got {}",
            w[0]
        );
        // And the pushed health snapshot is solo's (endpoint = the node base_url),
        // proving record_hashrate called the NodeWorkSource health() override.
        assert_eq!(handle.health().endpoint, "http://127.0.0.1:1");
    }

    #[test]
    fn node_work_source_record_hashrate_without_sink_is_noop() {
        // No sink attached ⇒ record_hashrate must not panic (unconfigured build).
        let src = NodeWorkSource::idle_for_test("http://127.0.0.1:1");
        WorkSource::record_hashrate(&src, 5.0); // no-op, no panic
    }

    // -----------------------------------------------------------------------
    // G6 Discord block-found notify: an ACCEPTED solo submit fires a detached
    // POST to the configured webhook. We stand up a localhost mock WEBHOOK (a
    // TcpListener that records POST bodies + replies 200 {}) and assert the
    // post lands with the height/hash text. The post is fire-and-forget, so we
    // poll the recorder with a timeout and tolerate scheduling latency.
    // -----------------------------------------------------------------------

    /// A localhost mock Discord webhook: a `TcpListener` accept loop that reads
    /// each incoming POST (draining the Content-Length body), records the body,
    /// and replies `200 {}`. Handles many connections; stops on `shutdown`
    /// (set on drop). Mirrors `MockNode`'s POST-draining discipline so the body
    /// is captured whole even if it arrives in separate TCP segments.
    struct WebhookRecorder {
        port: u16,
        posts: Arc<Mutex<Vec<String>>>,
        shutdown: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl WebhookRecorder {
        fn start() -> WebhookRecorder {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let posts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let shutdown = Arc::new(AtomicBool::new(false));

            let posts_t = Arc::clone(&posts);
            let shutdown_t = Arc::clone(&shutdown);
            listener.set_nonblocking(true).expect("listener nonblocking");
            let handle = std::thread::spawn(move || {
                while !shutdown_t.load(Ordering::Relaxed) {
                    let mut sock = match listener.accept() {
                        Ok((s, _)) => s,
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(_) => break,
                    };
                    sock.set_read_timeout(Some(Duration::from_millis(500))).ok();
                    sock.set_write_timeout(Some(Duration::from_millis(500))).ok();
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 2048];
                    let n = match sock.read(&mut tmp) {
                        Ok(0) | Err(_) => continue,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    // Drain the full Content-Length body (header + body may split
                    // across segments) so we record the JSON whole.
                    let want: usize = text
                        .split("\r\n")
                        .find_map(|l| {
                            let l = l.to_ascii_lowercase();
                            l.strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    let header_end = text.find("\r\n\r\n").map(|i| i + 4);
                    let mut body_bytes = match header_end {
                        Some(he) => buf[he..].to_vec(),
                        None => Vec::new(),
                    };
                    while body_bytes.len() < want {
                        match sock.read(&mut tmp) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => body_bytes.extend_from_slice(&tmp[..n]),
                        }
                    }
                    posts_t
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&body_bytes).to_string());
                    // Minimal valid Discord-style 200 reply so ureq sees success.
                    let _ = sock.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    );
                }
            });

            WebhookRecorder {
                port,
                posts,
                shutdown,
                handle: Some(handle),
            }
        }
    }

    impl Drop for WebhookRecorder {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    #[test]
    fn solo_accepted_submit_fires_block_found_webhook() {
        use crate::notify::DiscordNotifier;

        let tmpl = sample_template();
        // Node accepts the submit and reports height 7 + block_hash "ab".
        let node = MockNode::start_with_height(&tmpl, 7);
        let webhook = WebhookRecorder::start();

        let mut src =
            NodeWorkSource::connect(&node.base_url, "1122334455667788990011223344556677889900")
                .expect("connect");
        // http:// is fine here — DiscordNotifier::new does NOT validate the URL
        // (only the CLI enforces https). solutions_only=false; a solo block fires
        // regardless either way.
        src.attach_notifier(Arc::new(DiscordNotifier::new(
            format!("http://127.0.0.1:{}", webhook.port),
            false,
        )));

        let sol = Solution {
            template_id: tmpl.id,
            job_id: format!("node-{}", tmpl.id),
            xn2: 0,
            extranonce: 0x0102_0304_0506_0708,
            time: 1_700_000_123,
            nonce: 0x00C0_FFEE,
        };
        src.submit_solution(&sol).expect("submit Ok on HTTP 200 accepted");

        // The post is detached (a fresh ureq Agent built + connecting on its own
        // thread), which under a busy PARALLEL test run can take several seconds —
        // poll generously (up to ~10s) so this never flakes on a loaded box. It
        // still returns the instant the POST lands when the box is idle.
        let mut body = None;
        for _ in 0..1000 {
            if let Some(b) = webhook.posts.lock().unwrap().first().cloned() {
                body = Some(b);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let body = body.expect("block-found webhook POST should arrive within ~10s");
        // Discord payload shape + the height/hash text from block_found_message.
        assert!(body.contains("content"), "payload has a content field: {body}");
        assert!(body.contains('7'), "height 7 present in {body}");
        assert!(body.contains("ab"), "block hash 'ab' present in {body}");
        // And the payout addr (worker) is in the message too.
        assert!(
            body.contains("1122334455667788990011223344556677889900"),
            "worker addr present in {body}"
        );
    }

    #[test]
    fn solo_accepted_submit_without_notifier_does_not_post() {
        // No notifier attached ⇒ an accepted submit must NOT attempt any post
        // (notifier None ⇒ the block-found branch is skipped). This is the
        // default build's behaviour: zero notify side-effects.
        let tmpl = sample_template();
        let node = MockNode::start(&tmpl);
        let src =
            NodeWorkSource::connect(&node.base_url, "1122334455667788990011223344556677889900")
                .expect("connect");
        let sol = Solution {
            template_id: tmpl.id,
            job_id: format!("node-{}", tmpl.id),
            xn2: 0,
            extranonce: 0x0102_0304_0506_0708,
            time: 1_700_000_123,
            nonce: 0x00C0_FFEE,
        };
        // Must not panic and must return Ok with no notifier wired.
        src.submit_solution(&sol).expect("submit Ok with no notifier");
        assert_eq!(src.health().accepted, 1, "accepted still counted");
    }
}
