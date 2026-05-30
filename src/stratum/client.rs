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

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use super::protocol::{
    authorize_request, serialize_line, subscribe_request, submit_request, NotifyParams,
    Notification, Response, SubscribeResult,
};

/// How long to wait on `connect()` for the TCP three-way handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Read timeout on the socket so the reader thread can't block forever waiting
/// for a frame from a wedged bridge. ~120s comfortably exceeds the bridge's
/// notify/difficulty cadence, so a timeout means the link is actually dead.
const READ_TIMEOUT: Duration = Duration::from_secs(120);
/// Reconnect backoff bounds.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

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
}

impl Shared {
    fn set_difficulty(&self, d: f64) {
        self.difficulty_bits.store(d.to_bits(), Ordering::Relaxed);
    }
    fn difficulty(&self) -> f64 {
        f64::from_bits(self.difficulty_bits.load(Ordering::Relaxed))
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
    /// Connect to `endpoint` (`host:port`), run the subscribe + authorize
    /// handshake, capture `extranonce1`/`extranonce2_size`, confirm authorize
    /// returned `true` (bail otherwise), then spawn the reader thread.
    pub fn connect(endpoint: &str, worker_addr: &str) -> Result<Self> {
        let hs = Self::handshake(endpoint, worker_addr)
            .with_context(|| format!("stratum handshake to {endpoint}"))?;

        let shared = Arc::new(Shared {
            latest_job: Mutex::new(None),
            difficulty_bits: AtomicU64::new(1.0f64.to_bits()),
            extranonce1_hex: Mutex::new(hs.subscribe.extranonce1_hex.clone()),
            extranonce2_size: AtomicU64::new(hs.subscribe.extranonce2_size as u64),
            shutdown: AtomicBool::new(false),
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

        let reader = Self::spawn_reader(
            endpoint.to_string(),
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
        endpoint: String,
        worker_addr: String,
        initial_reader: BufReader<TcpStream>,
        shared: Arc<Shared>,
        writer: Arc<Mutex<TcpStream>>,
    ) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("stratum-reader".to_string())
            .spawn(move || {
                let mut reader = initial_reader;
                let mut backoff = BACKOFF_MIN;
                let mut line = String::new();

                loop {
                    if shared.shutdown.load(Ordering::Relaxed) {
                        return;
                    }

                    line.clear();
                    let read = reader.read_line(&mut line);
                    let needs_reconnect = match read {
                        Ok(0) => {
                            // Clean EOF: peer closed the connection.
                            if shared.shutdown.load(Ordering::Relaxed) {
                                return;
                            }
                            tracing::warn!(
                                "stratum: connection closed by {endpoint}, reconnecting"
                            );
                            true
                        }
                        Ok(_) => {
                            dispatch_frame(line.trim(), &shared);
                            backoff = BACKOFF_MIN; // healthy read resets backoff
                            false
                        }
                        Err(e) => {
                            if shared.shutdown.load(Ordering::Relaxed) {
                                return;
                            }
                            tracing::warn!(
                                "stratum: read error from {endpoint}: {e}; reconnecting"
                            );
                            true
                        }
                    };

                    if needs_reconnect
                        && !reconnect(
                            &endpoint,
                            &worker_addr,
                            &shared,
                            &writer,
                            &mut reader,
                            &mut backoff,
                        )
                    {
                        return; // shutdown requested mid-reconnect
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
                    *slot = Some(StratumJob {
                        notify,
                        extranonce1_hex,
                    });
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
/// The backoff doubles each failed attempt up to [`BACKOFF_MAX`]; a successful
/// reconnect resets it (the caller also resets on the next healthy read).
fn reconnect(
    endpoint: &str,
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

        match StratumClient::handshake(endpoint, worker_addr) {
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
                }
                *backoff = BACKOFF_MIN;
                tracing::info!("stratum: reconnected to {endpoint}");
                return true;
            }
            Err(e) => {
                tracing::warn!("stratum: reconnect to {endpoint} failed: {e}");
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

    /// Build a fresh `Shared` with defaults, as `connect()` would.
    fn fresh_shared(xn1: &str, xn2_size: u64) -> Arc<Shared> {
        Arc::new(Shared {
            latest_job: Mutex::new(None),
            difficulty_bits: AtomicU64::new(1.0f64.to_bits()),
            extranonce1_hex: Mutex::new(xn1.to_string()),
            extranonce2_size: AtomicU64::new(xn2_size),
            shutdown: AtomicBool::new(false),
        })
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
}
