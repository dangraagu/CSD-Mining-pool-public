//! Pure reliability-watchdog decision logic (P1 §2).
//!
//! No I/O, no threads, no clock of its own — every decision is a pure function
//! of a [`WatchdogSnapshot`] + [`WatchdogCfg`] + an injected `now_ms`, so the
//! whole policy is unit-tested with a fake clock. The client thread that drives
//! it (and the reconnect it requests) lives in `client`/`loop_stratum`; this
//! module only decides *whether* to act.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Tunables for the reliability watchdogs. Defaults are conservative,
/// battle-tested values; nothing here is required of the operator.
#[derive(Debug, Clone, Copy)]
pub struct WatchdogCfg {
    /// Consecutive un-acked submits before forcing a reconnect. A healthy pool
    /// acks every submit, so a long streak means a half-open socket (gets jobs,
    /// silently drops shares) — the live "connected but shares vanish" failure.
    pub max_unacked: u64,
    /// No *new* job for this long ⇒ the push channel is likely dead ⇒ reconnect.
    /// Conservative (5 min) so a merely-quiet chain doesn't trigger needless
    /// reconnects (respecting the af8c236 "quiet link is not a dead link" rule).
    pub job_stale: Duration,
    /// How often the watchdog thread evaluates the snapshot.
    pub poll: Duration,
}

impl Default for WatchdogCfg {
    fn default() -> Self {
        WatchdogCfg {
            max_unacked: 10,
            job_stale: Duration::from_secs(300),
            poll: Duration::from_secs(15),
        }
    }
}

/// A point-in-time view of the session counters the watchdog reasons over.
/// (Grows with §3/§4: last-accept timestamp for the accepted-share dead-man.)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WatchdogSnapshot {
    /// Submits with no ack of any kind since the last ack.
    pub consecutive_unacked: u64,
    /// Unix-ms a *new* job last arrived (0 = none yet).
    pub last_new_job_ms: u64,
}

/// What the watchdog wants done. (Grows: `Failover` with §3, a guarded `Exit`
/// process-dead-man later.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogAction {
    /// Drop and re-establish the connection (re-handshake), reusing the
    /// af8c236-blessed reconnect path.
    ForceReconnect,
}

/// True if no *new* job has arrived within `threshold`, given at least one job
/// has been seen (`last_new_job_ms > 0`). A same-id resend never advances
/// `last_new_job_ms`, so a stalled-but-chatty pool is still caught.
pub fn job_is_stale(last_new_job_ms: u64, now_ms: u64, threshold: Duration) -> bool {
    last_new_job_ms > 0 && now_ms.saturating_sub(last_new_job_ms) > threshold.as_millis() as u64
}

/// Decide whether the watchdog should act, given a snapshot + config + clock.
/// Pure → fake-clock unit-tested. v0.1.8 wires two triggers, both mapping to
/// `ForceReconnect`; `Failover`/`Exit` arrive with §3 and a guarded dead-man.
pub fn watchdog_decision(
    snap: WatchdogSnapshot,
    cfg: WatchdogCfg,
    now_ms: u64,
) -> Option<WatchdogAction> {
    // Submit-ack: N submits, zero acks ⇒ half-open socket ⇒ reconnect.
    if snap.consecutive_unacked >= cfg.max_unacked {
        return Some(WatchdogAction::ForceReconnect);
    }
    // Job-staleness: pool stopped pushing work ⇒ push channel likely dead.
    if job_is_stale(snap.last_new_job_ms, now_ms, cfg.job_stale) {
        return Some(WatchdogAction::ForceReconnect);
    }
    None
}

