//! Hung-GPU / zero-hashrate watchdog — pure decision logic + a thin driver.
//!
//! One GPU per process (`--device N`, one process per card). A GPU can wedge
//! mid-run — a driver hiccup, an unstable overclock, a TDR reset, a VRAM ECC
//! fault — and keep the process *alive* while it produces **zero** hashes. The
//! Stratum connection stays healthy and fresh jobs keep arriving, so the
//! reliability watchdog (`stratum::watchdog`) sees nothing wrong: it only knows
//! about the socket, not the silicon. This module is the missing half — it
//! watches the GPU's own hashrate and, when it flatlines while there IS work to
//! do, first tries an **in-process CUDA recovery** and, failing that, **exits
//! with a distinct non-zero code** so a supervisor (systemd / HiveOS / the
//! launcher `.bat`) restarts the process clean.
//!
//! Like `stratum::watchdog`, the policy is a **pure function** of an injected
//! [`GpuWatchdogSnapshot`] + [`GpuWatchdogCfg`] — no I/O, no clock of its own,
//! no GPU handle — so the whole "stalled vs idle vs recovering" decision is
//! unit-tested with plain values. The thread that drives it (sampling the live
//! hashrate, calling `backend.recover()`, and exiting the process) is a thin
//! shell at the bottom; this module only decides *whether* to act.
//!
//! Crucially it must NOT fire on a *legitimately idle* GPU: a miner waiting for
//! its first job, a pool that has gone quiet, or a half-dead socket has zero
//! hashrate too — but that is not a hung GPU, and killing the process there
//! would be a reconnect-storm-by-restart. So the decision only ever escalates
//! when hashrate is floored **AND** fresh jobs are flowing **AND** the
//! connection is healthy: a GPU that has work and a link but produces nothing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Process exit code used when the GPU is conclusively hung and in-process
/// recovery did not bring hashrate back. DISTINCT and DOCUMENTED so a
/// supervisor can tell "stalled GPU, please restart me" apart from a normal
/// exit (0), a generic error (1), or the selftest/verify codes (1/2). A
/// systemd unit (`Restart=on-failure` / `RestartForceExitStatus=17`), a HiveOS
/// wrapper, or the launcher `.bat` restart loop should treat this as
/// "restart the miner". Chosen as 17 (no collision with the existing
/// `check-update`/`verify-file` 0/1/2 codes).
pub const EXIT_GPU_STALLED: i32 = 17;

/// Tunables for the GPU stall watchdog. Defaults are conservative so a healthy
/// rig NEVER trips: a real stall persists for many samples, whereas a momentary
/// dip between launches (template refresh, a brief CPU-only window) recovers
/// within one or two samples.
#[derive(Debug, Clone, Copy)]
pub struct GpuWatchdogCfg {
    /// Master on/off. `false` ⇒ the watchdog never samples or acts (the
    /// `--no-gpu-watchdog` escape hatch); behaviour is identical to a build
    /// without this feature.
    pub enabled: bool,
    /// Hashrate at/below this (GH/s) counts as "floored" (effectively zero). A
    /// small positive floor (not exactly 0.0) so a near-dead GPU dribbling a
    /// handful of H/s still trips. Operator-tunable via `--gpu-floor`.
    pub floor_ghs: f64,
    /// Consecutive floored samples, while jobs flow + the link is healthy,
    /// before acting (the "dwell"). At the 15s sample cadence the default 4 ⇒
    /// ~60s of *continuous* zero-with-work before the first recovery attempt.
    /// Operator-tunable via `--gpu-watchdog-dwell`.
    pub dwell_samples: u32,
    /// After a recovery attempt, how long (ms) to give the GPU to come back
    /// before escalating to a process exit. If hashrate returns to above the
    /// floor within this window the watchdog disarms; if it is still floored
    /// when the window elapses (and jobs/link are still healthy), it exits with
    /// [`EXIT_GPU_STALLED`]. Operator-tunable via `--gpu-watchdog-recover-secs`.
    pub recover_window: Duration,
    /// Maximum in-process recovery attempts before giving up and exiting. A
    /// driver that needs a full process restart will fail recovery repeatedly;
    /// this bounds the thrash so we hand off to the supervisor promptly instead
    /// of looping recover() forever. 0 ⇒ never attempt recovery, go straight to
    /// exit on a confirmed stall (still gated by dwell + jobs + link).
    pub max_recoveries: u32,
    /// How often the watchdog thread samples the live hashrate.
    pub poll: Duration,
}

impl Default for GpuWatchdogCfg {
    fn default() -> Self {
        GpuWatchdogCfg {
            enabled: true,
            // 0.001 GH/s = 1 MH/s. A working GPU is hundreds of MH/s to many
            // GH/s; even the weakest supported card clears 1 MH/s by orders of
            // magnitude, so anything at/under this is a dead kernel, never a
            // slow-but-alive one.
            floor_ghs: 0.001,
            dwell_samples: 4,
            recover_window: Duration::from_secs(60),
            max_recoveries: 3,
            poll: Duration::from_secs(15),
        }
    }
}

