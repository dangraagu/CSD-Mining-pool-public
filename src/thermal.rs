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
    /// **Max-pause dead-man cap** (seconds). If the thermal gate stays
    /// CONTINUOUSLY paused for longer than this, the poller forces a bounded
    /// resume window so the GPU stall watchdog can adjudicate (see [`Deadman`]).
    /// Defaults to [`DEFAULT_MAX_PAUSE_SECS`] (10 min) — deliberately long so it
    /// never interferes with legitimate sustained thermal throttling. Only
    /// meaningful when `enabled`.
    pub max_pause_secs: u64,
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
            max_pause_secs: DEFAULT_MAX_PAUSE_SECS,
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
/// `max_pause_secs == None` ⇒ the [`DEFAULT_MAX_PAUSE_SECS`] dead-man cap.
///
/// Pure → unit-tested; no clamping surprises are silent in the tests.
pub fn build_thermal_cfg(
    limit_c: Option<f64>,
    resume_c: Option<f64>,
    max_pause_secs: Option<u64>,
) -> ThermalCfg {
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
        max_pause_secs: max_pause_secs.unwrap_or(DEFAULT_MAX_PAUSE_SECS),
    }
}

/// Default hysteresis gap (°C) between the pause limit and the resume threshold
/// when the operator gives a `--temp-limit` but no explicit `--temp-resume`.
pub const DEFAULT_HYSTERESIS_C: f64 = 5.0;

/// Default **max-pause dead-man** cap (seconds): how long the thermal gate may
/// stay CONTINUOUSLY paused before the poller forces a watchdog-adjudication
/// window. 10 minutes — deliberately long and conservative so it NEVER interferes
/// with legitimate sustained thermal throttling (a card that is genuinely just
/// hot is expected to sit paused for many minutes; only a card paused for an
/// unreasonably long uninterrupted stretch — the hallmark of a hang that NVML
/// keeps reporting as hot — trips the dead-man). Operator-tunable via
/// `--temp-max-pause-secs`.
pub const DEFAULT_MAX_PAUSE_SECS: u64 = 600;

/// Length of the forced-resume window the dead-man opens once the cap is hit.
///
/// WHY a WINDOW and not a single tick: the GPU stall watchdog only escalates a
/// floored GPU after `GpuWatchdogCfg::dwell_samples` (default 4) CONSECUTIVE
/// floored-with-work polls at its ~15 s cadence (~60 s), and it RESETS that
/// streak the instant the gate re-pauses (a thermal pause reports
/// `jobs_flowing == false`). So a one-poll blip of "Running" can never let the
/// watchdog adjudicate a genuine hang — the streak would reset before dwell is
/// reached. The dead-man therefore HOLDS the gate Running for a bounded window
/// comfortably longer than the watchdog's dwell×poll, so:
///   - a genuinely HUNG card (still reporting hot, cannot cool because it is not
///     actually hashing) stays floored-with-work for the whole window ⇒ the
///     watchdog accumulates its dwell and recovers/exit(17)s it; whereas
///   - a merely HOT (healthy, throttling) card resumes hashing and cools; the
///     window costs it at most ~one window of hot mining per cap interval (≤90 s
///     of mining per 10 min paused = negligible), after which the gate re-pauses
///     on the next hot sample under normal hysteresis.
/// 90 s = the 60 s dwell plus margin for poll-phase alignment. std-only.
pub const FORCE_RESUME_WINDOW_SECS: u64 = 90;