/// Accepted-share dead-man (v0.1.9 #2): true when the pool is STILL sending fresh
/// work but has not ACCEPTED a share within `accept_threshold` — it takes our
/// submits but never credits us (a forked / misconfigured / hostile pool). This
/// is distinct from the submit-ack streak (a pool that stops ACKing at all) and
/// job-staleness (a dead push channel); it catches a pool that acks + pushes but
/// never accepts, and maps to a FAILOVER (rotate endpoints), not a same-endpoint
/// reconnect.
///
/// Guards against false-tripping a healthy-but-quiet miner:
///   - `last_accept_ms == 0` (never accepted yet) ⇒ never trips — a brand-new or
///     very-low-hashrate miner that simply hasn't found a share is not a victim.
///   - jobs must be FRESH (`last_new_job_ms` within `job_fresh_within`); if the
///     push channel is also quiet/dead, that's job-staleness's job, not this.
///
/// Use a LONG `accept_threshold` (e.g. 30 min) so a miner that accepts even
/// occasionally never trips. (For a single-endpoint miner a failover just
/// re-dials the same pool, so even a rare false-trip is merely a brief reconnect.)
pub fn accept_deadman(
    last_accept_ms: u64,
    last_new_job_ms: u64,
    now_ms: u64,
    accept_threshold: Duration,
    job_fresh_within: Duration,
) -> bool {
    if last_accept_ms == 0 {
        return false; // never been credited a share ⇒ not a dropped-shares victim
    }
    // The pool must still be pushing FRESH work, else this is a dead push channel
    // (job-staleness handles that) — and a genuinely quiet pool isn't dropping us.
    if last_new_job_ms == 0
        || now_ms.saturating_sub(last_new_job_ms) > job_fresh_within.as_millis() as u64
    {
        return false;
    }
    now_ms.saturating_sub(last_accept_ms) >= accept_threshold.as_millis() as u64
}

/// A `'static` view the watchdog thread uses to observe the session and request
/// a reconnect — without holding the mining loop's borrows. Implemented by the
/// live client (real socket) and a mock (tests).
pub trait WatchdogView: Send + Sync {
    /// Current session counters.
    fn snapshot(&self) -> WatchdogSnapshot;
    /// Force the connection to drop + re-handshake (reusing the af8c236 path).
    fn request_reconnect(&self);
}

/// One watchdog evaluation: read the view's snapshot, decide, and act. Returns
/// `true` if it triggered a reconnect. `now_ms` is injected so this is unit-
/// tested with a fake clock + a mock view — no thread, no sleep, no socket.
pub fn watchdog_tick(view: &dyn WatchdogView, cfg: WatchdogCfg, now_ms: u64) -> bool {
    match watchdog_decision(view.snapshot(), cfg, now_ms) {
        Some(WatchdogAction::ForceReconnect) => {
            view.request_reconnect();
            true
        }
        None => false,
    }
}

/// Spawn the reliability-watchdog thread: every `cfg.poll`, evaluate `view` and
/// act, until `stop` is set. Sleeps in small slices so it honors `stop`
/// promptly (within ~200 ms). The caller may detach the handle — the thread
/// owns its `Arc`s and exits as soon as `stop` flips.
pub fn spawn_watchdog(
    view: Arc<dyn WatchdogView>,
    cfg: WatchdogCfg,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("stratum-watchdog".to_string())
        .spawn(move || {
            let slice = Duration::from_millis(200).min(cfg.poll);
            let mut waited = Duration::ZERO;
            while !stop.load(Ordering::Relaxed) {
                if waited >= cfg.poll {
                    waited = Duration::ZERO;
                    watchdog_tick(view.as_ref(), cfg, now_unix_ms());
                }
                std::thread::sleep(slice);
                waited += slice;
            }
        })
        .expect("spawning stratum watchdog thread")
}