/// A point-in-time view the GPU watchdog reasons over. All fields are sampled by
/// the driver thread and passed by value into the pure decision; nothing here is
/// a live handle, so the policy is testable with literals.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GpuWatchdogSnapshot {
    /// Latest GPU-only hashrate sample (GH/s). The CPU pool's contribution is
    /// excluded on purpose: a hung GPU with a busy CPU pool would otherwise look
    /// "alive". 0.0 (or below the floor) = the GPU produced nothing this sample.
    pub gpu_ghs: f64,
    /// Consecutive prior samples (BEFORE this one) that were already floored
    /// while work was flowing. The driver maintains this streak; the decision
    /// adds the current sample to it. Reset to 0 by the driver whenever a sample
    /// is above the floor OR work stopped flowing (so an idle gap never
    /// accumulates toward a stall).
    pub floored_streak: u32,
    /// True iff fresh jobs are arriving from the pool (the loop is NOT merely
    /// waiting for work). Derived from the job age vs the staleness horizon. A
    /// stalled GPU is only actionable when there IS work to hash.
    pub jobs_flowing: bool,
    /// True iff the Stratum connection is healthy (not mid-reconnect / not a
    /// dead/half-open socket). A zero hashrate during a reconnect is expected,
    /// not a GPU fault.
    pub conn_healthy: bool,
    /// How many in-process recovery attempts have already been made this stall.
    /// Lets the decision stop retrying recover() once `max_recoveries` is hit
    /// and escalate to exit instead.
    pub recoveries_done: u32,
    /// Set once a recovery attempt has been made and we are inside the
    /// post-recovery grace window: `Some(ms_since_recovery)`. `None` before any
    /// recovery this stall. Drives the "did hashrate come back in time?" check.
    pub ms_since_recovery: Option<u64>,
}

/// What the GPU watchdog wants done, decided purely from a snapshot + cfg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuWatchdogAction {
    /// GPU looks fine (or is legitimately idle / mid-reconnect / waiting for
    /// work) — do nothing.
    Ok,
    /// Confirmed stall and we still have recovery budget: attempt an in-process
    /// CUDA recovery (`backend.recover()`) and start the grace window.
    Recover,
    /// Confirmed stall with no recovery left (recovery exhausted, or recovery
    /// already tried and the grace window elapsed with hashrate still floored,
    /// or `max_recoveries == 0`): exit the process with [`EXIT_GPU_STALLED`] so
    /// a supervisor restarts it.
    Exit,
}

/// True iff `ghs` is at/below the configured floor (i.e. the GPU produced
/// effectively nothing). Uses `<=` so an exact-floor sample counts as floored.
/// A NaN sample (should never happen, but a divide could produce one) is
/// treated as floored — a GPU emitting NaN H/s is not healthy.
#[inline]
pub fn is_floored(ghs: f64, floor_ghs: f64) -> bool {
    ghs.is_nan() || ghs <= floor_ghs
}

/// The pure GPU-stall decision. Given the live snapshot + the tunables, decide
/// whether to do nothing, attempt an in-process recovery, or exit for a
/// supervisor restart.
///
/// The escalation ladder, in order:
///   1. **Disabled / not-floored / no-work / unhealthy-link ⇒ `Ok`.** The
///      watchdog only ever acts on a GPU that is floored *with work to do over a
///      healthy link*. A floored sample while idle (no jobs) or mid-reconnect is
///      expected and is NOT a stall. This is the false-positive guard.
///   2. **Floored-with-work but dwell not yet reached ⇒ `Ok`.** A real stall
///      persists; a momentary dip must clear within `dwell_samples`. The streak
///      counts the current floored sample too (`floored_streak + 1`).
///   3. **Dwell reached, a recovery already attempted, still inside the grace
///      window ⇒ `Ok`.** We asked the GPU to recover; give it `recover_window`
///      to spin back up before doing anything else (it is still floored, but we
///      are waiting, not idle).
///   4. **Dwell reached, recovery attempted, grace window elapsed, still floored
///      ⇒ escalate.** Recovery did not bring it back: `Recover` again if budget
///      remains, else `Exit`.
///   5. **Dwell reached, no recovery attempted yet ⇒ `Recover`** (or `Exit`
///      immediately if `max_recoveries == 0`).
pub fn gpu_watchdog_decision(
    snap: GpuWatchdogSnapshot,
    cfg: GpuWatchdogCfg,
) -> GpuWatchdogAction {
    // (1) Off, or no actionable stall condition. A GPU is only "hung" if it is
    // floored AND there is work AND the link is up. Anything else is benign idle.
    if !cfg.enabled
        || !is_floored(snap.gpu_ghs, cfg.floor_ghs)
        || !snap.jobs_flowing
        || !snap.conn_healthy
    {
        return GpuWatchdogAction::Ok;
    }

    // From here on: floored, with fresh work, over a healthy link.

    // (2) Dwell gate: require `dwell_samples` CONSECUTIVE floored-with-work
    // samples (this one included) before acting, so a single dip never trips.
    let floored_count = snap.floored_streak.saturating_add(1);
    if floored_count < cfg.dwell_samples {
        return GpuWatchdogAction::Ok;
    }

    // Dwell satisfied — this is a confirmed stall. Decide recover vs wait vs exit.
    match snap.ms_since_recovery {
        // (3)/(4) A recovery was already attempted this stall.
        Some(ms) => {
            if ms < cfg.recover_window.as_millis() as u64 {
                // (3) Still inside the post-recovery grace window: keep waiting
                // for hashrate to return. (Still floored, but we already acted.)
                GpuWatchdogAction::Ok
            } else if snap.recoveries_done < cfg.max_recoveries {
                // (4a) Grace elapsed, still floored, budget remains: try again.
                GpuWatchdogAction::Recover
            } else {
                // (4b) Grace elapsed, still floored, no budget left: hand off.
                GpuWatchdogAction::Exit
            }
        }
        // (5) No recovery attempted yet this stall.
        None => {
            if cfg.max_recoveries == 0 {
                // Recovery disabled outright: a confirmed stall exits immediately.
                GpuWatchdogAction::Exit
            } else {
                GpuWatchdogAction::Recover
            }
        }
    }
}