/// The max-pause **dead-man**: a tiny, pure, clock-injected state machine that
/// bounds how long the thermal gate may sit CONTINUOUSLY paused before it forces
/// a watchdog-adjudication window. Defence-in-depth ONLY (a LOW-severity
/// hardening): the common case never reaches it, because a stopped GPU cools (so
/// hysteresis resumes normally) and most real hangs break NVML reads (which
/// `thermal_decision` fail-safes to Running). It catches the residual case where
/// a hung GPU keeps reporting temp > resume forever — without it, the watchdog
/// would stay suppressed (a thermal pause stands the stall-watchdog down) and a
/// real hang could be masked indefinitely.
///
/// State machine (driven once per poll by [`Deadman::tick`], which is fed the
/// gate's just-computed [`ThermalState`] and the time elapsed since the previous
/// tick — the ONLY clock dependency, injected so it is unit-tested with literal
/// `Duration`s):
///   - **Not paused** ⇒ reset everything (`paused_for = 0`, window closed). This
///     is what makes the cap apply only to a CONTINUOUS pause: any genuine resume
///     (cooled, or telemetry dropped out) zeroes the timer.
///   - **Paused, no window open, `paused_for < cap`** ⇒ accrue `paused_for`, do
///     nothing (honour the throttle).
///   - **Paused, no window open, `paused_for >= cap`** ⇒ OPEN a forced-resume
///     window: override the applied state to Running, and reset `paused_for` to 0
///     so the NEXT dead-man check is a full cap away (a healthy card that re-pauses
///     after the window does not immediately re-trip). Signals `forced_open` so
///     the poller logs it loudly.
///   - **Window open, not yet `FORCE_RESUME_WINDOW_SECS` elapsed** ⇒ keep the
///     override Running (hold the gate open across the watchdog's dwell) regardless
///     of how hot the sample reads.
///   - **Window open, window elapsed** ⇒ close it and hand control back to normal
///     thermal hysteresis on the very next tick.
///
/// On the default / non-nvml build the thermal throttle is disabled and the gate
/// is never paused, so `tick` only ever takes the "not paused ⇒ reset" branch and
/// the dead-man is an inert no-op (it is only ever driven from inside the
/// `cfg.enabled` thermal path — see the pollers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadman {
    /// How long we have been CONTINUOUSLY paused since the last resume / since the
    /// last forced window opened. Reset to zero on any resume or on a force.
    paused_for: std::time::Duration,
    /// Time remaining in an OPEN forced-resume window, or `None` if no window is
    /// open. While `Some(_)`, the gate is held Running so the watchdog can act.
    forcing_left: Option<std::time::Duration>,
}

/// What the poller must do with the dead-man's verdict for this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadmanOutcome {
    /// The state the poller should actually APPLY to the gate this tick. Equals
    /// the input thermal state unless the dead-man is forcing a resume, in which
    /// case it is forced to [`ThermalState::Running`].
    pub apply_state: ThermalState,
    /// `true` on the single tick the window OPENS (the cap was just exceeded), so
    /// the poller logs the dead-man activation exactly once per window.
    pub forced_open: bool,
}

impl Default for Deadman {
    fn default() -> Self {
        Deadman {
            paused_for: std::time::Duration::ZERO,
            forcing_left: None,
        }
    }
}

impl Deadman {
    /// A fresh dead-man (no pause accrued, no window open).
    pub fn new() -> Self {
        Deadman::default()
    }

    /// Is a forced-resume window currently open? (Test/inspection helper.)
    #[inline]
    pub fn is_forcing(&self) -> bool {
        self.forcing_left.is_some()
    }

    /// Advance the dead-man by one poll. `state` is the gate decision JUST computed
    /// by [`thermal_decision`] / [`thermal_tick`] for this sample; `elapsed` is the
    /// wall time since the previous `tick` (injected — the only clock input);
    /// `cap` is the continuous-pause limit (`ThermalCfg::max_pause_secs`); `window`
    /// is [`FORCE_RESUME_WINDOW_SECS`]. Returns the [`DeadmanOutcome`] telling the
    /// poller what to apply + whether to log a fresh activation. Pure: the only
    /// mutation is `self`'s two fields; no I/O, no real clock.
    pub fn tick(
        &mut self,
        state: ThermalState,
        elapsed: std::time::Duration,
        cap: std::time::Duration,
        window: std::time::Duration,
    ) -> DeadmanOutcome {
        // A forced window in progress takes precedence: hold Running until it
        // elapses, no matter what the sample says (this is the span we give the
        // stall watchdog to adjudicate a possible hang).
        if let Some(left) = self.forcing_left {
            let remaining = left.saturating_sub(elapsed);
            if remaining.is_zero() {
                // Window done — close it and resume normal thermal control NEXT
                // tick. This tick still applies the gate's own decision.
                self.forcing_left = None;
                // If we're still paused, start re-accruing toward the next cap.
                self.paused_for = std::time::Duration::ZERO;
                return DeadmanOutcome { apply_state: state, forced_open: false };
            } else {
                self.forcing_left = Some(remaining);
                return DeadmanOutcome { apply_state: ThermalState::Running, forced_open: false };
            }
        }

        // No window open. Only a CONTINUOUS pause accrues; any resume zeroes it.
        if !state.is_paused() {
            self.paused_for = std::time::Duration::ZERO;
            return DeadmanOutcome { apply_state: state, forced_open: false };
        }

        // Paused: accrue, and trip the dead-man once we exceed the cap.
        self.paused_for = self.paused_for.saturating_add(elapsed);
        if self.paused_for >= cap {
            // Open a forced-resume window: hold Running for `window`, and reset the
            // pause timer so the next trip is a full cap away.
            self.forcing_left = Some(window);
            self.paused_for = std::time::Duration::ZERO;
            DeadmanOutcome { apply_state: ThermalState::Running, forced_open: true }
        } else {
            // Under the cap: honour the throttle.
            DeadmanOutcome { apply_state: state, forced_open: false }
        }
    }
}

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

