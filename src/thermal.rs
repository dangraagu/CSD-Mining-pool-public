//! Pure GPU temperature-limit (thermal-throttle) decision logic.
//!
//! Optional NVML telemetry (feature = "nvml") can read the GPU's core
//! temperature; this module turns a stream of temperature samples into a
//! **pause / resume** decision with hysteresis, so the miner backs off launches
//! when the card runs hot and resumes once it has cooled by a margin. Like
//! `stratum::watchdog` and `gpu_watchdog`, the policy is a **pure function** of
//! an injected sample + [`ThermalCfg`] + the current paused state — no I/O, no
//! clock, no GPU handle — so the whole hysteresis is unit-tested with literals.
//!
//! The thermal pause is a SOFT, client-side throttle layered ON TOP of the GPU's
//! own hardware thermal protection (which the driver enforces regardless). It
//! exists so an operator can cap a card well below its hardware limit (quieter /
//! cooler / longer-lived) and so a runaway-temperature card stops feeding the
//! pool instead of thermal-throttling to a crawl.
//!
//! Crucially, a thermal pause MUST NOT be mistaken for a hung GPU: the GPU stall
//! watchdog (`gpu_watchdog`) only escalates a floored hashrate when work is
//! flowing over a healthy link, so the loop signals "paused for temperature" and
//! the stall watchdog stands down for the duration (see `loop_stratum`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Tunables for the temperature-limit throttle. Defaults are conservative: the
/// throttle is OFF unless the operator opts in with a limit, and the resume
/// margin is a sensible 5 °C of hysteresis below the limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThermalCfg {
    /// Master on/off. `false` ⇒ never pause, regardless of temperature
    /// (behaviour identical to a build without the feature / without a limit).
    pub enabled: bool,
    /// Pause new GPU launches once the core temperature rises ABOVE this (°C).
    pub limit_c: f64,
    /// Resume launches once the core temperature falls AT/BELOW this (°C). Must
    /// be strictly below `limit_c` to give hysteresis (no rapid pause/resume
    /// flapping right at the limit). `build_thermal_cfg` enforces the ordering.
    pub resume_c: f64,
}

impl Default for ThermalCfg {
    fn default() -> Self {
        ThermalCfg {
            enabled: false,
            // Inert defaults — only meaningful once `enabled` is set by an
            // operator-supplied `--temp-limit`. 83/78 are typical safe-but-warm
            // values for consumer NVIDIA cards if ever used directly.
            limit_c: 83.0,
            resume_c: 78.0,
        }
    }
}

/// The throttle's view of the GPU after a sample: should the miner be launching
/// kernels, or paused to let the card cool?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThermalState {
    /// Run normally — temperature is within bounds (or the throttle is off /
    /// telemetry is unavailable).
    Running,
    /// Pause new GPU launches — the card is at/over its configured limit.
    Paused,
}

impl ThermalState {
    /// Convenience: is this the paused state?
    #[inline]
    pub fn is_paused(self) -> bool {
        matches!(self, ThermalState::Paused)
    }
}

/// The pure temperature-limit decision **with hysteresis**.
///
/// Given whether we are CURRENTLY paused, the latest temperature sample (`None`
/// = telemetry unavailable this sample), and the config, decide the next state:
///
///   - Throttle disabled ⇒ always [`ThermalState::Running`].
///   - Temperature unknown (`None` or NaN) ⇒ **fail safe to Running**: we never
///     pause on missing data, and a card that WAS paused resumes rather than
///     getting wedged-paused forever if telemetry drops out. (The GPU's own
///     hardware thermal protection and the stall watchdog remain as backstops.)
///   - Known temperature, hysteresis band:
///       * not paused & `temp > limit_c`      ⇒ Paused   (crossed the ceiling)
///       * not paused & `temp <= limit_c`     ⇒ Running  (still under)
///       * paused     & `temp <= resume_c`    ⇒ Running  (cooled enough)
///       * paused     & `temp >  resume_c`    ⇒ Paused   (still too warm — the
///         hysteresis gap: between `resume_c` and `limit_c` we HOLD the prior
///         paused state instead of flapping).
///
/// Boundary conventions (pinned by tests): the pause edge uses `>` (exactly
/// `limit_c` does NOT pause), the resume edge uses `<=` (exactly `resume_c`
/// resumes). So a card sitting precisely at `limit_c` that was running keeps
/// running, and one sitting precisely at `resume_c` that was paused resumes.
pub fn thermal_decision(currently_paused: bool, temp_c: Option<f64>, cfg: ThermalCfg) -> ThermalState {
    if !cfg.enabled {
        return ThermalState::Running;
    }
    let temp = match temp_c {
        Some(t) if !t.is_nan() => t,
        // Unknown / NaN ⇒ fail safe to Running (never pause on missing data;
        // never stay stuck paused when telemetry is gone).
        _ => return ThermalState::Running,
    };
    if currently_paused {
        // Hysteresis: stay paused until we cool to the resume threshold.
        if temp <= cfg.resume_c {
            ThermalState::Running
        } else {
            ThermalState::Paused
        }
    } else {
        // Running: pause only once we exceed the limit.
        if temp > cfg.limit_c {
            ThermalState::Paused
        } else {
            ThermalState::Running
        }
    }
}