/// A backend that can attempt to recover from a wedged GPU state in-process by
/// rebuilding its driver-side resources (module / streams / device buffers).
///
/// Default is a no-op returning `false` ("not recovered"), so backends that
/// can't meaningfully self-heal (CPU, or a backend that doesn't implement it)
/// degrade safely — the watchdog then escalates straight to an exit on a
/// confirmed stall, which is exactly right for a CPU "backend" (it can't hang
/// the way a GPU does, but if it somehow produced zero-with-work, a restart is
/// the only lever). A GPU backend overrides this to tear down and rebuild its
/// CUDA resources.
pub trait Recoverable {
    /// Attempt in-process recovery. Returns `true` if the rebuild succeeded
    /// (the caller then waits `recover_window` to see hashrate return), `false`
    /// if recovery itself failed (the caller escalates sooner). MUST NOT panic;
    /// a backend that can't recover returns `false`. MUST be safe to call from
    /// the watchdog thread while the mining thread may be mid-`hash_range`
    /// (implementations serialize via their own interior lock).
    fn recover(&self) -> bool {
        false
    }
}

/// A `'static` view the GPU-watchdog thread uses to observe the miner and act,
/// without holding the mining loop's borrows. Implemented by a live adapter over
/// the loop's hashrate sampler + the client's health, and by a mock in tests.
pub trait GpuWatchdogView: Send + Sync {
    /// Latest GPU-only hashrate (GH/s) since the previous sample.
    fn gpu_ghs(&self) -> f64;
    /// Are fresh jobs flowing (the loop has work, not just waiting)?
    fn jobs_flowing(&self) -> bool;
    /// Is the Stratum link healthy (not mid-reconnect / half-open)?
    fn conn_healthy(&self) -> bool;
    /// Attempt in-process GPU recovery; `true` if the rebuild succeeded.
    fn recover(&self) -> bool;
    /// Escalate: the GPU is conclusively hung and recovery failed — terminate
    /// the process with [`EXIT_GPU_STALLED`] so a supervisor restarts it. The
    /// live impl calls `std::process::exit`; the mock records it instead (so the
    /// driver is testable without killing the test runner).
    fn escalate_exit(&self);
}

/// Mutable bookkeeping the driver thread carries across samples (the part the
/// pure decision is a function of, but which must persist between polls).
#[derive(Debug, Clone, Copy, Default)]
pub struct GpuWatchdogState {
    /// Consecutive floored-with-work samples observed so far.
    pub floored_streak: u32,
    /// Recovery attempts made in the CURRENT stall episode.
    pub recoveries_done: u32,
    /// `Some(ms)` since the last recovery in this episode, else `None`. Stored as
    /// the wall-clock ms AT recovery; the driver computes the delta at sample
    /// time. We keep the raw stamp so the pure decision gets a fresh delta.
    pub recovery_at_ms: Option<u64>,
}

impl GpuWatchdogState {
    /// Reset to the no-stall baseline (called whenever a sample is healthy or
    /// work stopped flowing — the stall episode is over).
    pub fn reset(&mut self) {
        *self = GpuWatchdogState::default();
    }
}

/// Build the pure-decision snapshot from a live view sample + the carried state
/// + the current clock. Factored out so the driver's per-tick assembly is itself
/// covered without a thread. `now_ms` is injected (fake clock in tests).
pub fn snapshot_from(
    gpu_ghs: f64,
    jobs_flowing: bool,
    conn_healthy: bool,
    state: &GpuWatchdogState,
    now_ms: u64,
) -> GpuWatchdogSnapshot {
    GpuWatchdogSnapshot {
        gpu_ghs,
        floored_streak: state.floored_streak,
        jobs_flowing,
        conn_healthy,
        recoveries_done: state.recoveries_done,
        ms_since_recovery: state
            .recovery_at_ms
            .map(|at| now_ms.saturating_sub(at)),
    }
}