/// Like [`thermal_tick`], but additionally runs the max-pause [`Deadman`] so a
/// gate held paused beyond `cfg.max_pause_secs` is force-resumed for a bounded
/// window (letting the GPU stall watchdog adjudicate a possible hang). `elapsed`
/// is the wall time since the previous call (injected — keeps this unit-testable
/// without a real clock). Returns `(applied_state, changed, forced_open)`:
///   - `applied_state` is what was written to the gate (the thermal decision,
///     or forced `Running` while the dead-man window is open),
///   - `changed` is whether the gate's flag flipped (for once-per-edge logging),
///   - `forced_open` is `true` only on the tick the dead-man window OPENS (so the
///     caller logs the activation loudly, exactly once).
///
/// Composition note: the dead-man feeds on the RAW thermal decision (so its
/// continuous-pause timer tracks the true thermal verdict), but the gate is
/// updated with the possibly-overridden state. While a window is open the raw
/// decision keeps re-deriving `Paused` (the card is still hot, and we just set the
/// gate Running) — the dead-man absorbs that and re-emits `Running` until the
/// window elapses, then hands control straight back to hysteresis.
pub fn thermal_tick_with_deadman(
    gate: &ThermalGate,
    deadman: &mut Deadman,
    temp_c: Option<f64>,
    cfg: ThermalCfg,
    elapsed: std::time::Duration,
) -> (ThermalState, bool, bool) {
    let decision = thermal_decision(gate.is_paused(), temp_c, cfg);
    let outcome = deadman.tick(
        decision,
        elapsed,
        std::time::Duration::from_secs(cfg.max_pause_secs),
        std::time::Duration::from_secs(FORCE_RESUME_WINDOW_SECS),
    );
    let changed = gate.apply(outcome.apply_state);
    (outcome.apply_state, changed, outcome.forced_open)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ThermalCfg {
        ThermalCfg {
            enabled: true,
            limit_c: 80.0,
            resume_c: 75.0,
            max_pause_secs: DEFAULT_MAX_PAUSE_SECS,
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
        let c = build_thermal_cfg(None, None, None);
        assert!(!c.enabled);
        // Disabled cfg never pauses.
        assert_eq!(thermal_decision(false, Some(120.0), c), ThermalState::Running);
    }

    #[test]
    fn build_cfg_derives_default_hysteresis_when_resume_missing() {
        let c = build_thermal_cfg(Some(83.0), None, None);
        assert!(c.enabled);
        assert_eq!(c.limit_c, 83.0);
        assert_eq!(c.resume_c, 83.0 - DEFAULT_HYSTERESIS_C); // 78.0
    }

    #[test]
    fn build_cfg_honours_valid_explicit_resume() {
        let c = build_thermal_cfg(Some(80.0), Some(70.0), None);
        assert_eq!(c.resume_c, 70.0);
        assert!(c.resume_c < c.limit_c);
    }

    #[test]
    fn build_cfg_corrects_non_hysteretic_resume_down() {
        // resume >= limit would flap every sample — corrected to limit - default.
        let c = build_thermal_cfg(Some(80.0), Some(85.0), None);
        assert_eq!(c.resume_c, 80.0 - DEFAULT_HYSTERESIS_C); // 75.0
        assert!(c.resume_c < c.limit_c);
        // Equal is also corrected (strict `<` required).
        let c2 = build_thermal_cfg(Some(80.0), Some(80.0), None);
        assert_eq!(c2.resume_c, 75.0);
    }

    #[test]
    fn build_cfg_floors_resume_at_zero() {
        // A tiny limit can't produce a negative resume threshold.
        let c = build_thermal_cfg(Some(3.0), None, None);
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

    // --- max-pause dead-man (the watchdog-recovery backstop) ---

    use std::time::Duration;

    const CAP: Duration = Duration::from_secs(600);
    const WIN: Duration = Duration::from_secs(90);

    #[test]
    fn deadman_under_cap_stays_paused() {
        // A gate held paused for LESS than the cap must never be overridden: the
        // dead-man honours legitimate sustained throttling.
        let mut dm = Deadman::new();
        let step = Duration::from_secs(60);
        let mut elapsed = Duration::ZERO;
        // 9 minutes of continuous pause (< 10 min cap): every tick stays Paused,
        // never forces.
        while elapsed + step < CAP {
            let out = dm.tick(ThermalState::Paused, step, CAP, WIN);
            assert_eq!(out.apply_state, ThermalState::Paused, "under the cap the gate stays paused");
            assert!(!out.forced_open, "no forced window under the cap");
            assert!(!dm.is_forcing());
            elapsed += step;
        }
    }

    #[test]
    fn deadman_force_resumes_exactly_once_past_cap_then_repauses() {
        // The core property: held paused PAST the cap, the dead-man forces ONE
        // resume window (so the watchdog can adjudicate), and — if the card is
        // still hot afterwards — the gate RE-PAUSES rather than forcing again.
        let mut dm = Deadman::new();
        let step = Duration::from_secs(60);

        // Accrue right up to the cap: 10 ticks × 60s = 600s. The tick that REACHES
        // the cap opens the window (apply Running, forced_open = true) exactly once.
        let mut force_opens = 0;
        for i in 1..=10 {
            let out = dm.tick(ThermalState::Paused, step, CAP, WIN);
            if i < 10 {
                assert_eq!(out.apply_state, ThermalState::Paused, "still paused before the cap");
                assert!(!out.forced_open);
            } else {
                // 600s reached ⇒ window opens this tick.
                assert_eq!(out.apply_state, ThermalState::Running, "cap reached ⇒ forced resume");
                assert!(out.forced_open, "the activation is signalled exactly on the opening tick");
                assert!(dm.is_forcing());
            }
            if out.forced_open {
                force_opens += 1;
            }
        }
        assert_eq!(force_opens, 1, "the cap fires the dead-man exactly once");

        // The window holds Running across the watchdog's dwell. Advance through it;
        // still-hot samples must NOT reopen a second window — the window simply
        // stays open until it elapses, with no further `forced_open`.
        let mut held = Duration::ZERO;
        while held + step < WIN {
            let out = dm.tick(ThermalState::Paused, step, CAP, WIN);
            assert_eq!(out.apply_state, ThermalState::Running, "held Running for the whole window");
            assert!(!out.forced_open, "no re-activation while the window is open");
            held += step;
        }
        // The tick that consumes the rest of the window closes it; this tick hands
        // control back to the gate's own decision (still Paused, the card is hot).
        let closing = dm.tick(ThermalState::Paused, step, CAP, WIN);
        assert_eq!(closing.apply_state, ThermalState::Paused, "window closed ⇒ re-pauses (still hot)");
        assert!(!closing.forced_open);
        assert!(!dm.is_forcing(), "window is closed after it elapses");

        // And it does NOT immediately re-force: the pause timer was reset, so the
        // next trip is a full cap away. One more tick stays paused, no force.
        let after = dm.tick(ThermalState::Paused, step, CAP, WIN);
        assert_eq!(after.apply_state, ThermalState::Paused, "no immediate second force — a full cap must re-accrue");
        assert!(!after.forced_open);
    }

    #[test]
    fn deadman_resume_resets_the_continuous_pause_timer() {
        // A genuine resume partway through must zero the accrual, so the cap only
        // ever bounds a CONTINUOUS pause (a card that cools and re-heats restarts
        // the clock — exactly the legitimate-throttle case we must not disturb).
        let mut dm = Deadman::new();
        let step = Duration::from_secs(60);
        // 9 minutes paused (under cap)...
        for _ in 0..9 {
            let out = dm.tick(ThermalState::Paused, step, CAP, WIN);
            assert!(!out.forced_open);
        }
        // ...then a resume (cooled / telemetry blip) zeroes the timer.
        let r = dm.tick(ThermalState::Running, step, CAP, WIN);
        assert_eq!(r.apply_state, ThermalState::Running);
        assert!(!r.forced_open);
        // Now another 9 minutes paused must STILL be under the cap (the clock
        // restarted), so no force fires.
        for _ in 0..9 {
            let out = dm.tick(ThermalState::Paused, step, CAP, WIN);
            assert_eq!(out.apply_state, ThermalState::Paused);
            assert!(!out.forced_open, "the continuous-pause timer restarted on resume; no force yet");
        }
    }

    #[test]
    fn deadman_hung_card_window_spans_the_watchdog_dwell() {
        // Sizing guard: the forced window must outlast the GPU watchdog's
        // dwell×poll so a genuinely hung (still-hot) card is held Running long
        // enough for the watchdog to accumulate its streak and act. Encodes the
        // dependency on the watchdog defaults (dwell 4 × poll 15s = 60s).
        let watchdog_dwell_span = Duration::from_secs(4 * 15);
        assert!(
            Duration::from_secs(FORCE_RESUME_WINDOW_SECS) > watchdog_dwell_span,
            "the forced-resume window must exceed the watchdog dwell so a hung GPU gets adjudicated"
        );
    }

    #[test]
    fn thermal_tick_with_deadman_forces_gate_running_past_cap_then_repauses() {
        // The composition the pollers actually use: a real ThermalGate driven by
        // thermal_tick_with_deadman with a SMALL cap so the test is fast. A
        // permanently-hot sample would normally wedge the gate paused forever;
        // the dead-man must flip the gate to Running once, hold it across a short
        // window, then let it re-pause.
        let gate = ThermalGate::new();
        let mut dm = Deadman::new();
        // cap = 3s, window = 2s, poll-step = 1s. Always-hot sample (95 > limit 80).
        let small = ThermalCfg { max_pause_secs: 3, ..cfg() };
        let win = std::time::Duration::from_secs(FORCE_RESUME_WINDOW_SECS);
        // Drive with a 1s elapsed each tick, but override the window via a direct
        // dead-man so the test stays fast: use the real helper for the gate/decision
        // wiring, and check the FORCE happens when the cap is crossed.
        let step = std::time::Duration::from_secs(1);
        let hot = Some(95.0_f64);

        // t=1: paused (hot), under cap.
        let (s1, _c1, f1) = thermal_tick_with_deadman(&gate, &mut dm, hot, small, step);
        assert_eq!(s1, ThermalState::Paused);
        assert!(!f1);
        assert!(gate.is_paused(), "gate paused while hot and under cap");
        // t=2: still paused, under cap.
        let (_s2, _c2, f2) = thermal_tick_with_deadman(&gate, &mut dm, hot, small, step);
        assert!(!f2);
        assert!(gate.is_paused());
        // t=3: cap (3s) reached ⇒ FORCE Running, gate flips to Running, logged once.
        let (s3, c3, f3) = thermal_tick_with_deadman(&gate, &mut dm, hot, small, step);
        assert_eq!(s3, ThermalState::Running, "cap crossed ⇒ forced resume applied to the gate");
        assert!(f3, "forced_open signalled exactly once");
        assert!(c3, "gate flipped paused→running");
        assert!(!gate.is_paused(), "the dead-man actually un-paused the live gate");
        assert!(dm.is_forcing());
        let _ = win; // window length asserted elsewhere; here we only need the flip + hold

        // Hold across the window: still hot, but the gate stays Running and no
        // second force fires while the window is open. Advance enough 1s steps to
        // exceed FORCE_RESUME_WINDOW_SECS.
        let mut held = 0u64;
        while held < FORCE_RESUME_WINDOW_SECS {
            let (_s, _c, f) = thermal_tick_with_deadman(&gate, &mut dm, hot, small, step);
            assert!(!f, "no re-activation while the forced window is open");
            held += 1;
            if !dm.is_forcing() {
                break; // window just closed on this tick
            }
            assert!(!gate.is_paused(), "gate held Running for the whole window");
        }
        // After the window closes, the still-hot card re-pauses the gate.
        let (s_after, _c, f_after) = thermal_tick_with_deadman(&gate, &mut dm, hot, small, step);
        assert_eq!(s_after, ThermalState::Paused, "window over + still hot ⇒ gate re-pauses");
        assert!(!f_after, "no immediate second force — a fresh cap must re-accrue");
        assert!(gate.is_paused());
    }

    #[test]
    fn build_cfg_default_max_pause_when_unset() {
        // The cap defaults to DEFAULT_MAX_PAUSE_SECS, and an explicit value is
        // honoured verbatim.
        let c = build_thermal_cfg(Some(80.0), None, None);
        assert_eq!(c.max_pause_secs, DEFAULT_MAX_PAUSE_SECS);
        let c2 = build_thermal_cfg(Some(80.0), None, Some(300));
        assert_eq!(c2.max_pause_secs, 300);
        // Disabled cfg (no limit) still carries the default cap (inert).
        let off = build_thermal_cfg(None, None, Some(42));
        assert!(!off.enabled);
        assert_eq!(off.max_pause_secs, DEFAULT_MAX_PAUSE_SECS);
    }
}
