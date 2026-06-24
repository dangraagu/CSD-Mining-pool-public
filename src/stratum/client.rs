//! Live Stratum v1 client: TCP connect, subscribe/authorize handshake, and a
//! background reader thread that keeps the latest pushed job and the current
//! share difficulty up to date.
//!
//! This is the **protocol/transport** layer only. The reader thread parses
//! `mining.notify` into a [`StratumJob`] (the raw 9-tuple + the session
//! extranonce1) and stashes `mining.set_difficulty` — it does NOT build a
//! [`crate::csd_consensus::WorkTemplate`]; the mapping step does that.
//!
//! Concurrency model:
//!   - The write half of the socket lives behind a `Mutex<TcpStream>` so the
//!     mining thread (`send_submit`) and reconnect path can serialize writes.
//!   - The reader thread owns a `BufReader` over a *cloned* `TcpStream` handle
//!     (a dup of the same OS socket) and loops over `\n`-delimited lines.
//!   - Latest job + difficulty are shared via `Arc<Mutex<..>>` and read by the
//!     mining thread through [`StratumClient::latest_job`] /
//!     [`StratumClient::current_difficulty`].
//!   - On read error/timeout the reader logs and reconnects with capped
//!     backoff, re-running the subscribe/authorize handshake. It never panics.

use std::io::{self, BufRead, BufReader, ErrorKind, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};

use crate::endpoint::EndpointList;
use crate::notify::{share_accepted_message, DiscordNotifier};
use crate::stats_server::StatsHandle;
use super::protocol::{
    authorize_request, serialize_line, subscribe_request, submit_request,
    suggest_difficulty_request, NotifyParams, Notification, Response, SubscribeResult,
};
use super::watchdog::{WatchdogSnapshot, WatchdogView};

/// How long to wait on `connect()` for the TCP three-way handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Read timeout on the socket so the reader thread wakes periodically instead of
/// blocking forever on a quiet link. A timeout is a **benign idle** signal (the
/// bridge simply hasn't pushed a frame lately), NOT a disconnect — see
/// [`classify_read`], which keeps the session on a timeout. (Treating the
/// timeout as fatal was the `os error 11` reconnect-storm bug on idle CPU rigs.)
///
/// Kept SHORT (5s) so the reader observes a shutdown promptly. On `Drop`, a
/// cross-handle `shutdown()` is not guaranteed to interrupt an in-flight `recv()`
/// (notably on Windows), so the reader instead wakes on this timeout, re-checks
/// `shutdown` at the top of its loop, and exits — bounding teardown to ~5s on
/// every platform instead of stalling up to 120s. Safe because a timeout is
/// classified `Idle` (it never reconnects), so a quiet link just re-reads.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Reconnect backoff bounds.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// After failing over to a backup pool, how long to stay there before the
/// reconnect path retries the primary again (a quiet-period failback).
const FAILBACK_MS: u64 = 30 * 60 * 1000; // 30 min

/// What the reader thread should do with the outcome of one `read_line`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadAction {
    /// A full `\n`-terminated frame (n > 0 bytes) arrived — process it.
    Frame,
    /// A benign **idle read-timeout**: `SO_RCVTIMEO` fired (`WouldBlock` /
    /// `TimedOut`, surfaced as EAGAIN "os error 11"), or the syscall was
    /// `Interrupted`. The link is quiet, not dead — keep the session and keep
    /// waiting. Reconnecting here is the reconnect-storm bug.
    Idle,
    /// Clean EOF (peer closed) or a real I/O error — reconnect.
    Reconnect,
}

/// Decide what to do with a `read_line` outcome, without touching the socket —
/// kept pure so the idle-vs-dead policy is unit-tested directly.
fn classify_read(read: &io::Result<usize>) -> ReadAction {
    match read {
        Ok(0) => ReadAction::Reconnect, // clean EOF: peer closed the connection
        Ok(_) => ReadAction::Frame,
        Err(e) => match e.kind() {
            // SO_RCVTIMEO / EAGAIN / an interrupted syscall: the link is quiet,
            // not dead. Keep waiting instead of tearing down the session.
            ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted => {
                ReadAction::Idle
            }
            _ => ReadAction::Reconnect,
        },
    }
}

/// The outcome of a `mining.submit`, parsed from the pool's JSON-RPC response.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ack {
    /// Pool accepted the share (`result: true`).
    Accepted,
    /// Pool rejected the share; carries the reason text for the log.
    Rejected(String),
    /// A reject whose reason is staleness / job-not-found — a timing loss, not a
    /// bad share — tracked separately so it doesn't read as a hardware fault.
    Stale,
}

/// Per-session share counters, written by the reader thread on each ack and read
/// (lock-free) by the loop / heartbeat. (Grows in later P1 steps: unacked
/// streak + last-accept timestamp for the watchdogs.)
#[derive(Default)]
struct SessionStats {
    accepted: AtomicU64,
    rejected: AtomicU64,
    stale: AtomicU64,
    /// Total shares written to the socket (for shares_10m + the submit-ack wd).
    submitted: AtomicU64,
    /// Consecutive submits with no ack of any kind; reset by ANY ack. Drives the
    /// submit-ack watchdog (a half-open socket gets jobs but drops shares).
    consecutive_unacked: AtomicU64,
    /// Unix-ms of the last ACCEPTED share (0 = none yet); accepted-share dead-man.
    last_accept_ms: AtomicU64,
    /// Unix-ms a *new* job_id last arrived (0 = none yet); job-staleness wd. Only
    /// advanced on a changed id, so a same-id resend can't mask a stalled pool.
    last_new_job_ms: AtomicU64,
    /// Unix-ms of the last successful (re)connect (0 = none yet). Storm-guard for
    /// the accepted-share dead-man (v0.1.9 #2): every reconnect restarts its clock
    /// so a never-crediting endpoint fails over at most once per `accept_stale`.
    last_reconnect_ms: AtomicU64,
    /// Lifetime count of successful reconnects (after the initial connect) — a
    /// connection-churn signal surfaced in telemetry so an operator can see an
    /// unstable link.
    reconnects: AtomicU64,
    /// Lifetime count of endpoint failovers (accepted-share dead-man rotations) —
    /// surfaced in telemetry so an operator can see a flaky/non-crediting primary.
    failovers: AtomicU64,
}

/// Current wall-clock time as milliseconds since the Unix epoch (0 if the clock
/// predates the epoch — never panics). Stamps share/job timestamps for the
/// staleness watchdogs.
fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Pure decision for the G6 pool accepted-share milestone (heartbeat). Returns
/// `Some(total)` to post when a milestone ping is due, `None` to stay silent:
///   - `solutions_only` ⇒ always `None` (a pool miner can't observe block-found
///     client-side, so "solutions only" means the pool path is silent).
///   - else `Some(accepted)` iff `accepted > last_notified` (the running total
///     grew since the last post — the caller advances its high-water mark to the
///     returned value), `None` otherwise (no growth ⇒ no duplicate ping).
///
/// Free + pure so the gating is unit-tested directly without a live client.
fn milestone_to_post(accepted: u64, last_notified: u64, solutions_only: bool) -> Option<u64> {
    if solutions_only {
        return None;
    }
    (accepted > last_notified).then_some(accepted)
}

/// Classify a received line as a `mining.submit` response, if it is one.
///
/// A submit ack is a JSON-RPC *response*: a numeric `id` plus a top-level
/// `result`/`error`, and crucially **no `method`** (server pushes like
/// `mining.notify` always carry `method`). Submit ids start at 100
/// ([`StratumClient::send_submit`] seeds `next_id` at 100); subscribe/authorize
/// use ids 1/2 and only ever occur inside the handshake loop, never on the
/// reader thread — so an `id >= 100` response the reader sees is a submit ack.
/// Returns `None` for pushes / handshake replies / junk so the caller falls
/// through to [`dispatch_frame`]. Pure → unit-tested directly.
fn classify_ack(line: &str) -> Option<Ack> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let obj = v.as_object()?;
    // A server push (notify/set_difficulty/…) carries `method` — not an ack.
    if obj.get("method").and_then(|m| m.as_str()).is_some() {
        return None;
    }
    // Must be a JSON-RPC response carrying a submit id (>= 100).
    let id = obj.get("id").and_then(|i| i.as_u64())?;
    if id < 100 {
        return None; // subscribe/authorize handshake reply, not a submit ack
    }
    if obj.get("result").and_then(|r| r.as_bool()) == Some(true) {
        return Some(Ack::Accepted);
    }
    // Anything else is a reject; pull a reason and sub-classify staleness.
    let reason = obj
        .get("error")
        .filter(|e| !e.is_null())
        .map(reason_from_error)
        .unwrap_or_else(|| "rejected".to_string());
    let low = reason.to_ascii_lowercase();
    if low.contains("stale") || low.contains("job not found") || low.contains("unknown job") {
        Some(Ack::Stale)
    } else {
        Some(Ack::Rejected(reason))
    }
}

/// Pull a human reason out of a JSON-RPC `error` value. Stratum sends
/// `[code, "message", data]`; some pools send a bare string or an object.
fn reason_from_error(err: &serde_json::Value) -> String {
    if let Some(msg) = err
        .as_array()
        .and_then(|a| a.get(1))
        .and_then(|m| m.as_str())
    {
        return msg.to_string();
    }
    if let Some(s) = err.as_str() {
        return s.to_string();
    }
    err.to_string()
}