/// One watchdog evaluation against a live view: sample, build the snapshot from
/// the carried `state`, decide, act, and update `state`. Returns the action
/// taken (for logging/tests). `now_ms` is injected so this is unit-tested with a
/// fake clock + a mock view — no thread, no sleep, no GPU.
///
/// State transitions (the mutable half the pure decision can't own):
///   - A healthy sample, or work not flowing / link down ⇒ `state.reset()` (the
///     stall episode, if any, is over) and return `Ok`.
///   - A floored-with-work sample ⇒ bump `floored_streak`, then on the decision:
///       * `Recover` ⇒ call `view.recover()`; on success stamp `recovery_at_ms =
///         now` and bump `recoveries_done`; on failure leave the stamp so the
///         next tick re-evaluates (and, with budget gone, exits).
///       * `Exit`    ⇒ `view.escalate_exit()` (process dies in the live impl).
///       * `Ok`      ⇒ keep waiting.
pub fn gpu_watchdog_tick(
    view: &dyn GpuWatchdogView,
    cfg: GpuWatchdogCfg,
    state: &mut GpuWatchdogState,
    now_ms: u64,
) -> GpuWatchdogAction {
    if !cfg.enabled {
        return GpuWatchdogAction::Ok;
    }

    let gpu_ghs = view.gpu_ghs();
    let jobs_flowing = view.jobs_flowing();
    let conn_healthy = view.conn_healthy();

    // If this sample is NOT an actionable-stall sample (healthy hashrate, or no
    // work, or unhealthy link), the stall episode is over: clear all carried
    // state so a fresh stall starts counting from zero, and do nothing.
    if !is_floored(gpu_ghs, cfg.floor_ghs) || !jobs_flowing || !conn_healthy {
        state.reset();
        return GpuWatchdogAction::Ok;
    }

    // Actionable-stall sample: build the snapshot from the PRIOR streak, decide,
    // then fold this sample into the streak for next time.
    let snap = snapshot_from(gpu_ghs, jobs_flowing, conn_healthy, state, now_ms);
    let action = gpu_watchdog_decision(snap, cfg);
    state.floored_streak = state.floored_streak.saturating_add(1);

    match action {
        GpuWatchdogAction::Recover => {
            tracing::warn!(
                "gpu-watchdog: STALL confirmed (gpu={:.4} GH/s floored for {} samples, jobs flowing, link healthy) — attempting in-process GPU recovery (attempt {}/{})",
                gpu_ghs,
                state.floored_streak,
                state.recoveries_done + 1,
                cfg.max_recoveries,
            );
            let ok = view.recover();
            if ok {
                state.recoveries_done = state.recoveries_done.saturating_add(1);
                state.recovery_at_ms = Some(now_ms);
                tracing::warn!("gpu-watchdog: recovery rebuild succeeded; waiting up to {:?} for hashrate to return", cfg.recover_window);
            } else {
                // Recovery itself failed. Stamp the attempt so the grace window
                // starts (a failed rebuild still "used" an attempt); the next
                // tick, once the window elapses with budget gone, will Exit.
                state.recoveries_done = state.recoveries_done.saturating_add(1);
                state.recovery_at_ms = Some(now_ms);
                tracing::error!("gpu-watchdog: in-process GPU recovery FAILED; will exit for supervisor restart if hashrate does not return");
            }
        }
        GpuWatchdogAction::Exit => {
            tracing::error!(
                "gpu-watchdog: GPU conclusively hung (gpu={:.4} GH/s, {} floored samples, {} recoveries exhausted) — exiting with code {} for supervisor restart",
                gpu_ghs,
                state.floored_streak,
                state.recoveries_done,
                EXIT_GPU_STALLED,
            );
            view.escalate_exit();
        }
        GpuWatchdogAction::Ok => {}
    }
    action
}

/// Spawn the GPU-watchdog thread: every `cfg.poll`, evaluate `view` and act,
/// until `stop` is set. Sleeps in small slices so it honors `stop` promptly.
/// No-op (returns immediately-joinable thread) when `cfg.enabled` is false. The
/// caller may detach the handle — the thread owns its `Arc`s.
pub fn spawn_gpu_watchdog(
    view: Arc<dyn GpuWatchdogView>,
    cfg: GpuWatchdogCfg,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("gpu-watchdog".to_string())
        .spawn(move || {
            if !cfg.enabled {
                return;
            }
            let mut state = GpuWatchdogState::default();
            let slice = Duration::from_millis(200).min(cfg.poll);
            // Wait one full `poll` before the first evaluation so a just-started
            // GPU (no hashrate sample yet) isn't mis-read as floored.
            let mut waited = Duration::ZERO;
            while !stop.load(Ordering::Relaxed) {
                if waited >= cfg.poll {
                    waited = Duration::ZERO;
                    gpu_watchdog_tick(view.as_ref(), cfg, &mut state, now_unix_ms());
                }
                std::thread::sleep(slice);
                waited += slice;
            }
        })
        .expect("spawning gpu-watchdog thread")
}

