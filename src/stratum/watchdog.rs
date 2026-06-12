//! Pure reliability-watchdog decision logic (P1 §2).
//!
//! No I/O, no threads, no clock of its own — every decision is a pure function
//! of a [`WatchdogSnapshot`] + [`WatchdogCfg`] + an injected `now_ms`, so the
//! whole policy is unit-tested with a fake clock. The client thread that drives
//! it (and the reconnect it requests) lives in `client`/`loop_stratum`; this
//! module only decides *whether* to act.

use std::time::Duration;

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
}