/// Build a validated [`ThermalCfg`] from operator inputs, enforcing the
/// hysteresis ordering so `resume_c < limit_c` always holds.
///
/// `limit_c == None` ⇒ throttle disabled (returns an inert default). When a
/// limit is given, `resume_c` defaults to `limit_c - DEFAULT_HYSTERESIS_C` if
/// not supplied; an explicitly-supplied `resume_c` that is not strictly below
/// the limit is corrected DOWN to `limit_c - DEFAULT_HYSTERESIS_C` (with a
/// floor of 0) and the caller is expected to warn — a non-hysteretic config
/// (resume >= limit) would flap every sample, so we never honour it verbatim.
/// Pure → unit-tested; no clamping surprises are silent in the tests.
pub fn build_thermal_cfg(limit_c: Option<f64>, resume_c: Option<f64>) -> ThermalCfg {
    let limit = match limit_c {
        Some(l) => l,
        None => return ThermalCfg::default(), // disabled
    };
    let resume = match resume_c {
        Some(r) if r < limit => r,
        // Missing OR not-below-limit ⇒ derive a sane hysteresis gap below limit.
        _ => (limit - DEFAULT_HYSTERESIS_C).max(0.0),
    };
    ThermalCfg {
        enabled: true,
        limit_c: limit,
        resume_c: resume,
    }
}

/// Default hysteresis gap (°C) between the pause limit and the resume threshold
/// when the operator gives a `--temp-limit` but no explicit `--temp-resume`.
pub const DEFAULT_HYSTERESIS_C: f64 = 5.0;

/// A `'static`, lock-free shared flag the mining loop reads to decide whether to
/// skip GPU launches, and the thermal poller writes after each decision. Also
/// exposes the latest paused state to the GPU stall watchdog so a thermal pause
/// is never mistaken for a hung GPU.
///
/// `false` = run, `true` = paused-for-temperature. Starts un-paused.
#[derive(Debug, Default)]
pub struct ThermalGate {
    paused: AtomicBool,
}

impl ThermalGate {
    /// A fresh, un-paused gate.
    pub fn new() -> Self {
        ThermalGate {
            paused: AtomicBool::new(false),
        }
    }
    /// True iff the miner is currently paused for temperature.
    #[inline]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
    /// Apply a freshly-computed [`ThermalState`], updating the shared flag.
    /// Returns `true` if the state CHANGED (so the caller can log the
    /// pause/resume transition exactly once).
    pub fn apply(&self, state: ThermalState) -> bool {
        let now = state.is_paused();
        let prev = self.paused.swap(now, Ordering::Relaxed);
        prev != now
    }
}

/// Drive one thermal decision from the gate's CURRENT state + a fresh sample,
/// updating the gate. Returns `(new_state, changed)`. Factored out so the poller
/// loop body is itself unit-tested without a thread or a real GPU. Pure w.r.t.
/// the injected `temp_c`; the only effect is the gate's atomic.
pub fn thermal_tick(gate: &ThermalGate, temp_c: Option<f64>, cfg: ThermalCfg) -> (ThermalState, bool) {
    let state = thermal_decision(gate.is_paused(), temp_c, cfg);
    let changed = gate.apply(state);
    (state, changed)
}