/// Record `line` as a submit ack into `stats` if it is one. Returns `true` if it
/// was an ack (counted here); `false` if the caller should hand it to
/// [`dispatch_frame`] as a server push.
fn record_ack(line: &str, stats: &SessionStats) -> bool {
    let ack = match classify_ack(line) {
        Some(a) => a,
        None => return false,
    };
    // ANY ack — accept or reject — proves the connection is alive and
    // responding, so the unacked streak resets (submit-ack watchdog).
    stats.consecutive_unacked.store(0, Ordering::Relaxed);
    match ack {
        Ack::Accepted => {
            stats.accepted.fetch_add(1, Ordering::Relaxed);
            stats.last_accept_ms.store(now_unix_ms(), Ordering::Relaxed);
        }
        Ack::Rejected(reason) => {
            stats.rejected.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("stratum: share REJECTED by pool: {reason}");
        }
        Ack::Stale => {
            stats.stale.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("stratum: share STALE (too late / job not found)");
        }
    }
    true
}

/// A point-in-time view of the miner's liveness + share accounting, for the INFO
/// heartbeat. Empty default for sources without a live connection (the test
/// mock).
#[derive(Debug, Clone, Default)]
pub struct HealthSnapshot {
    pub accepted: u64,
    pub rejected: u64,
    pub stale: u64,
    pub submitted: u64,
    /// Seconds since the last *new* job (`None` = no job received yet).
    pub job_age_s: Option<u64>,
    /// The pool endpoint currently connected to (tracks failover, as of v0.1.9 #1).
    pub endpoint: String,
    /// Lifetime successful reconnects — a connection-churn signal for dashboards.
    /// 0 for sources without a live pool (the test mock).
    pub reconnects: u64,
    /// Lifetime endpoint failovers (accepted-share dead-man rotations). 0 for
    /// sources without a live pool.
    pub failovers: u64,
}

/// A job pushed by the pool via `mining.notify`, paired with the session
/// `extranonce1` captured at subscribe time. The notify→header mapping is
/// Task 3; this is the raw material that mapping will consume.
#[derive(Clone, Debug)]
pub struct StratumJob {
    pub notify: NotifyParams,
    pub extranonce1_hex: String,
}

/// Shared state the reader thread writes and the mining thread reads.
struct Shared {
    /// Latest pushed job, if any has arrived yet.
    latest_job: Mutex<Option<StratumJob>>,
    /// Current share difficulty (from `mining.set_difficulty`), bit-encoded as
    /// an `f64` in an `AtomicU64` so reads are lock-free. Defaults to 1.0 until
    /// the pool sends its first `set_difficulty`.
    difficulty_bits: AtomicU64,
    /// Session extranonce1 (hex) captured at subscribe time; refreshed on each
    /// reconnect (the bridge may hand out a new one). Behind a Mutex because a
    /// reconnect mutates it while submits read it.
    extranonce1_hex: Mutex<String>,
    /// extranonce2 byte width from the subscribe result (the bridge sends 4).
    extranonce2_size: AtomicU64,
    /// Set on shutdown so the reader loop exits instead of reconnecting.
    shutdown: AtomicBool,
    /// Set by the watchdog's `request_failover` (accepted-share dead-man). The
    /// reader consumes it (swap-to-false) on its next reconnect to
    /// `endpoints.advance()` BEFORE re-dialing, so the same af8c236 reconnect
    /// lands on the NEXT endpoint. One-shot; never forks `reconnect()`.
    force_failover: AtomicBool,
    /// Accept/reject/stale share counters, written by the reader on each ack.
    stats: SessionStats,
    /// The endpoint we are CURRENTLY connected to — updated by the reader on each
    /// successful (re)connect. Distinct from `StratumClient.endpoint` (the static
    /// primary) so the heartbeat + `/1/summary` show the TRUE pool after a
    /// failover. Behind a `Mutex`; written on reconnect, read by `health_snapshot`.
    current_endpoint: Mutex<String>,
    /// Cached, benchmark-derived `mining.suggest_difficulty` value, bit-encoded as
    /// an `f64` in an `AtomicU64` (lock-free, same trick as `difficulty_bits`).
    /// Seeded to `f64::NAN` = "no suggestion yet"; set once after the startup
    /// benchmark so the reconnect path can RE-send the same hint after every
    /// reconnect (the bridge resets a session to its diff floor on reconnect, so
    /// without this a reconnect would lose the head-start). A NaN/non-positive
    /// value reads back as `None` and is never sent.
    suggest_difficulty_bits: AtomicU64,
}

impl Shared {
    fn set_difficulty(&self, d: f64) {
        self.difficulty_bits.store(d.to_bits(), Ordering::Relaxed);
    }
    fn difficulty(&self) -> f64 {
        f64::from_bits(self.difficulty_bits.load(Ordering::Relaxed))
    }
    /// Cache a benchmark-derived suggest-difficulty so it can be re-sent on every
    /// reconnect. A non-finite or non-positive `d` is rejected (the cache stays
    /// `None`) — it would be malformed on the wire and must never be re-sent.
    fn set_suggest_difficulty(&self, d: f64) {
        let bits = if d.is_finite() && d > 0.0 {
            d.to_bits()
        } else {
            f64::NAN.to_bits()
        };
        self.suggest_difficulty_bits.store(bits, Ordering::Relaxed);
    }
    /// The cached suggest-difficulty, or `None` if none has been set (the NaN
    /// sentinel) — used by the reconnect path to decide whether to re-send.
    fn suggest_difficulty(&self) -> Option<f64> {
        let d = f64::from_bits(self.suggest_difficulty_bits.load(Ordering::Relaxed));
        (d.is_finite() && d > 0.0).then_some(d)
    }
}

/// A connected Stratum v1 client.
pub struct StratumClient {
    endpoint: String,
    worker_addr: String,
    shared: Arc<Shared>,
    /// Write half of the socket. The reader thread holds its own cloned read
    /// handle; this one is used by `send_submit` and the reconnect path.
    writer: Arc<Mutex<TcpStream>>,
    /// Monotonic JSON-RPC request id source for outgoing submits.
    next_id: AtomicU64,
    /// Reader-thread handle. `Option` so `Drop` can `join()` after signalling
    /// shutdown.
    reader: Option<JoinHandle<()>>,
    /// Optional stats sink (D2): when the operator runs `--stats-port`, the
    /// loop's hashrate samples + live health are pushed here for the telemetry
    /// server. `None` ⇒ no stats endpoint and zero overhead.
    stats: Option<Arc<StatsHandle>>,
    /// Optional G6 Discord notifier. `None` until `--discord-webhook` wires one
    /// in via [`StratumClient::attach_notifier`]; when set (and NOT
    /// `--discord-solutions-only`), a heartbeat sample where the accepted-share
    /// total grew posts the running total. A pool miner can't observe block-found
    /// client-side, so `--discord-solutions-only` ⇒ the pool path stays silent.
    notifier: Option<Arc<DiscordNotifier>>,
    /// High-water mark of the accepted total we last notified on, so the
    /// heartbeat only posts when `stats.accepted` has grown (no duplicate pings).
    last_notified_accepted: AtomicU64,
}

/// Result of one handshake attempt.
///
/// We hand back the *buffered reader* the handshake used (not a fresh clone)
/// so any server pushes the bridge sent immediately after the authorize reply
/// — and which `BufReader` may have already pulled into its internal buffer —
/// are preserved for the reader thread instead of being silently dropped with
/// the buffer. `write_stream` is a separate clone of the same socket for the
/// writer mutex.
struct Handshake {
    reader: BufReader<TcpStream>,
    write_stream: TcpStream,
    subscribe: SubscribeResult,
    /// Push frames (notify/set_difficulty) the bridge bunched in BEFORE the
    /// authorize reply, consumed by the handshake loop. Replayed through
    /// `dispatch_frame` once `Shared` exists so the first job/difficulty isn't
    /// lost. (Pushes that arrive AFTER the authorize reply stay buffered in
    /// `reader` and are handled by the reader thread directly.)
    early_pushes: Vec<String>,
}

impl StratumClient {
    /// Connect to a single `endpoint` (`host:port`) with no failover. Thin shim
    /// over [`connect_failover`](Self::connect_failover) on a one-element list,
    /// preserving every existing caller/test on a no-failover path.
    pub fn connect(endpoint: &str, worker_addr: &str) -> Result<Self> {
        Self::connect_failover(&[endpoint.to_string()], worker_addr)
    }