/// Wall-clock ms since the Unix epoch (0 if the clock predates it — never panics).
fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> WatchdogCfg {
        WatchdogCfg::default()
    }

    #[test]
    fn no_action_when_healthy() {
        let snap = WatchdogSnapshot {
            consecutive_unacked: 3,
            last_new_job_ms: 1_000_000,
        };
        // 100 s since the last job (< 300 s) and only 3 unacked ⇒ all fine.
        assert_eq!(watchdog_decision(snap, cfg(), 1_000_000 + 100_000), None);
    }

    #[test]
    fn submit_ack_streak_forces_reconnect() {
        let at = 1_000_100;
        let snap = WatchdogSnapshot {
            consecutive_unacked: 10,
            last_new_job_ms: 1_000_000,
        };
        assert_eq!(
            watchdog_decision(snap, cfg(), at),
            Some(WatchdogAction::ForceReconnect)
        );
        // 9 is still under the threshold.
        let snap9 = WatchdogSnapshot {
            consecutive_unacked: 9,
            last_new_job_ms: 1_000_000,
        };
        assert_eq!(watchdog_decision(snap9, cfg(), at), None);
    }

    #[test]
    fn stale_job_forces_reconnect() {
        let last = 1_000_000u64;
        let snap = WatchdogSnapshot {
            consecutive_unacked: 0,
            last_new_job_ms: last,
        };
        // 301 s later ⇒ stale.
        assert_eq!(
            watchdog_decision(snap, cfg(), last + 301_000),
            Some(WatchdogAction::ForceReconnect)
        );
        // 299 s ⇒ fresh.
        assert_eq!(watchdog_decision(snap, cfg(), last + 299_000), None);
    }

    #[test]
    fn never_stale_before_first_job() {
        // last_new_job_ms == 0 (no job yet) is never "stale" — we just connected.
        assert!(!job_is_stale(0, 9_999_999, Duration::from_secs(300)));
        let snap = WatchdogSnapshot {
            consecutive_unacked: 0,
            last_new_job_ms: 0,
        };
        assert_eq!(watchdog_decision(snap, cfg(), 9_999_999), None);
    }

    #[test]
    fn accept_deadman_trips_only_when_jobs_fresh_and_accepts_stale() {
        let accept_thr = Duration::from_secs(1800); // 30 min
        let job_fresh = Duration::from_secs(300); // 5 min
        let now = 10_000_000u64;
        let m = 60_000u64; // ms per minute

        // Jobs fresh (1 min ago), last accept 31 min ago ⇒ TRIP (pool not crediting).
        assert!(accept_deadman(now - 31 * m, now - m, now, accept_thr, job_fresh));
        // Last accept only 29 min ago ⇒ under threshold, no trip.
        assert!(!accept_deadman(now - 29 * m, now - m, now, accept_thr, job_fresh));
        // Never accepted (0) ⇒ never trips (new / very-low-hashrate miner).
        assert!(!accept_deadman(0, now - m, now, accept_thr, job_fresh));
        // Accepts stale BUT jobs also stale (10 min ago > 5 min window) ⇒ no trip
        // (that's the job-staleness watchdog's case, not dropped shares).
        assert!(!accept_deadman(now - 31 * m, now - 10 * m, now, accept_thr, job_fresh));
        // Jobs fresh but no job has ever arrived (0) ⇒ no trip.
        assert!(!accept_deadman(now - 31 * m, 0, now, accept_thr, job_fresh));
    }

    struct MockView {
        snap: WatchdogSnapshot,
        reconnects: std::sync::atomic::AtomicU64,
    }
    impl MockView {
        fn new(snap: WatchdogSnapshot) -> Self {
            MockView {
                snap,
                reconnects: std::sync::atomic::AtomicU64::new(0),
            }
        }
        fn reconnect_calls(&self) -> u64 {
            self.reconnects.load(std::sync::atomic::Ordering::Relaxed)
        }
    }
    impl WatchdogView for MockView {
        fn snapshot(&self) -> WatchdogSnapshot {
            self.snap
        }
        fn request_reconnect(&self) {
            self.reconnects
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[test]
    fn tick_reconnects_on_trigger_and_idles_when_healthy() {
        // A submit-ack streak → the tick acts exactly once.
        let trig = MockView::new(WatchdogSnapshot {
            consecutive_unacked: 10,
            last_new_job_ms: 1,
        });
        assert!(watchdog_tick(&trig, WatchdogCfg::default(), 1_000));
        assert_eq!(trig.reconnect_calls(), 1);
        // Healthy snapshot → no action, no reconnect.
        let ok = MockView::new(WatchdogSnapshot {
            consecutive_unacked: 1,
            last_new_job_ms: 1_000,
        });
        assert!(!watchdog_tick(&ok, WatchdogCfg::default(), 1_100));
        assert_eq!(ok.reconnect_calls(), 0);
    }

    #[test]
    fn spawned_watchdog_ticks_and_stops() {
        let view = Arc::new(MockView::new(WatchdogSnapshot {
            consecutive_unacked: 10, // always triggers ForceReconnect
            last_new_job_ms: 1,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let cfg = WatchdogCfg {
            poll: Duration::from_millis(5),
            ..WatchdogCfg::default()
        };
        let h = spawn_watchdog(view.clone(), cfg, Arc::clone(&stop));
        std::thread::sleep(Duration::from_millis(60));
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        assert!(
            view.reconnect_calls() >= 1,
            "the watchdog thread must tick + act before stop"
        );
    }
}