/// Wall-clock ms since the Unix epoch (0 if the clock predates it — never panics).
fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GpuWatchdogCfg {
        GpuWatchdogCfg::default()
    }

    /// A healthy snapshot baseline: GPU hashing fine, work flowing, link up.
    fn healthy_snap() -> GpuWatchdogSnapshot {
        GpuWatchdogSnapshot {
            gpu_ghs: 2.5,
            floored_streak: 0,
            jobs_flowing: true,
            conn_healthy: true,
            recoveries_done: 0,
            ms_since_recovery: None,
        }
    }

    // --- is_floored ---

    #[test]
    fn is_floored_uses_inclusive_floor_and_catches_nan() {
        assert!(is_floored(0.0, 0.001));
        assert!(is_floored(0.001, 0.001), "exact floor counts as floored");
        assert!(!is_floored(0.0011, 0.001));
        assert!(!is_floored(3.0, 0.001));
        assert!(is_floored(f64::NAN, 0.001), "NaN hashrate is not healthy");
        // A zero floor: only literal zero (or below) is floored.
        assert!(is_floored(0.0, 0.0));
        assert!(!is_floored(0.0001, 0.0));
    }

    // --- the pure decision: false-positive guards (the safety-critical part) ---

    #[test]
    fn healthy_gpu_is_ok() {
        assert_eq!(gpu_watchdog_decision(healthy_snap(), cfg()), GpuWatchdogAction::Ok);
    }

    #[test]
    fn disabled_never_acts_even_when_hung() {
        // A fully-confirmed stall, but the watchdog is off ⇒ Ok.
        let snap = GpuWatchdogSnapshot {
            gpu_ghs: 0.0,
            floored_streak: 100,
            jobs_flowing: true,
            conn_healthy: true,
            recoveries_done: 0,
            ms_since_recovery: None,
        };
        let off = GpuWatchdogCfg { enabled: false, ..cfg() };
        assert_eq!(gpu_watchdog_decision(snap, off), GpuWatchdogAction::Ok);
    }

    #[test]
    fn floored_but_no_jobs_is_ok_not_a_stall() {
        // Zero hashrate because there is NO WORK (idle / waiting for first job)
        // must never be treated as a hung GPU — this is the idle-vs-stall guard.
        let snap = GpuWatchdogSnapshot {
            gpu_ghs: 0.0,
            floored_streak: 100, // long-floored, but...
            jobs_flowing: false, // ...no work to hash ⇒ benign idle
            conn_healthy: true,
            recoveries_done: 0,
            ms_since_recovery: None,
        };
        assert_eq!(gpu_watchdog_decision(snap, cfg()), GpuWatchdogAction::Ok);
    }

    #[test]
    fn floored_but_link_unhealthy_is_ok_not_a_stall() {
        // Zero hashrate during a reconnect / half-open socket is expected, not a
        // GPU fault — never act while the link is down.
        let snap = GpuWatchdogSnapshot {
            gpu_ghs: 0.0,
            floored_streak: 100,
            jobs_flowing: true,
            conn_healthy: false, // mid-reconnect
            recoveries_done: 0,
            ms_since_recovery: None,
        };
        assert_eq!(gpu_watchdog_decision(snap, cfg()), GpuWatchdogAction::Ok);
    }

    // --- dwell gate ---

    #[test]
    fn floored_with_work_below_dwell_is_ok() {
        // floored_streak counts PRIOR samples; the decision adds the current one.
        // default dwell = 4, so prior streak 0,1,2 (=> counts 1,2,3) are still Ok.
        for prior in 0..(cfg().dwell_samples - 1) {
            let snap = GpuWatchdogSnapshot {
                gpu_ghs: 0.0,
                floored_streak: prior,
                jobs_flowing: true,
                conn_healthy: true,
                recoveries_done: 0,
                ms_since_recovery: None,
            };
            assert_eq!(
                gpu_watchdog_decision(snap, cfg()),
                GpuWatchdogAction::Ok,
                "prior streak {prior} (count {}) is under dwell {}",
                prior + 1,
                cfg().dwell_samples
            );
        }
    }

    #[test]
    fn floored_with_work_at_dwell_recovers_first() {
        // prior streak = dwell-1 (=3) ⇒ count 4 == dwell ⇒ first action is Recover
        // (no recovery attempted yet, max_recoveries > 0).
        let snap = GpuWatchdogSnapshot {
            gpu_ghs: 0.0,
            floored_streak: cfg().dwell_samples - 1,
            jobs_flowing: true,
            conn_healthy: true,
            recoveries_done: 0,
            ms_since_recovery: None,
        };
        assert_eq!(gpu_watchdog_decision(snap, cfg()), GpuWatchdogAction::Recover);
    }

    #[test]
    fn dwell_boundary_is_exact() {
        // count == dwell-1 ⇒ Ok ; count == dwell ⇒ act. Pin the exact edge.
        let c = GpuWatchdogCfg { dwell_samples: 5, ..cfg() };
        let mk = |prior| GpuWatchdogSnapshot {
            gpu_ghs: 0.0,
            floored_streak: prior,
            jobs_flowing: true,
            conn_healthy: true,
            recoveries_done: 0,
            ms_since_recovery: None,
        };
        assert_eq!(gpu_watchdog_decision(mk(3), c), GpuWatchdogAction::Ok); // count 4
        assert_eq!(gpu_watchdog_decision(mk(4), c), GpuWatchdogAction::Recover); // count 5
    }

    // --- recovery / grace window / exit ladder ---

    #[test]
    fn max_recoveries_zero_exits_immediately_on_confirmed_stall() {
        // Recovery disabled: a dwell-satisfied stall exits straight away (still
        // gated by jobs+link, which are healthy here).
        let c = GpuWatchdogCfg { max_recoveries: 0, ..cfg() };
        let snap = GpuWatchdogSnapshot {
            gpu_ghs: 0.0,
            floored_streak: c.dwell_samples, // well past dwell
            jobs_flowing: true,
            conn_healthy: true,
            recoveries_done: 0,
            ms_since_recovery: None,
        };
        assert_eq!(gpu_watchdog_decision(snap, c), GpuWatchdogAction::Exit);
    }

    #[test]
    fn within_grace_window_after_recovery_waits() {
        // A recovery was attempted 30s ago, window is 60s, still floored ⇒ keep
        // waiting (Ok), do NOT exit yet.
        let snap = GpuWatchdogSnapshot {
            gpu_ghs: 0.0,
            floored_streak: 10,
            jobs_flowing: true,
            conn_healthy: true,
            recoveries_done: 1,
            ms_since_recovery: Some(30_000),
        };
        assert_eq!(gpu_watchdog_decision(snap, cfg()), GpuWatchdogAction::Ok);
    }

    #[test]
    fn grace_window_boundary_waits_until_elapsed() {
        // Exactly at the window edge uses `<` so == window ⇒ no longer waiting.
        let c = cfg(); // 60s window
        let at_edge = GpuWatchdogSnapshot {
            gpu_ghs: 0.0,
            floored_streak: 10,
            jobs_flowing: true,
            conn_healthy: true,
            recoveries_done: 1,
            ms_since_recovery: Some(60_000), // == window
        };
        // == window: not waiting anymore; budget remains (1 < 3) ⇒ Recover again.
        assert_eq!(gpu_watchdog_decision(at_edge, c), GpuWatchdogAction::Recover);
        let just_under = GpuWatchdogSnapshot {
            ms_since_recovery: Some(59_999),
            ..at_edge
        };
        assert_eq!(gpu_watchdog_decision(just_under, c), GpuWatchdogAction::Ok);
    }

    #[test]
    fn grace_elapsed_retries_recover_while_budget_remains() {
        // 1 recovery done, window elapsed, still floored, budget 3 ⇒ Recover.
        let snap = GpuWatchdogSnapshot {
            gpu_ghs: 0.0,
            floored_streak: 10,
            jobs_flowing: true,
            conn_healthy: true,
            recoveries_done: 1,
            ms_since_recovery: Some(120_000),
        };
        assert_eq!(gpu_watchdog_decision(snap, cfg()), GpuWatchdogAction::Recover);
    }

    #[test]
    fn grace_elapsed_exits_when_recoveries_exhausted() {
        // max_recoveries reached, window elapsed, still floored ⇒ Exit.
        let snap = GpuWatchdogSnapshot {
            gpu_ghs: 0.0,
            floored_streak: 20,
            jobs_flowing: true,
            conn_healthy: true,
            recoveries_done: cfg().max_recoveries, // 3 done
            ms_since_recovery: Some(120_000),
        };
        assert_eq!(gpu_watchdog_decision(snap, cfg()), GpuWatchdogAction::Exit);
    }

    #[test]
    fn recovery_brought_it_back_disarms_via_not_floored() {
        // If hashrate returned (above floor), the sample is not floored ⇒ Ok
        // regardless of any prior recovery bookkeeping — the decision short-
        // circuits at the floored check, and the driver will reset state.
        let snap = GpuWatchdogSnapshot {
            gpu_ghs: 2.0, // back to life
            floored_streak: 10,
            jobs_flowing: true,
            conn_healthy: true,
            recoveries_done: 2,
            ms_since_recovery: Some(120_000),
        };
        assert_eq!(gpu_watchdog_decision(snap, cfg()), GpuWatchdogAction::Ok);
    }

    // --- snapshot_from assembly ---

    #[test]
    fn snapshot_from_computes_ms_delta_and_carries_streak() {
        let state = GpuWatchdogState {
            floored_streak: 3,
            recoveries_done: 1,
            recovery_at_ms: Some(1_000),
        };
        let snap = snapshot_from(0.0, true, true, &state, 1_000 + 45_000);
        assert_eq!(snap.floored_streak, 3);
        assert_eq!(snap.recoveries_done, 1);
        assert_eq!(snap.ms_since_recovery, Some(45_000));
        // No recovery yet ⇒ None.
        let fresh = GpuWatchdogState::default();
        let snap2 = snapshot_from(0.0, true, true, &fresh, 9_999);
        assert_eq!(snap2.ms_since_recovery, None);
        // Clock step-back saturates to 0, never panics / underflows.
        let snap3 = snapshot_from(0.0, true, true, &state, 500);
        assert_eq!(snap3.ms_since_recovery, Some(0));
    }

    // --- the driver tick + a mock view (no thread, fake clock) ---

    struct MockView {
        ghs: std::sync::Mutex<f64>,
        jobs: AtomicBool,
        healthy: AtomicBool,
        recover_ok: AtomicBool,
        recover_calls: std::sync::atomic::AtomicU32,
        exit_calls: std::sync::atomic::AtomicU32,
    }
    impl MockView {
        fn new(ghs: f64, jobs: bool, healthy: bool, recover_ok: bool) -> Self {
            MockView {
                ghs: std::sync::Mutex::new(ghs),
                jobs: AtomicBool::new(jobs),
                healthy: AtomicBool::new(healthy),
                recover_ok: AtomicBool::new(recover_ok),
                recover_calls: std::sync::atomic::AtomicU32::new(0),
                exit_calls: std::sync::atomic::AtomicU32::new(0),
            }
        }
        fn set_ghs(&self, v: f64) {
            *self.ghs.lock().unwrap() = v;
        }
        fn recover_calls(&self) -> u32 {
            self.recover_calls.load(Ordering::Relaxed)
        }
        fn exit_calls(&self) -> u32 {
            self.exit_calls.load(Ordering::Relaxed)
        }
    }
    impl GpuWatchdogView for MockView {
        fn gpu_ghs(&self) -> f64 {
            *self.ghs.lock().unwrap()
        }
        fn jobs_flowing(&self) -> bool {
            self.jobs.load(Ordering::Relaxed)
        }
        fn conn_healthy(&self) -> bool {
            self.healthy.load(Ordering::Relaxed)
        }
        fn recover(&self) -> bool {
            self.recover_calls.fetch_add(1, Ordering::Relaxed);
            self.recover_ok.load(Ordering::Relaxed)
        }
        fn escalate_exit(&self) {
            // Record instead of exiting — so the test runner survives.
            self.exit_calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn tick_healthy_resets_state_and_does_nothing() {
        let v = MockView::new(3.0, true, true, true);
        let mut st = GpuWatchdogState {
            floored_streak: 2,
            recoveries_done: 1,
            recovery_at_ms: Some(123),
        };
        let a = gpu_watchdog_tick(&v, cfg(), &mut st, 10_000);
        assert_eq!(a, GpuWatchdogAction::Ok);
        // A healthy sample wipes any in-progress stall bookkeeping.
        assert_eq!(st.floored_streak, 0);
        assert_eq!(st.recoveries_done, 0);
        assert_eq!(st.recovery_at_ms, None);
        assert_eq!(v.recover_calls(), 0);
        assert_eq!(v.exit_calls(), 0);
    }

    #[test]
    fn tick_idle_zero_does_not_accumulate_streak() {
        // Floored but no jobs: must NOT build toward a stall (streak stays 0).
        let v = MockView::new(0.0, false, true, true);
        let mut st = GpuWatchdogState::default();
        for _ in 0..10 {
            assert_eq!(gpu_watchdog_tick(&v, cfg(), &mut st, 0), GpuWatchdogAction::Ok);
            assert_eq!(st.floored_streak, 0, "idle gap must not accumulate");
        }
        assert_eq!(v.recover_calls(), 0);
        assert_eq!(v.exit_calls(), 0);
    }

    #[test]
    fn tick_full_stall_sequence_recovers_then_exits() {
        // A GPU that is floored-with-work for the whole episode and whose
        // recovery never restores hashrate must: dwell, Recover up to
        // max_recoveries (waiting out each grace window), then Exit.
        let c = GpuWatchdogCfg {
            dwell_samples: 2,
            max_recoveries: 2,
            recover_window: Duration::from_millis(100),
            ..cfg()
        };
        // recover() "succeeds" (rebuild ok) but hashrate stays floored.
        let v = MockView::new(0.0, true, true, true);
        let mut st = GpuWatchdogState::default();
        let mut t = 0u64;
        let step = 15_000u64; // 15s between samples

        // sample 1: count 1 < dwell 2 ⇒ Ok
        assert_eq!(gpu_watchdog_tick(&v, c, &mut st, t), GpuWatchdogAction::Ok);
        t += step;
        // sample 2: count 2 == dwell, no recovery yet ⇒ Recover #1
        assert_eq!(gpu_watchdog_tick(&v, c, &mut st, t), GpuWatchdogAction::Recover);
        assert_eq!(v.recover_calls(), 1);
        assert_eq!(st.recoveries_done, 1);
        // sample 3: 15s later (> 100ms window), still floored, budget 2>1 ⇒ Recover #2
        t += step;
        assert_eq!(gpu_watchdog_tick(&v, c, &mut st, t), GpuWatchdogAction::Recover);
        assert_eq!(v.recover_calls(), 2);
        assert_eq!(st.recoveries_done, 2);
        // sample 4: window elapsed, still floored, budget exhausted ⇒ Exit
        t += step;
        assert_eq!(gpu_watchdog_tick(&v, c, &mut st, t), GpuWatchdogAction::Exit);
        assert_eq!(v.exit_calls(), 1);
    }

    #[test]
    fn tick_recovery_that_restores_hashrate_disarms() {
        // Dwell, Recover; then hashrate returns ⇒ the next tick is Ok and resets
        // state (no exit).
        let c = GpuWatchdogCfg {
            dwell_samples: 2,
            max_recoveries: 3,
            recover_window: Duration::from_millis(100),
            ..cfg()
        };
        let v = MockView::new(0.0, true, true, true);
        let mut st = GpuWatchdogState::default();
        let mut t = 0u64;
        gpu_watchdog_tick(&v, c, &mut st, t); // count1 Ok
        t += 15_000;
        assert_eq!(gpu_watchdog_tick(&v, c, &mut st, t), GpuWatchdogAction::Recover);
        assert_eq!(v.recover_calls(), 1);
        // Hashrate comes back:
        v.set_ghs(2.0);
        t += 15_000;
        assert_eq!(gpu_watchdog_tick(&v, c, &mut st, t), GpuWatchdogAction::Ok);
        assert_eq!(st.floored_streak, 0, "recovered ⇒ state reset");
        assert_eq!(st.recovery_at_ms, None);
        assert_eq!(v.exit_calls(), 0, "must not exit after a successful recovery");
    }

    #[test]
    fn tick_recover_failure_still_stamps_and_exits_after_window() {
        // If recover() itself fails (rebuild error), we still consume the attempt
        // and start the grace window; with no budget left we exit after it.
        let c = GpuWatchdogCfg {
            dwell_samples: 1,        // act on the first floored-with-work sample
            max_recoveries: 1,
            recover_window: Duration::from_millis(50),
            ..cfg()
        };
        let v = MockView::new(0.0, true, true, false); // recover() returns false
        let mut st = GpuWatchdogState::default();
        let mut t = 0u64;
        // sample 1: count 1 == dwell ⇒ Recover (which fails), attempt consumed.
        assert_eq!(gpu_watchdog_tick(&v, c, &mut st, t), GpuWatchdogAction::Recover);
        assert_eq!(v.recover_calls(), 1);
        assert_eq!(st.recoveries_done, 1);
        assert!(st.recovery_at_ms.is_some());
        // sample 2: window elapsed, still floored, no budget ⇒ Exit.
        t += 15_000;
        assert_eq!(gpu_watchdog_tick(&v, c, &mut st, t), GpuWatchdogAction::Exit);
        assert_eq!(v.exit_calls(), 1);
    }

    #[test]
    fn tick_disabled_is_inert() {
        let off = GpuWatchdogCfg { enabled: false, ..cfg() };
        let v = MockView::new(0.0, true, true, true);
        let mut st = GpuWatchdogState::default();
        for _ in 0..50 {
            assert_eq!(gpu_watchdog_tick(&v, off, &mut st, 0), GpuWatchdogAction::Ok);
        }
        assert_eq!(v.recover_calls(), 0);
        assert_eq!(v.exit_calls(), 0);
    }

    #[test]
    fn spawned_disabled_watchdog_exits_immediately() {
        let v = Arc::new(MockView::new(0.0, true, true, true));
        let off = GpuWatchdogCfg { enabled: false, ..cfg() };
        let stop = Arc::new(AtomicBool::new(false));
        let h = spawn_gpu_watchdog(v.clone(), off, Arc::clone(&stop));
        h.join().unwrap(); // returns without needing `stop`
        assert_eq!(v.recover_calls(), 0);
        assert_eq!(v.exit_calls(), 0);
    }

    #[test]
    fn spawned_watchdog_ticks_and_stops() {
        // Enabled, floored-with-work, tiny poll + dwell: the thread must act
        // (recover at least once) then stop promptly when `stop` flips.
        let v = Arc::new(MockView::new(0.0, true, true, true));
        let c = GpuWatchdogCfg {
            poll: Duration::from_millis(5),
            dwell_samples: 1,
            max_recoveries: 100, // keep recovering (don't exit the test runner)
            recover_window: Duration::from_secs(3600),
            ..cfg()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let h = spawn_gpu_watchdog(v.clone(), c, Arc::clone(&stop));
        std::thread::sleep(Duration::from_millis(80));
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        assert!(v.recover_calls() >= 1, "watchdog thread must tick + recover before stop");
        assert_eq!(v.exit_calls(), 0, "within grace window + budget ⇒ no exit");
    }
}