    /// Connect to the PRIMARY pool (`endpoints.first()`), run the subscribe +
    /// authorize handshake, capture `extranonce1`/`extranonce2_size`, confirm
    /// authorize returned `true` (bail otherwise), then spawn the reader thread
    /// with the full failover [`EndpointList`]. On a dropped connection the
    /// reader rotates through `endpoints` (and fails back to the primary after a
    /// quiet interval); a single-element list is the no-failover path.
    pub fn connect_failover(endpoints: &[String], worker_addr: &str) -> Result<Self> {
        let endpoint = endpoints
            .first()
            .ok_or_else(|| anyhow!("connect_failover needs at least one endpoint"))?;

        let hs = Self::handshake(endpoint, worker_addr)
            .with_context(|| format!("stratum handshake to {endpoint}"))?;

        let shared = Arc::new(Shared {
            latest_job: Mutex::new(None),
            difficulty_bits: AtomicU64::new(1.0f64.to_bits()),
            extranonce1_hex: Mutex::new(hs.subscribe.extranonce1_hex.clone()),
            extranonce2_size: AtomicU64::new(hs.subscribe.extranonce2_size as u64),
            shutdown: AtomicBool::new(false),
            force_failover: AtomicBool::new(false),
            stats: SessionStats::default(),
            current_endpoint: Mutex::new(endpoint.to_string()),
            // No benchmark has run yet at connect time (the startup benchmark in
            // `drive()` happens AFTER connect); seed the cache empty (NaN).
            suggest_difficulty_bits: AtomicU64::new(f64::NAN.to_bits()),
        });

        let extranonce1 = hs.subscribe.extranonce1_hex.clone();
        let xn2_size = hs.subscribe.extranonce2_size;

        // The reader thread takes the handshake's buffered reader directly (so
        // early pushes already in its buffer aren't lost); the writer mutex
        // takes the separate write-side clone of the same socket.
        let writer = Arc::new(Mutex::new(hs.write_stream));

        // Replay pushes the bridge sent before the authorize reply (consumed by
        // the handshake loop). Done now that `shared` exists and before the
        // reader thread starts, so the first job/difficulty is available
        // immediately instead of waiting for the next push.
        for push in &hs.early_pushes {
            dispatch_frame(push, &shared);
        }

        // Hand the reader the FULL ordered endpoint list (index 0 = primary it
        // is already connected to) so its reconnect path can rotate to a backup
        // and fail back. A one-element list is a no-op rotation.
        let reader = Self::spawn_reader(
            EndpointList::new(endpoints.to_vec()),
            worker_addr.to_string(),
            hs.reader,
            Arc::clone(&shared),
            Arc::clone(&writer),
        );

        tracing::info!(
            "stratum: connected to {endpoint} (extranonce1={extranonce1}, xn2_size={xn2_size})"
        );

        Ok(StratumClient {
            endpoint: endpoint.to_string(),
            worker_addr: worker_addr.to_string(),
            shared,
            writer,
            next_id: AtomicU64::new(100),
            reader: Some(reader),
            stats: None,
            notifier: None,
            last_notified_accepted: AtomicU64::new(0),
        })
    }