/// Spawn the thermal-poll thread: every `poll`, read the GPU temperature via
/// `sample` and update `gate`, until `stop` is set. Sleeps in small slices so it
/// honours `stop` promptly. No-op (returns an immediately-joinable thread) when
/// `cfg.enabled` is false. `sample` returns `Some(temp_c)` or `None` if
/// telemetry is unavailable; the caller wires it to NVML (feature = "nvml") or a
/// stub. The caller may detach the handle — the thread owns its `Arc`s.
pub fn spawn_thermal_poller<F>(
    gate: Arc<ThermalGate>,
    cfg: ThermalCfg,
    poll: std::time::Duration,
    sample: F,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()>
where
    F: Fn() -> Option<f64> + Send + 'static,
{
    std::thread::Builder::new()
        .name("thermal-poller".to_string())
        .spawn(move || {
            if !cfg.enabled {
                return;
            }
            tracing::info!(
                "thermal: armed (limit={:.0}°C, resume={:.0}°C, poll={:?})",
                cfg.limit_c,
                cfg.resume_c,
                poll,
            );
            let slice = std::time::Duration::from_millis(200).min(poll);
            let mut waited = std::time::Duration::ZERO;
            // Evaluate once up front so a hot start pauses promptly.
            {
                let (_s, _c) = thermal_tick(&gate, sample(), cfg);
            }
            while !stop.load(Ordering::Relaxed) {
                if waited >= poll {
                    waited = std::time::Duration::ZERO;
                    let (state, changed) = thermal_tick(&gate, sample(), cfg);
                    if changed {
                        match state {
                            ThermalState::Paused => tracing::warn!(
                                "thermal: PAUSING GPU launches — temperature above limit {:.0}°C (will resume at/below {:.0}°C)",
                                cfg.limit_c, cfg.resume_c,
                            ),
                            ThermalState::Running => tracing::info!(
                                "thermal: RESUMING GPU launches — temperature back at/below {:.0}°C",
                                cfg.resume_c,
                            ),
                        }
                    }
                }
                std::thread::sleep(slice);
                waited += slice;
            }
        })
        .expect("spawning thermal-poller thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ThermalCfg {
        ThermalCfg {
            enabled: true,
            limit_c: 80.0,
            resume_c: 75.0,
        }
    }

    // --- the pure decision: disabled / unknown ---

    #[test]
    fn disabled_never_pauses_even_when_hot() {
        let off = ThermalCfg { enabled: false, ..cfg() };
        assert_eq!(thermal_decision(false, Some(99.0), off), ThermalState::Running);
        // Even if somehow already paused, disabled ⇒ Running.
        assert_eq!(thermal_decision(true, Some(99.0), off), ThermalState::Running);
    }

    #[test]
    fn unknown_temperature_fails_safe_to_running() {
        // None ⇒ never pause...
        assert_eq!(thermal_decision(false, None, cfg()), ThermalState::Running);
        // ...and a paused card with telemetry now gone RESUMES (never wedged).
        assert_eq!(thermal_decision(true, None, cfg()), ThermalState::Running);
        // NaN is treated exactly like None.
        assert_eq!(thermal_decision(false, Some(f64::NAN), cfg()), ThermalState::Running);
        assert_eq!(thermal_decision(true, Some(f64::NAN), cfg()), ThermalState::Running);
    }

    // --- the hysteresis core ---

    #[test]
    fn running_pauses_only_above_limit() {
        // Below the limit ⇒ keep running.
        assert_eq!(thermal_decision(false, Some(70.0), cfg()), ThermalState::Running);
        // EXACTLY at the limit ⇒ still running (pause edge is strict `>`).
        assert_eq!(thermal_decision(false, Some(80.0), cfg()), ThermalState::Running);
        // Above the limit ⇒ pause.
        assert_eq!(thermal_decision(false, Some(80.1), cfg()), ThermalState::Paused);
        assert_eq!(thermal_decision(false, Some(95.0), cfg()), ThermalState::Paused);
    }

    #[test]
    fn paused_resumes_only_at_or_below_resume_threshold() {
        // Still above resume ⇒ stay paused (the hysteresis band).
        assert_eq!(thermal_decision(true, Some(79.0), cfg()), ThermalState::Paused);
        assert_eq!(thermal_decision(true, Some(75.1), cfg()), ThermalState::Paused);
        // EXACTLY at resume ⇒ resume (resume edge is `<=`).
        assert_eq!(thermal_decision(true, Some(75.0), cfg()), ThermalState::Running);
        // Below resume ⇒ resume.
        assert_eq!(thermal_decision(true, Some(60.0), cfg()), ThermalState::Running);
    }

    #[test]
    fn hysteresis_band_holds_prior_state_between_resume_and_limit() {
        // In the band (resume, limit] the decision keeps the PRIOR state:
        // a temp of 77°C (between 75 and 80) holds whatever we were.
        let mid = 77.0;
        assert_eq!(thermal_decision(false, Some(mid), cfg()), ThermalState::Running, "running stays running in-band");
        assert_eq!(thermal_decision(true, Some(mid), cfg()), ThermalState::Paused, "paused stays paused in-band");
    }

    #[test]
    fn full_heat_then_cool_cycle_has_single_transitions() {
        // Simulate a heat-up past the limit then a cool-down past resume, feeding
        // the decision its own prior output. Assert exactly one pause edge and
        // one resume edge, and that the band does not flap.
        let c = cfg();
        let mut paused = false;
        // Heat up: 70 -> 79 -> 80 -> 81 (pause here) -> 82.
        let heat = [70.0, 79.0, 80.0, 81.0, 82.0];
        let mut pause_edges = 0;
        for t in heat {
            let next = thermal_decision(paused, Some(t), c).is_paused();
            if next && !paused {
                pause_edges += 1;
            }
            paused = next;
        }
        assert_eq!(pause_edges, 1, "exactly one pause transition on the way up");
        assert!(paused, "ends paused after exceeding the limit");
        // Cool down: 79 -> 76 -> 75 (resume here) -> 70. Must NOT resume at 79/76
        // (still above resume) — that's the whole point of hysteresis.
        let cool = [79.0, 76.0, 75.0, 70.0];
        let mut resume_edges = 0;
        for t in cool {
            let next = thermal_decision(paused, Some(t), c).is_paused();
            if !next && paused {
                resume_edges += 1;
            }
            paused = next;
        }
        assert_eq!(resume_edges, 1, "exactly one resume transition on the way down");
        assert!(!paused, "ends running after cooling below resume");
    }

    // --- build_thermal_cfg validation ---

    #[test]
    fn build_cfg_none_limit_is_disabled() {
        let c = build_thermal_cfg(None, None);
        assert!(!c.enabled);
        // Disabled cfg never pauses.
        assert_eq!(thermal_decision(false, Some(120.0), c), ThermalState::Running);
    }

    #[test]
    fn build_cfg_derives_default_hysteresis_when_resume_missing() {
        let c = build_thermal_cfg(Some(83.0), None);
        assert!(c.enabled);
        assert_eq!(c.limit_c, 83.0);
        assert_eq!(c.resume_c, 83.0 - DEFAULT_HYSTERESIS_C); // 78.0
    }

    #[test]
    fn build_cfg_honours_valid_explicit_resume() {
        let c = build_thermal_cfg(Some(80.0), Some(70.0));
        assert_eq!(c.resume_c, 70.0);
        assert!(c.resume_c < c.limit_c);
    }

    #[test]
    fn build_cfg_corrects_non_hysteretic_resume_down() {
        // resume >= limit would flap every sample — corrected to limit - default.
        let c = build_thermal_cfg(Some(80.0), Some(85.0));
        assert_eq!(c.resume_c, 80.0 - DEFAULT_HYSTERESIS_C); // 75.0
        assert!(c.resume_c < c.limit_c);
        // Equal is also corrected (strict `<` required).
        let c2 = build_thermal_cfg(Some(80.0), Some(80.0));
        assert_eq!(c2.resume_c, 75.0);
    }

    #[test]
    fn build_cfg_floors_resume_at_zero() {
        // A tiny limit can't produce a negative resume threshold.
        let c = build_thermal_cfg(Some(3.0), None);
        assert_eq!(c.resume_c, 0.0);
        assert!(c.resume_c < c.limit_c);
    }

    // --- ThermalGate + thermal_tick (the mutable half) ---

    #[test]
    fn gate_starts_unpaused_and_tracks_changes() {
        let g = ThermalGate::new();
        assert!(!g.is_paused());
        // First crossing above limit ⇒ paused, changed = true.
        let (s, changed) = thermal_tick(&g, Some(90.0), cfg());
        assert_eq!(s, ThermalState::Paused);
        assert!(changed, "first pause is a change");
        assert!(g.is_paused());
        // A second hot sample ⇒ still paused, changed = false.
        let (s2, changed2) = thermal_tick(&g, Some(90.0), cfg());
        assert_eq!(s2, ThermalState::Paused);
        assert!(!changed2, "staying paused is not a change");
        // In-band cool (77°C) ⇒ HOLDS paused (hysteresis), no change.
        let (s3, changed3) = thermal_tick(&g, Some(77.0), cfg());
        assert_eq!(s3, ThermalState::Paused);
        assert!(!changed3);
        // Cool to/below resume ⇒ resume, changed = true.
        let (s4, changed4) = thermal_tick(&g, Some(74.0), cfg());
        assert_eq!(s4, ThermalState::Running);
        assert!(changed4, "resume is a change");
        assert!(!g.is_paused());
    }

    #[test]
    fn gate_apply_reports_transition_only() {
        let g = ThermalGate::new();
        assert!(!g.apply(ThermalState::Running), "no-op stays running");
        assert!(g.apply(ThermalState::Paused), "running->paused changes");
        assert!(!g.apply(ThermalState::Paused), "paused->paused no change");
        assert!(g.apply(ThermalState::Running), "paused->running changes");
    }

    // --- spawn_thermal_poller driver ---

    #[test]
    fn spawned_disabled_poller_exits_immediately() {
        let g = Arc::new(ThermalGate::new());
        let off = ThermalCfg { enabled: false, ..cfg() };
        let stop = Arc::new(AtomicBool::new(false));
        let h = spawn_thermal_poller(
            g.clone(),
            off,
            std::time::Duration::from_millis(5),
            || Some(120.0),
            stop,
        );
        h.join().unwrap(); // returns without needing `stop`
        assert!(!g.is_paused(), "disabled poller never pauses");
    }

    #[test]
    fn spawned_poller_pauses_on_hot_sample_then_stops() {
        let g = Arc::new(ThermalGate::new());
        let stop = Arc::new(AtomicBool::new(false));
        // Always reads a temperature above the limit ⇒ the poller must pause.
        let h = spawn_thermal_poller(
            g.clone(),
            cfg(),
            std::time::Duration::from_millis(5),
            || Some(95.0),
            Arc::clone(&stop),
        );
        // The up-front evaluation pauses immediately; give the thread a moment.
        std::thread::sleep(std::time::Duration::from_millis(40));
        assert!(g.is_paused(), "poller must pause on a hot sample");
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
    }

    #[test]
    fn spawned_poller_resumes_when_cooled() {
        use std::sync::atomic::AtomicU64;
        let g = Arc::new(ThermalGate::new());
        let stop = Arc::new(AtomicBool::new(false));
        // First few samples hot (pause), then cold (resume). A shared counter
        // drives the transition deterministically without real temperatures.
        let n = Arc::new(AtomicU64::new(0));
        let n2 = n.clone();
        let h = spawn_thermal_poller(
            g.clone(),
            cfg(),
            std::time::Duration::from_millis(5),
            move || {
                let i = n2.fetch_add(1, Ordering::Relaxed);
                Some(if i < 3 { 95.0 } else { 60.0 })
            },
            Arc::clone(&stop),
        );
        // Wait long enough for several polls: pause then resume.
        std::thread::sleep(std::time::Duration::from_millis(120));
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();
        assert!(!g.is_paused(), "poller must resume after cooling below resume");
        assert!(n.load(Ordering::Relaxed) >= 4, "poller sampled several times");
    }
}