    /// One full TCP connect + subscribe + authorize round. Used by both the
    /// initial `connect()` and the reader's reconnect path. Leaves the read
    /// timeout set on the returned stream so subsequent reads can't hang.
    fn handshake(endpoint: &str, worker_addr: &str) -> Result<Handshake> {
        let addr = endpoint
            .to_socket_addrs()
            .with_context(|| format!("resolving stratum endpoint {endpoint}"))?
            .next()
            .ok_or_else(|| anyhow!("no socket address resolved for {endpoint}"))?;

        let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .with_context(|| format!("connecting to {endpoint}"))?;
        stream.set_read_timeout(Some(READ_TIMEOUT)).ok();
        stream.set_nodelay(true).ok();

        // A buffered reader over a cloned handle for the synchronous handshake
        // replies. We deliberately hand this same reader to the persistent
        // reader thread afterwards so no early server pushes that landed in its
        // buffer are lost. `writer` is a second clone used for the writer mutex.
        let mut reader = BufReader::new(
            stream
                .try_clone()
                .context("cloning stream for handshake reader")?,
        );
        let mut writer = stream
            .try_clone()
            .context("cloning stream for handshake writer")?;
        // `stream` itself is dropped at end of scope; the two clones above keep
        // the OS socket alive (one for reading, one for writing).

        // --- mining.subscribe ---
        let sub_req = subscribe_request(1);
        writer
            .write_all(serialize_line(&sub_req)?.as_bytes())
            .context("sending mining.subscribe")?;
        writer.flush().ok();

        // --- mining.authorize ---
        let auth_req = authorize_request(2, worker_addr);
        writer
            .write_all(serialize_line(&auth_req)?.as_bytes())
            .context("sending mining.authorize")?;
        writer.flush().ok();

        // Read frames until we've captured both the subscribe result and the
        // authorize result. The bridge may interleave a notify/set_difficulty
        // push before the authorize reply, so match on `id` rather than order.
        let mut subscribe: Option<SubscribeResult> = None;
        let mut authorized: Option<bool> = None;
        // Pushes the bridge interleaves before the authorize reply. We must NOT
        // drop them: the real bridge sends the first set_difficulty + notify
        // right after the subscribe result, i.e. before the id=2 reply we are
        // still waiting for here. Captured raw and replayed via `dispatch_frame`
        // once `Shared` exists (see connect/reconnect), so the first job isn't
        // lost (which manifested as the miner hanging on "waiting for first
        // mining.notify").
        let mut early_pushes: Vec<String> = Vec::new();

        let mut line = String::new();
        while subscribe.is_none() || authorized.is_none() {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .context("reading stratum handshake reply")?;
            if n == 0 {
                return Err(anyhow!("connection closed during handshake"));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Responses to our calls carry our id (1 = subscribe, 2 =
            // authorize). Pushes (notify/set_difficulty) have id:null and a
            // `method`; capture them for replay rather than dropping them.
            let resp: Response = match serde_json::from_str(trimmed) {
                Ok(r) => r,
                Err(_) => {
                    early_pushes.push(trimmed.to_string()); // a push frame, not a response
                    continue;
                }
            };
            match resp.id {
                Some(1) => {
                    if let Some(err) = &resp.error {
                        if !err.is_null() {
                            return Err(anyhow!("mining.subscribe failed: {err}"));
                        }
                    }
                    subscribe = Some(SubscribeResult::parse(&resp.result)?);
                }
                Some(2) => {
                    if let Some(err) = &resp.error {
                        if !err.is_null() {
                            return Err(anyhow!("mining.authorize rejected: {err}"));
                        }
                    }
                    authorized = Some(resp.result.as_bool().unwrap_or(false));
                }
                _ => {
                    early_pushes.push(trimmed.to_string()); // id:null push interleaved in
                    continue;
                }
            }
        }

        let subscribe = subscribe.expect("loop exits only once Some");
        if authorized != Some(true) {
            return Err(anyhow!(
                "mining.authorize returned false for worker {worker_addr}"
            ));
        }

        Ok(Handshake {
            reader,
            write_stream: writer,
            subscribe,
            early_pushes,
        })
    }

    /// Spawn the background reader thread. It loops over `\n`-delimited frames,
    /// dispatching `mining.notify` → latest_job and `mining.set_difficulty` →
    /// difficulty. On any read error/timeout/EOF it reconnects with capped
    /// backoff (re-subscribe/re-authorize) unless shutdown was signalled.
    fn spawn_reader(
        endpoints: EndpointList,
        worker_addr: String,
        initial_reader: BufReader<TcpStream>,
        shared: Arc<Shared>,
        writer: Arc<Mutex<TcpStream>>,
    ) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("stratum-reader".to_string())
            .spawn(move || {
                let mut endpoints = endpoints;
                let mut reader = initial_reader;
                let mut backoff = BACKOFF_MIN;
                let mut line = String::new();

                loop {
                    if shared.shutdown.load(Ordering::Relaxed) {
                        return;
                    }

                    // Do NOT clear `line` here: a benign idle timeout
                    // (ReadAction::Idle) may have left a partial frame buffered,
                    // and the next read_line must append the rest. We clear only
                    // after dispatching a frame or before reconnecting.
                    let read = reader.read_line(&mut line);
                    match classify_read(&read) {
                        ReadAction::Frame => {
                            // A submit ack (accepted/rejected/stale) is counted
                            // here; anything else is a server push handed to
                            // dispatch_frame (jobs / difficulty).
                            let frame = line.trim();
                            if !record_ack(frame, &shared.stats) {
                                dispatch_frame(frame, &shared);
                            }
                            line.clear();
                            backoff = BACKOFF_MIN; // healthy read resets backoff
                        }
                        ReadAction::Idle => {
                            // Benign idle read-timeout (SO_RCVTIMEO): the bridge
                            // just hasn't pushed a frame within READ_TIMEOUT. The
                            // link is fine — keep the session and keep waiting.
                            // This is the fix for the `os error 11` reconnect storm.
                        }
                        ReadAction::Reconnect => {
                            if shared.shutdown.load(Ordering::Relaxed) {
                                return;
                            }
                            let current = endpoints.current().to_string();
                            if let Err(e) = &read {
                                tracing::warn!(
                                    "stratum: read error from {current}: {e}; reconnecting"
                                );
                            } else {
                                tracing::warn!(
                                    "stratum: connection closed by {current}, reconnecting"
                                );
                            }
                            // v0.1.9 #2: if the watchdog armed a failover, rotate
                            // to the next endpoint BEFORE reconnecting so the same
                            // af8c236 reconnect re-dials a DIFFERENT pool (the
                            // current one acks + pushes work but won't credit us).
                            // One-shot (swap-to-false): a later plain reconnect
                            // stays on the current endpoint. NOTE: the flag is
                            // consumed by whichever reconnect runs NEXT — a failover
                            // armed while the reader is already mid-reconnect is
                            // carried by the following reconnect. Intentional and
                            // benign: at worst one extra rotation, undone by
                            // maybe_failback after FAILBACK_MS.
                            if shared.force_failover.swap(false, Ordering::Relaxed) {
                                endpoints.advance();
                                // Telemetry: count this failover (flaky-primary signal).
                                shared.stats.failovers.fetch_add(1, Ordering::Relaxed);
                                tracing::warn!(
                                    "stratum: failover armed — rotating to {}",
                                    endpoints.current()
                                );
                            }
                            line.clear();
                            if !reconnect(
                                &mut endpoints,
                                &worker_addr,
                                &shared,
                                &writer,
                                &mut reader,
                                &mut backoff,
                            ) {
                                return; // shutdown requested mid-reconnect
                            }
                        }
                    }
                }
            })
            .expect("spawning stratum reader thread")
    }

    /// Latest job pushed by the pool, or `None` if none has arrived yet.
    pub fn latest_job(&self) -> Option<StratumJob> {
        self.shared
            .latest_job
            .lock()
            .ok()
            .and_then(|g| g.clone())
    }

    /// Current share difficulty (defaults to 1.0 until the pool sends one).
    pub fn current_difficulty(&self) -> f64 {
        self.shared.difficulty()
    }

    /// Session extranonce1 (hex), refreshed on reconnect.
    pub fn extranonce1_hex(&self) -> String {
        self.shared
            .extranonce1_hex
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// extranonce2 byte width the bridge expects (4).
    pub fn extranonce2_size(&self) -> usize {
        self.shared.extranonce2_size.load(Ordering::Relaxed) as usize
    }

    /// The pool endpoint this client is connected to.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The worker (csd1) address this client authorized as.
    pub fn worker_addr(&self) -> &str {
        &self.worker_addr
    }

    /// A `'static` [`WatchdogView`] handle for the reliability-watchdog thread:
    /// reads the session counters and can force a reconnect by shutting the
    /// socket (so the reader's af8c236 reconnect path fires).
    pub fn watchdog_handle(&self) -> Arc<dyn WatchdogView> {
        Arc::new(ClientWatchdog {
            shared: Arc::clone(&self.shared),
            writer: Arc::clone(&self.writer),
        })
    }

    /// Snapshot the session's share counters + job age for the heartbeat.
    pub fn health_snapshot(&self) -> HealthSnapshot {
        let s = &self.shared.stats;
        let last_job = s.last_new_job_ms.load(Ordering::Relaxed);
        let job_age_s = (last_job != 0).then(|| now_unix_ms().saturating_sub(last_job) / 1000);
        HealthSnapshot {
            accepted: s.accepted.load(Ordering::Relaxed),
            rejected: s.rejected.load(Ordering::Relaxed),
            stale: s.stale.load(Ordering::Relaxed),
            submitted: s.submitted.load(Ordering::Relaxed),
            job_age_s,
            // v0.1.9 #1: the endpoint we are CURRENTLY connected to (tracks
            // failover), falling back to the static primary on a poisoned lock.
            endpoint: self
                .shared
                .current_endpoint
                .lock()
                .map(|e| e.clone())
                .unwrap_or_else(|_| self.endpoint.clone()),
            reconnects: s.reconnects.load(Ordering::Relaxed),
            failovers: s.failovers.load(Ordering::Relaxed),
        }
    }

    /// Attach a stats sink (D2). Called once at startup when `--stats-port` is
    /// set, before the mining loop borrows the client (`&mut self`); `None`
    /// until then, so the unconfigured build carries no stats overhead.
    pub fn attach_stats(&mut self, handle: Arc<StatsHandle>) {
        self.stats = Some(handle);
    }

    /// Push a combined-hashrate sample (GH/s) plus the current health snapshot
    /// to the attached stats sink, if any. No-op when `--stats-port` is off.
    pub fn record_hashrate_sample(&self, ghs: f64) {
        if let Some(s) = &self.stats {
            s.record(ghs);
            s.set_health(self.health_snapshot());
        }
    }

    /// Attach a G6 Discord notifier. Called once at startup when
    /// `--discord-webhook` is set, before the mining loop borrows the client
    /// (`&mut self`); `None` until then, so the unconfigured build carries no
    /// notify overhead. When set (and not `--discord-solutions-only`), the
    /// heartbeat posts an accepted-share milestone whenever the total grows.
    pub fn attach_notifier(&mut self, n: Arc<DiscordNotifier>) {
        self.notifier = Some(n);
    }

    /// Heartbeat-driven accepted-share milestone post (G6). Called from the
    /// loop's 30s heartbeat (NOT the share path). No-op unless a notifier is
    /// attached. With `--discord-solutions-only` the pool stays silent (a pool
    /// miner never learns it solved a block, so there is no "solution" to post);
    /// otherwise, if `stats.accepted` has grown since the last post, it sends the
    /// running total. Best-effort + detached — never blocks the heartbeat. The
    /// pure decision (what total, if any, to post) is factored into
    /// [`milestone_to_post`] so it is unit-tested without a socket.
    pub fn notify_heartbeat_sample(&self) {
        if let Some(n) = &self.notifier {
            let acc = self.shared.stats.accepted.load(Ordering::Relaxed);
            let last = self.last_notified_accepted.load(Ordering::Relaxed);
            if let Some(total) = milestone_to_post(acc, last, n.solutions_only()) {
                self.last_notified_accepted.store(total, Ordering::Relaxed);
                n.post(share_accepted_message(total, &self.worker_addr));
            }
        }
    }

    /// Cache a benchmark-derived suggest-difficulty on the shared session so the
    /// reconnect path re-sends it after every reconnect (the bridge resets a new
    /// session to its diff floor, so a reconnect would otherwise lose the
    /// head-start). Pairs with [`Self::send_suggest_difficulty`], which puts it on
    /// the wire now. A non-finite / non-positive `d` is rejected by the cache (it
    /// stays empty) and is never re-sent.
    pub fn set_suggest_difficulty(&self, d: f64) {
        self.shared.set_suggest_difficulty(d);
    }

    /// The cached suggest-difficulty, if one was set. Exposed for tests / callers
    /// that want to know whether a startup benchmark produced a usable hint.
    pub fn cached_suggest_difficulty(&self) -> Option<f64> {
        self.shared.suggest_difficulty()
    }

    /// Send a single `mining.suggest_difficulty` line carrying `d`, serialized
    /// through the writer mutex exactly like [`Self::send_submit`]. Fire-and-
    /// forget: the bridge does not ack a suggest, so this only reports a local
    /// serialize/write error (the caller logs it and keeps mining — a missed
    /// suggest just means vardiff ramps from the floor, never a brick). Does NOT
    /// itself cache `d`; call [`Self::set_suggest_difficulty`] for the
    /// re-send-on-reconnect behaviour.
    pub fn send_suggest_difficulty(&self, d: f64) -> Result<()> {
        let mut w = self
            .writer
            .lock()
            .map_err(|_| anyhow!("stratum writer mutex poisoned"))?;
        send_suggest_difficulty_locked(&mut w, d)
    }

    /// Send a `mining.submit` line for a found share. Serializes writes through
    /// the writer mutex. Errors bubble up so the caller can log/account them.
    pub fn send_submit(
        &self,
        worker: &str,
        job_id: &str,
        xn2_hex: &str,
        ntime_hex: &str,
        nonce_hex: &str,
    ) -> Result<()> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = submit_request(id, worker, job_id, xn2_hex, ntime_hex, nonce_hex);
        let line = serialize_line(&req)?;
        let mut w = self
            .writer
            .lock()
            .map_err(|_| anyhow!("stratum writer mutex poisoned"))?;
        w.write_all(line.as_bytes())
            .context("writing mining.submit")?;
        w.flush().ok();
        // Account the submit: one more pending share until an ack resets the
        // streak (the submit-ack watchdog watches `consecutive_unacked`).
        self.shared.stats.submitted.fetch_add(1, Ordering::Relaxed);
        self.shared
            .stats
            .consecutive_unacked
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for StratumClient {
    fn drop(&mut self) {
        // Signal the reader to stop and nudge the socket so a blocked
        // `read_line` returns promptly, then join.
        self.shared.shutdown.store(true, Ordering::Relaxed);
        if let Ok(w) = self.writer.lock() {
            let _ = w.shutdown(std::net::Shutdown::Both);
        }
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}

/// Watchdog handle backed by the live client's shared state + writer socket.
struct ClientWatchdog {
    shared: Arc<Shared>,
    writer: Arc<Mutex<TcpStream>>,
}

impl WatchdogView for ClientWatchdog {
    fn snapshot(&self) -> WatchdogSnapshot {
        WatchdogSnapshot {
            consecutive_unacked: self.shared.stats.consecutive_unacked.load(Ordering::Relaxed),
            last_new_job_ms: self.shared.stats.last_new_job_ms.load(Ordering::Relaxed),
            last_accept_ms: self.shared.stats.last_accept_ms.load(Ordering::Relaxed),
            last_reconnect_ms: self.shared.stats.last_reconnect_ms.load(Ordering::Relaxed),
        }
    }
    fn request_reconnect(&self) {
        // Shut the socket so the reader's existing reconnect path (the
        // af8c236-blessed classify_read → reconnect) fires promptly — never a
        // forked reconnect.
        if let Ok(w) = self.writer.lock() {
            let _ = w.shutdown(std::net::Shutdown::Both);
        }
        tracing::warn!("stratum: watchdog requested reconnect (shutting socket)");
    }
    fn request_failover(&self) {
        // Arm the one-shot failover flag, THEN shut the socket. The reader's
        // existing reconnect arm consumes the flag and `endpoints.advance()`s
        // before re-dialing, so the SAME af8c236 reconnect lands on the next
        // endpoint — reconnect() is never forked. Set the flag BEFORE the
        // shutdown so it's always visible by the time the reader wakes.
        self.shared.force_failover.store(true, Ordering::Relaxed);
        if let Ok(w) = self.writer.lock() {
            let _ = w.shutdown(std::net::Shutdown::Both);
        }
        tracing::warn!("stratum: watchdog requested FAILOVER (rotate endpoint + reconnect)");
    }
}

/// Write one `mining.suggest_difficulty(d)` frame to an already-locked writer
/// stream. Shared by [`StratumClient::send_suggest_difficulty`] (startup send,
/// holding `self.writer`) and the reconnect path (which holds the freshly-swapped
/// writer). Fire-and-forget: serialize → write → best-effort flush; a write error
/// bubbles up so the caller can log it, but a missed suggest never stops mining
/// (vardiff just ramps from the floor). Mirrors the submit write in `send_submit`.
fn send_suggest_difficulty_locked(w: &mut TcpStream, d: f64) -> Result<()> {
    let req = suggest_difficulty_request(d);
    let line = serialize_line(&req)?;
    w.write_all(line.as_bytes())
        .context("writing mining.suggest_difficulty")?;
    w.flush().ok();
    Ok(())
}

/// Parse one received line and update [`Shared`] accordingly. Recognizes the
/// two server pushes we care about (`mining.notify`, `mining.set_difficulty`)
/// and silently ignores everything else (submit acks, keep-alives, blank lines,
/// unparseable junk) — a malformed push must never take the reader down.
///
/// Kept as a free function taking `&Shared` so it is unit-testable without a
/// socket: feed it a canned line and assert the resulting state.
fn dispatch_frame(line: &str, shared: &Shared) {
    if line.is_empty() {
        return;
    }
    let note: Notification = match serde_json::from_str(line) {
        Ok(n) => n,
        // Not a notification (e.g. a submit response with an `id`, or junk):
        // nothing for the reader to track here.
        Err(_) => return,
    };

    match note.method.as_str() {
        "mining.notify" => match NotifyParams::parse(&note.params) {
            Ok(notify) => {
                let extranonce1_hex = shared
                    .extranonce1_hex
                    .lock()
                    .map(|g| g.clone())
                    .unwrap_or_default();
                let job_id = notify.job_id.clone();
                let clean = notify.clean_jobs;
                if let Ok(mut slot) = shared.latest_job.lock() {
                    // Stamp the staleness clock only on a *changed* job_id: a
                    // same-id resend must not mask a stalled pool (G3).
                    let changed = slot
                        .as_ref()
                        .map(|j| j.notify.job_id != job_id)
                        .unwrap_or(true);
                    *slot = Some(StratumJob {
                        notify,
                        extranonce1_hex,
                    });
                    if changed {
                        shared
                            .stats
                            .last_new_job_ms
                            .store(now_unix_ms(), Ordering::Relaxed);
                    }
                }
                tracing::debug!("stratum: new job {job_id} (clean_jobs={clean})");
            }
            Err(e) => tracing::warn!("stratum: bad mining.notify, ignoring: {e}"),
        },
        "mining.set_difficulty" => {
            match note
                .params
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_f64())
            {
                Some(d) => {
                    shared.set_difficulty(d);
                    tracing::debug!("stratum: set_difficulty = {d}");
                }
                None => tracing::warn!(
                    "stratum: bad mining.set_difficulty params, ignoring: {}",
                    note.params
                ),
            }
        }
        // mining.set_extranonce, client.reconnect, etc. are not handled here
        // (out of scope for Task 2); ignore them.
        _ => {}
    }
}

/// Reconnect with capped exponential backoff: sleep, re-run the handshake, and
/// on success swap in the fresh write half + reader stream and refresh the
/// session extranonce1/size. Returns `false` iff shutdown was requested (so the
/// reader loop should exit); `true` once reconnected.
///
/// Failover: each attempt dials `endpoints.current()`. A failed handshake
/// rotates to the next pool via [`EndpointList::advance`] (a no-op for a single
/// endpoint); after a quiet interval on a backup, [`EndpointList::maybe_failback`]
/// brings the next attempt back to the primary. The backoff doubles each failed
/// attempt up to [`BACKOFF_MAX`]; a successful reconnect resets it (the caller
/// also resets on the next healthy read).
fn reconnect(
    endpoints: &mut EndpointList,
    worker_addr: &str,
    shared: &Arc<Shared>,
    writer: &Arc<Mutex<TcpStream>>,
    reader: &mut BufReader<TcpStream>,
    backoff: &mut Duration,
) -> bool {
    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            return false;
        }

        std::thread::sleep(*backoff);

        if shared.shutdown.load(Ordering::Relaxed) {
            return false;
        }

        // If we've been parked on a backup long enough, prefer the primary again
        // before this attempt (a quiet-period failback). On the primary this is
        // a no-op that just keeps the failback clock fresh.
        if endpoints.maybe_failback(now_unix_ms(), FAILBACK_MS) {
            tracing::info!("stratum: failback — retrying primary {}", endpoints.current());
        }
        let ep = endpoints.current().to_string();

        match StratumClient::handshake(&ep, worker_addr) {
            Ok(hs) => {
                // Refresh session params the bridge may have rotated.
                if let Ok(mut x) = shared.extranonce1_hex.lock() {
                    *x = hs.subscribe.extranonce1_hex.clone();
                }
                shared
                    .extranonce2_size
                    .store(hs.subscribe.extranonce2_size as u64, Ordering::Relaxed);

                // Replay pushes bunched in before the authorize reply (consumed
                // by the handshake), so a reconnect re-seeds the current
                // job/difficulty immediately too.
                for push in &hs.early_pushes {
                    dispatch_frame(push, shared);
                }

                // Install the new socket: the reader continues from the fresh
                // handshake's buffered reader (preserving any early pushes), and
                // the shared writer is swapped to the new write-side stream.
                *reader = hs.reader;
                if let Ok(mut w) = writer.lock() {
                    // Best-effort close of the dead socket before swap.
                    let _ = w.shutdown(std::net::Shutdown::Both);
                    *w = hs.write_stream;
                    // Reset the un-acked streak under the SAME writer lock the
                    // watchdog must hold to shut the socket, so a stale streak
                    // can't tear down the brand-new connection (review M1).
                    shared.stats.consecutive_unacked.store(0, Ordering::Relaxed);
                    // Re-send the benchmark-derived suggest-difficulty over the
                    // FRESH socket (a reconnect starts a new pool session at the
                    // diff floor, so without this the head-start is lost on every
                    // reconnect). Done under the same writer lock, right after the
                    // swap. Fail-safe: a write error here is logged and ignored —
                    // mining continues and vardiff just ramps from the floor.
                    if let Some(d) = shared.suggest_difficulty() {
                        match send_suggest_difficulty_locked(&mut w, d) {
                            Ok(()) => tracing::info!(
                                "stratum: re-sent mining.suggest_difficulty({d:.2}) after reconnect"
                            ),
                            Err(e) => tracing::info!(
                                "stratum: suggest_difficulty re-send after reconnect failed (continuing): {e}"
                            ),
                        }
                    }
                }
                *backoff = BACKOFF_MIN;
                tracing::info!("stratum: reconnected to {ep}");
                // v0.1.9 #1: record the endpoint we actually reconnected to so the
                // heartbeat + /1/summary report the live pool after a failover.
                if let Ok(mut cur) = shared.current_endpoint.lock() {
                    *cur = ep.to_string();
                }
                // v0.1.9 #2: stamp the reconnect so the accepted-share dead-man's
                // clock restarts here — a fresh connection is as good as an accept
                // for "is this endpoint crediting us?", and this bounds a never-
                // crediting endpoint to one failover per `accept_stale` (no storm).
                shared
                    .stats
                    .last_reconnect_ms
                    .store(now_unix_ms(), Ordering::Relaxed);
                // Telemetry: count this successful reconnect (connection-churn signal).
                shared.stats.reconnects.fetch_add(1, Ordering::Relaxed);
                return true;
            }
            Err(e) => {
                tracing::warn!("stratum: reconnect to {ep} failed: {e}; trying next endpoint");
                // Rotate to the next pool in the failover list (no-op for a
                // single endpoint) so the next attempt dials a different one.
                endpoints.advance();
            }
        }

        // Bump backoff, capped.
        *backoff = (*backoff * 2).min(BACKOFF_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::Instant;

    /// Build a fresh `Shared` with defaults, as `connect()` would.
    fn fresh_shared(xn1: &str, xn2_size: u64) -> Arc<Shared> {
        Arc::new(Shared {
            latest_job: Mutex::new(None),
            difficulty_bits: AtomicU64::new(1.0f64.to_bits()),
            extranonce1_hex: Mutex::new(xn1.to_string()),
            extranonce2_size: AtomicU64::new(xn2_size),
            shutdown: AtomicBool::new(false),
            force_failover: AtomicBool::new(false),
            stats: SessionStats::default(),
            current_endpoint: Mutex::new("test-pool:3333".to_string()),
            suggest_difficulty_bits: AtomicU64::new(f64::NAN.to_bits()),
        })
    }

    #[test]
    fn milestone_to_post_gates_on_growth_and_solutions_only() {
        // solutions_only ⇒ always silent (pool can't observe block-found).
        assert_eq!(milestone_to_post(10, 0, true), None);
        assert_eq!(milestone_to_post(10, 5, true), None);
        // Not solutions_only: post the new total iff it grew past last_notified.
        assert_eq!(milestone_to_post(10, 5, false), Some(10));
        assert_eq!(milestone_to_post(1, 0, false), Some(1)); // first accept
        // No growth ⇒ no duplicate ping.
        assert_eq!(milestone_to_post(5, 5, false), None);
        assert_eq!(milestone_to_post(3, 5, false), None); // never decreases
        assert_eq!(milestone_to_post(0, 0, false), None); // nothing accepted yet
    }

    #[test]
    fn idle_read_timeout_keeps_the_session() {
        // SO_RCVTIMEO firing on a quiet socket surfaces as WouldBlock/TimedOut
        // (EAGAIN, "os error 11"); a signal as Interrupted. None of these mean
        // the link is dead — the reader must keep waiting, never reconnect.
        for kind in [
            std::io::ErrorKind::WouldBlock,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::Interrupted,
        ] {
            let r: std::io::Result<usize> = Err(std::io::Error::from(kind));
            assert_eq!(
                classify_read(&r),
                ReadAction::Idle,
                "{kind:?} must stay idle, not reconnect"
            );
        }
    }

    #[test]
    fn eof_and_real_errors_reconnect() {
        assert_eq!(classify_read(&Ok(0)), ReadAction::Reconnect); // peer closed
        for kind in [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionAborted,
        ] {
            let r: std::io::Result<usize> = Err(std::io::Error::from(kind));
            assert_eq!(
                classify_read(&r),
                ReadAction::Reconnect,
                "{kind:?} must reconnect"
            );
        }
    }

    #[test]
    fn full_frame_is_processed() {
        assert_eq!(classify_read(&Ok(42)), ReadAction::Frame);
    }

    #[test]
    fn classify_ack_accepts_rejects_and_stale() {
        assert_eq!(
            classify_ack(r#"{"id":101,"result":true,"error":null}"#),
            Some(Ack::Accepted)
        );
        // result:false with no error → generic reject.
        assert_eq!(
            classify_ack(r#"{"id":102,"result":false,"error":null}"#),
            Some(Ack::Rejected("rejected".to_string()))
        );
        // error array [code, message, data] → reject carrying the message.
        assert_eq!(
            classify_ack(r#"{"id":103,"result":null,"error":[23,"Low difficulty share",null]}"#),
            Some(Ack::Rejected("Low difficulty share".to_string()))
        );
        // stale / job-not-found → Stale sub-class.
        assert_eq!(
            classify_ack(r#"{"id":104,"result":null,"error":[21,"Job not found",null]}"#),
            Some(Ack::Stale)
        );
        assert_eq!(
            classify_ack(r#"{"id":105,"result":null,"error":[21,"stale share",null]}"#),
            Some(Ack::Stale)
        );
        // Not submit acks: a server push (has `method`)…
        assert_eq!(
            classify_ack(r#"{"id":null,"method":"mining.notify","params":[]}"#),
            None
        );
        // …a handshake reply (id < 100)…
        assert_eq!(classify_ack(r#"{"id":2,"result":true}"#), None);
        // …and junk / blank.
        assert_eq!(classify_ack("not json"), None);
        assert_eq!(classify_ack(""), None);
    }

    #[test]
    fn record_ack_counts_into_stats() {
        let stats = SessionStats::default();
        assert!(record_ack(r#"{"id":101,"result":true}"#, &stats));
        assert!(record_ack(r#"{"id":102,"result":true}"#, &stats));
        assert!(record_ack(
            r#"{"id":103,"result":false,"error":[20,"bad",null]}"#,
            &stats
        ));
        assert!(record_ack(
            r#"{"id":104,"error":[21,"stale share",null]}"#,
            &stats
        ));
        // A server push is NOT an ack → false (caller will dispatch it).
        assert!(!record_ack(
            r#"{"id":null,"method":"mining.notify","params":[]}"#,
            &stats
        ));
        assert_eq!(stats.accepted.load(Ordering::Relaxed), 2);
        assert_eq!(stats.rejected.load(Ordering::Relaxed), 1);
        assert_eq!(stats.stale.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn record_ack_resets_unacked_and_stamps_accept() {
        let stats = SessionStats::default();
        // An accept clears a pending streak AND stamps the accept clock.
        stats.consecutive_unacked.store(7, Ordering::Relaxed);
        assert!(record_ack(r#"{"id":101,"result":true}"#, &stats));
        assert_eq!(stats.consecutive_unacked.load(Ordering::Relaxed), 0);
        assert!(stats.last_accept_ms.load(Ordering::Relaxed) > 0);
        // A reject ALSO resets the streak (the pool is responding) but does not
        // stamp last_accept.
        stats.consecutive_unacked.store(3, Ordering::Relaxed);
        assert!(record_ack(r#"{"id":102,"result":false}"#, &stats));
        assert_eq!(stats.consecutive_unacked.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn dispatch_notify_updates_latest_job() {
        let shared = fresh_shared("cafef00d", 4);
        let line = r#"{"id":null,"method":"mining.notify","params":["jX","00ff","aa","bb",["cc"],"01000000","1d00ffff","60c0babe",true]}"#;
        dispatch_frame(line, &shared);
        let job = shared.latest_job.lock().unwrap().clone().unwrap();
        assert_eq!(job.notify.job_id, "jX");
        assert_eq!(job.notify.ntime_hex, "60c0babe");
        // extranonce1 is stitched in from the session at dispatch time.
        assert_eq!(job.extranonce1_hex, "cafef00d");
    }

    #[test]
    fn dispatch_set_difficulty_updates_difficulty() {
        let shared = fresh_shared("00", 4);
        assert_eq!(shared.difficulty(), 1.0); // default before any push
        let line = r#"{"id":null,"method":"mining.set_difficulty","params":[2048.5]}"#;
        dispatch_frame(line, &shared);
        assert_eq!(shared.difficulty(), 2048.5);
    }

    #[test]
    fn suggest_difficulty_cache_starts_none_and_round_trips() {
        let shared = fresh_shared("00", 4);
        // Fresh session: no benchmark-derived suggestion cached yet.
        assert_eq!(shared.suggest_difficulty(), None);
        // Caching a real value makes it Some(d), so the reconnect path can re-send.
        shared.set_suggest_difficulty(4096.0);
        assert_eq!(shared.suggest_difficulty(), Some(4096.0));
        // Re-caching overwrites (a re-benchmark could, in principle, change it).
        shared.set_suggest_difficulty(8192.0);
        assert_eq!(shared.suggest_difficulty(), Some(8192.0));
    }

    #[test]
    fn suggest_difficulty_cache_rejects_bogus_values() {
        let shared = fresh_shared("00", 4);
        // A non-finite or non-positive value must never become a cached "Some"
        // (it would be malformed on the wire on every reconnect). Caching it
        // leaves the cache empty / None.
        shared.set_suggest_difficulty(f64::NAN);
        assert_eq!(shared.suggest_difficulty(), None);
        shared.set_suggest_difficulty(f64::INFINITY);
        assert_eq!(shared.suggest_difficulty(), None);
        shared.set_suggest_difficulty(0.0);
        assert_eq!(shared.suggest_difficulty(), None);
        shared.set_suggest_difficulty(-1.0);
        assert_eq!(shared.suggest_difficulty(), None);
    }

    #[test]
    fn dispatch_stamps_last_new_job_only_on_changed_id() {
        let shared = fresh_shared("00", 4);
        let job_a = r#"{"id":null,"method":"mining.notify","params":["A","p","a","b",[],"01000000","1d00ffff","60c0babe",true]}"#;
        dispatch_frame(job_a, &shared);
        let t1 = shared.stats.last_new_job_ms.load(Ordering::Relaxed);
        assert!(t1 > 0, "a new job stamps the staleness clock");
        // Same job_id again → the clock must NOT move (a resend can't mask a
        // stalled pool).
        dispatch_frame(job_a, &shared);
        assert_eq!(shared.stats.last_new_job_ms.load(Ordering::Relaxed), t1);
        // A different job_id is stored (the `changed` branch is taken).
        let job_b = r#"{"id":null,"method":"mining.notify","params":["B","p","a","b",[],"01000000","1d00ffff","60c0babe",true]}"#;
        dispatch_frame(job_b, &shared);
        assert_eq!(
            shared.latest_job.lock().unwrap().as_ref().unwrap().notify.job_id,
            "B"
        );
    }

    #[test]
    fn dispatch_ignores_junk_and_unknown_methods() {
        let shared = fresh_shared("00", 4);
        // None of these should panic or mutate state.
        dispatch_frame("", &shared);
        dispatch_frame("not json at all", &shared);
        dispatch_frame(r#"{"id":7,"result":true,"error":null}"#, &shared); // submit ack
        dispatch_frame(
            r#"{"id":null,"method":"mining.set_extranonce","params":["ab",4]}"#,
            &shared,
        );
        assert!(shared.latest_job.lock().unwrap().is_none());
        assert_eq!(shared.difficulty(), 1.0);
    }

    #[test]
    fn dispatch_bad_notify_does_not_clobber_state() {
        let shared = fresh_shared("00", 4);
        // First a good job, then a malformed notify (wrong arity) — the good
        // job must survive (we don't overwrite with garbage).
        dispatch_frame(
            r#"{"id":null,"method":"mining.notify","params":["good","p","a","b",[],"01000000","1d00ffff","60c0babe",true]}"#,
            &shared,
        );
        dispatch_frame(
            r#"{"id":null,"method":"mining.notify","params":["bad","p","a"]}"#,
            &shared,
        );
        let job = shared.latest_job.lock().unwrap().clone().unwrap();
        assert_eq!(job.notify.job_id, "good");
    }

    /// End-to-end-ish smoke test against a localhost listener that plays the
    /// bridge: it accepts one connection, replies to subscribe + authorize, and
    /// pushes one set_difficulty + one notify. Asserts `connect()` completes
    /// the handshake and the reader surfaces the job + difficulty. Kept tightly
    /// scoped and self-contained so it stays reliable in CI.
    #[test]
    fn connect_handshake_and_first_job_against_fake_bridge() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut br = BufReader::new(sock.try_clone().unwrap());

            // Read the two handshake requests (subscribe id=1, authorize id=2).
            let mut req_line = String::new();
            br.read_line(&mut req_line).unwrap(); // subscribe
            req_line.clear();
            br.read_line(&mut req_line).unwrap(); // authorize

            // Reply: subscribe result (xn1="abcd1234", xn2_size=4), then
            // authorize true, then a set_difficulty and a notify push.
            sock.write_all(
                b"{\"id\":1,\"result\":[[[\"mining.notify\",\"1\"]],\"abcd1234\",4],\"error\":null}\n",
            )
            .unwrap();
            sock.write_all(b"{\"id\":2,\"result\":true,\"error\":null}\n")
                .unwrap();
            sock.write_all(b"{\"id\":null,\"method\":\"mining.set_difficulty\",\"params\":[512.0]}\n")
                .unwrap();
            sock.write_all(
                b"{\"id\":null,\"method\":\"mining.notify\",\"params\":[\"fakejob\",\"00ff\",\"aa\",\"bb\",[\"cc\"],\"01000000\",\"1d00ffff\",\"60c0babe\",true]}\n",
            )
            .unwrap();
            sock.flush().unwrap();

            // Hold the connection open briefly so the client reader can consume
            // the pushes before EOF would trigger a reconnect attempt.
            std::thread::sleep(Duration::from_millis(300));
        });

        let client =
            StratumClient::connect(&addr.to_string(), "csd1testaddr").expect("connect ok");
        assert_eq!(client.extranonce1_hex(), "abcd1234");
        assert_eq!(client.extranonce2_size(), 4);

        // Poll briefly for the pushed job/difficulty to land (reader is async).
        let mut job = None;
        for _ in 0..50 {
            if let Some(j) = client.latest_job() {
                job = Some(j);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let job = job.expect("reader surfaced the pushed job");
        assert_eq!(job.notify.job_id, "fakejob");
        assert_eq!(job.extranonce1_hex, "abcd1234");
        assert_eq!(client.current_difficulty(), 512.0);

        drop(client); // triggers reader shutdown + join
        let _ = server.join();
    }

    /// Regression test for the "waiting for first mining.notify" hang.
    ///
    /// The real bridge bunches `subscribe-result -> set_difficulty -> notify`
    /// and sends the first notify BEFORE the authorize reply. The handshake
    /// loop reads those pushes while still waiting for the id=2 response, so it
    /// must CAPTURE them (not discard) — otherwise the first job is lost and the
    /// miner waits forever for a notify that already arrived.
    #[test]
    fn connect_captures_early_notify_sent_before_authorize_reply() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut br = BufReader::new(sock.try_clone().unwrap());
            let mut req_line = String::new();
            br.read_line(&mut req_line).unwrap(); // subscribe
            req_line.clear();
            br.read_line(&mut req_line).unwrap(); // authorize

            // Real-bridge order: subscribe result, THEN set_difficulty + notify,
            // THEN the authorize reply LAST. The pushes land while the handshake
            // loop is still waiting for the id=2 authorize response.
            sock.write_all(
                b"{\"id\":1,\"result\":[[[\"mining.notify\",\"1\"]],\"abcd1234\",4],\"error\":null}\n",
            )
            .unwrap();
            sock.write_all(b"{\"id\":null,\"method\":\"mining.set_difficulty\",\"params\":[512.0]}\n")
                .unwrap();
            sock.write_all(
                b"{\"id\":null,\"method\":\"mining.notify\",\"params\":[\"earlyjob\",\"00ff\",\"aa\",\"bb\",[\"cc\"],\"01000000\",\"1d00ffff\",\"60c0babe\",true]}\n",
            )
            .unwrap();
            sock.write_all(b"{\"id\":2,\"result\":true,\"error\":null}\n")
                .unwrap();
            sock.flush().unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });

        let client =
            StratumClient::connect(&addr.to_string(), "csd1testaddr").expect("connect ok");

        // The early notify (sent before the authorize reply) must be surfaced.
        let mut job = None;
        for _ in 0..50 {
            if let Some(j) = client.latest_job() {
                job = Some(j);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let job = job.expect("early notify (pre-authorize) must be surfaced, not discarded");
        assert_eq!(job.notify.job_id, "earlyjob");
        assert_eq!(client.current_difficulty(), 512.0);

        drop(client);
        let _ = server.join();
    }

    /// After a successful handshake, `send_suggest_difficulty(d)` must put a
    /// `mining.suggest_difficulty` line carrying `d` on the wire (id null,
    /// single-f64 params) — exactly as `send_submit` puts a submit line. The fake
    /// bridge here captures everything the client writes and asserts the suggest
    /// frame appears with the right value.
    #[test]
    fn send_suggest_difficulty_puts_frame_on_wire() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().unwrap();

        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut br = BufReader::new(sock.try_clone().unwrap());
            // Handshake: read subscribe + authorize, reply.
            let mut line = String::new();
            br.read_line(&mut line).unwrap(); // subscribe
            line.clear();
            br.read_line(&mut line).unwrap(); // authorize
            sock.write_all(
                b"{\"id\":1,\"result\":[[[\"mining.notify\",\"1\"]],\"abcd1234\",4],\"error\":null}\n",
            )
            .unwrap();
            sock.write_all(b"{\"id\":2,\"result\":true,\"error\":null}\n")
                .unwrap();
            sock.flush().unwrap();
            // Now forward every subsequent line the client writes to the test.
            loop {
                line.clear();
                match br.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(line.trim().to_string()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let client = StratumClient::connect(&addr.to_string(), "csd1testaddr").expect("connect ok");
        // Cache + send a benchmark-derived suggestion.
        client.set_suggest_difficulty(4096.0);
        client.send_suggest_difficulty(4096.0).expect("send ok");

        // The bridge must receive a mining.suggest_difficulty frame with d=4096.
        let mut saw = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(frame) => {
                    if frame.contains("mining.suggest_difficulty") {
                        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
                        assert_eq!(v["method"], "mining.suggest_difficulty");
                        assert!(v["id"].is_null(), "suggest is a fire-and-forget (id null)");
                        assert_eq!(v["params"][0].as_f64().unwrap(), 4096.0);
                        saw = true;
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            }
        }
        assert!(saw, "client must send mining.suggest_difficulty after connect");

        drop(client);
        let _ = server.join();
    }

    /// Play the bridge side of one subscribe/authorize handshake on `sock`:
    /// read the two requests, then reply subscribe-result + authorize-true.
    /// Shared by the failover test's two fake pools. Returns once the handshake
    /// is written (the caller decides whether to keep the socket open or drop it).
    fn serve_handshake(sock: &mut TcpStream) {
        let mut br = BufReader::new(sock.try_clone().unwrap());
        let mut req = String::new();
        br.read_line(&mut req).unwrap(); // subscribe (id=1)
        req.clear();
        br.read_line(&mut req).unwrap(); // authorize (id=2)
        sock.write_all(
            b"{\"id\":1,\"result\":[[[\"mining.notify\",\"1\"]],\"abcd1234\",4],\"error\":null}\n",
        )
        .unwrap();
        sock.write_all(b"{\"id\":2,\"result\":true,\"error\":null}\n")
            .unwrap();
        sock.flush().unwrap();
    }

    /// Failover: when the PRIMARY pool dies after the handshake, the reader must
    /// rotate to the BACKUP via `EndpointList::advance` and re-handshake there.
    ///
    /// Two listeners stand in for pool A (primary) and pool B (backup). A
    /// accepts the first connection, completes the handshake, then drops the
    /// socket → the reader sees EOF (`ReadAction::Reconnect`). B accepts a
    /// connection and completes a full handshake, signalling (via a channel)
    /// that it received a `mining.subscribe` — i.e. the reader rotated off the
    /// dead primary onto the backup. Asserts B is reached within a bounded wait
    /// (no fixed long sleeps).
    #[test]
    fn reconnect_rotates_to_backup_on_primary_failure() {
        let listener_a = TcpListener::bind("127.0.0.1:0").expect("bind A");
        let listener_b = TcpListener::bind("127.0.0.1:0").expect("bind B");
        let addr_a = listener_a.local_addr().unwrap().to_string();
        let addr_b = listener_b.local_addr().unwrap().to_string();

        // A `done` flag lets the server threads exit once the assertion is made,
        // so neither leaks past the test.
        let done = Arc::new(AtomicBool::new(false));

        // Server A (primary): handshake the FIRST connection, then drop it so the
        // reader hits EOF and must reconnect. Crucially, keep the listener ALIVE
        // and keep accepting: the reconnect path dials A again (it's still the
        // `current` endpoint until a failed handshake advances off it), and we
        // give every such attempt an immediate EOF (accept + drop). That makes
        // A's failure FAST and deterministic — the reader can't stall on a
        // 15s connect-timeout to a closed port under parallel-test load (the
        // source of a flaky 100s+ run), it just gets EOF and advances to B.
        let done_a = Arc::clone(&done);
        let server_a = std::thread::spawn(move || {
            listener_a
                .set_nonblocking(true)
                .expect("A set_nonblocking");
            let mut first = true;
            while !done_a.load(Ordering::Relaxed) {
                match listener_a.accept() {
                    Ok((mut sock, _)) => {
                        // Accepted sockets can inherit the listener's non-blocking
                        // flag (esp. on Windows); force blocking so the handshake's
                        // read_line waits for the client's bytes instead of
                        // returning WouldBlock (a parallel-run flake source).
                        sock.set_nonblocking(false).ok();
                        if first {
                            // First connection: complete the handshake, then drop
                            // → the established session sees EOF and reconnects.
                            serve_handshake(&mut sock);
                            first = false;
                        }
                        // Drop `sock` (handshake socket OR any reconnect attempt)
                        // → immediate EOF for the client, no timeout wait.
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        // Server B (backup): accept a connection and complete the handshake,
        // reporting back (via a channel) that it received a subscribe — i.e. the
        // reader rotated off the dead primary onto the backup.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let done_b = Arc::clone(&done);
        let server_b = std::thread::spawn(move || {
            let (mut sock, _) = listener_b.accept().expect("B accept");
            serve_handshake(&mut sock);
            let _ = tx.send(()); // B got a subscribe → reader rotated here
                                 // Hold the socket open until the test is done so the client doesn't
                                 // immediately EOF-and-reconnect away mid-assertion.
            while !done_b.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        // Connect with A as primary and B as the failover backup.
        let client = StratumClient::connect_failover(
            &[addr_a.clone(), addr_b.clone()],
            "csd1testworker",
        )
        .expect("connect to primary A ok");

        // The reader should rotate from the dead A to B and handshake there.
        // Bounded wait for B to report it received a subscribe. The path is:
        // EOF on A → reconnect: sleep BACKOFF_MIN(1s) → dial A (immediate EOF as
        // A keeps accepting+dropping) → advance() to B → sleep ~2s → handshake
        // B. So ~3s typical; the channel returns the instant B is hit (no fixed
        // sleep here). An 8s ceiling is generous slack so it never flakes.
        let reached_b = rx.recv_timeout(Duration::from_secs(8)).is_ok();
        // Release the server threads before asserting (so they exit even on
        // failure) — the join below then returns promptly.
        done.store(true, Ordering::Relaxed);
        assert!(
            reached_b,
            "reader must rotate from the dead primary A to backup B and re-subscribe"
        );

        // v0.1.9 #1: the live endpoint surfaced by health_snapshot() must FOLLOW
        // the failover to B (it was the static primary A before). The reconnect
        // updates current_endpoint just after re-subscribe, so poll briefly to
        // avoid racing that write.
        let mut endpoint_followed = false;
        for _ in 0..100 {
            if client.health_snapshot().endpoint == addr_b {
                endpoint_followed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            endpoint_followed,
            "health_snapshot().endpoint must track the failover to B; got {:?}",
            client.health_snapshot().endpoint
        );

        // Telemetry: A died and we reconnected our way to B via the reconnect
        // path's OWN advance-on-handshake-failure — that is NOT a watchdog
        // failover, so the failover counter stays 0 while reconnects is non-zero.
        let hs = client.health_snapshot();
        assert_eq!(hs.failovers, 0, "primary death is a reconnect, not a failover");
        assert!(hs.reconnects >= 1, "reconnected at least once reaching B");

        drop(client);
        let _ = server_a.join();
        let _ = server_b.join();
    }

    /// Unit: `request_failover` arms the one-shot `force_failover` flag (so the
    /// reader rotates) AND shuts the socket; a plain `request_reconnect` shuts the
    /// socket WITHOUT arming failover (stays on the current endpoint).
    #[test]
    fn request_failover_arms_flag_but_reconnect_does_not() {
        let shared = fresh_shared("aabbccdd", 4);
        // A throwaway connected socket stands in for the writer half.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let wd = ClientWatchdog {
            shared: Arc::clone(&shared),
            writer: Arc::new(Mutex::new(stream)),
        };
        // A plain reconnect must NOT arm failover.
        wd.request_reconnect();
        assert!(
            !shared.force_failover.load(Ordering::Relaxed),
            "request_reconnect must not arm failover"
        );
        // A failover arms the one-shot flag for the reader to consume. The reader
        // consumes it with a single swap-to-false, after which a later plain
        // reconnect must NOT rotate again — assert that exact one-shot semantics.
        wd.request_failover();
        assert!(
            shared.force_failover.swap(false, Ordering::Relaxed),
            "request_failover must arm force_failover"
        );
        assert!(
            !shared.force_failover.load(Ordering::Relaxed),
            "the one-shot flag must be cleared after a single consume"
        );
    }

    /// Integration (v0.1.9 #2): `request_failover` rotates a HEALTHY primary A to
    /// the backup B. Unlike `reconnect_rotates_to_backup_on_primary_failure` (A
    /// dies), here A stays up and re-handshakes every dial — so the ONLY way the
    /// reader can reach B is the `force_failover` advance. If the flag weren't
    /// consumed, a reconnect would re-dial healthy A and never reach B.
    #[test]
    fn watchdog_failover_rotates_healthy_primary_to_backup() {
        let listener_a = TcpListener::bind("127.0.0.1:0").expect("bind A");
        let listener_b = TcpListener::bind("127.0.0.1:0").expect("bind B");
        let addr_a = listener_a.local_addr().unwrap().to_string();
        let addr_b = listener_b.local_addr().unwrap().to_string();
        let done = Arc::new(AtomicBool::new(false));

        // Server A (HEALTHY primary): handshake EVERY connection and hold it open.
        // A never dies, so the reader only leaves A via the failover advance.
        let done_a = Arc::clone(&done);
        let server_a = std::thread::spawn(move || {
            listener_a.set_nonblocking(true).expect("A set_nonblocking");
            let mut held: Vec<TcpStream> = Vec::new();
            while !done_a.load(Ordering::Relaxed) {
                match listener_a.accept() {
                    Ok((mut sock, _)) => {
                        sock.set_nonblocking(false).ok();
                        serve_handshake(&mut sock);
                        held.push(sock); // keep the session(s) open — A stays healthy
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        // Server B (backup): accept + handshake, signal the reader rotated here.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let done_b = Arc::clone(&done);
        let server_b = std::thread::spawn(move || {
            let (mut sock, _) = listener_b.accept().expect("B accept");
            serve_handshake(&mut sock);
            let _ = tx.send(());
            while !done_b.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let client = StratumClient::connect_failover(
            &[addr_a.clone(), addr_b.clone()],
            "csd1testworker",
        )
        .expect("connect to primary A ok");

        // The healthy session is on A. Fire a FAILOVER via the watchdog handle.
        client.watchdog_handle().request_failover();

        // The reader must rotate A→B and handshake there. On Windows a cross-handle
        // shutdown may not interrupt the in-flight recv, so the reader can take up
        // to READ_TIMEOUT(5s) to notice the shut socket, then backoff(1s) + dial B.
        // Under heavy parallel-test load on the shared box that 5s SO_RCVTIMEO is a
        // floor the OS can deliver late, so use a 30s ceiling (only paid on an
        // actual failure); the channel returns the instant B is hit.
        let reached_b = rx.recv_timeout(Duration::from_secs(30)).is_ok();
        done.store(true, Ordering::Relaxed);
        assert!(
            reached_b,
            "request_failover must rotate from HEALTHY primary A to backup B"
        );

        // The live endpoint must follow the failover to B (same 30s-ish budget so a
        // slow-but-correct rotation doesn't fail this second assertion).
        let mut followed = false;
        for _ in 0..500 {
            if client.health_snapshot().endpoint == addr_b {
                followed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            followed,
            "health_snapshot().endpoint must track the failover to B; got {:?}",
            client.health_snapshot().endpoint
        );

        // Telemetry: the rotation counts as exactly ONE failover, and at least one
        // successful reconnect (the dial to B).
        let hs = client.health_snapshot();
        assert_eq!(hs.failovers, 1, "exactly one endpoint failover");
        assert!(hs.reconnects >= 1, "at least one successful reconnect (to B)");

        drop(client);
        let _ = server_a.join();
        let _ = server_b.join();
    }
}
