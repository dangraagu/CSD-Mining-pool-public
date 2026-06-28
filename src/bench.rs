//! Startup hashrate micro-benchmark for the `mining.suggest_difficulty` hint.
//!
//! Measures the SELECTED backend's raw hashrate by sweeping [`MiningBackend::
//! hash_range`] against an **all-zero target** (which no sha256d output can ever
//! satisfy, so the sweep runs the whole range and returns `None` — an honest
//! "how many nonces/second does this backend do" measurement, reusing the exact
//! timing pattern in [`crate::selftest`]).
//!
//! Each iteration sweeps the FULL `[0, u32::MAX)` nonce range in one
//! `hash_range` call (mirroring [`crate::backends::cuda::benchmark_geometry`]) so
//! the GPU/CPU backend runs its REAL saturating launch geometry — not a tiny
//! under-filled slice that never fills a launch (the v0.1.15 bug under-reported a
//! 1.2 GH/s OpenCL GPU ~67x). Only fully-completed sweeps count toward the rate.
//!
//! It is deliberately **fail-safe and bounded**, because it runs on a
//! no-clawback fleet binary where the feature must NEVER prevent mining:
//!   - Wall-clock budget (`budget`, capped by the caller) checked between sweeps,
//!     plus a hard [`HARD_CAP`] timer thread that sets the backend's `stop` flag
//!     so even a single over-long full-u32 sweep is cut off mid-flight (the
//!     backend polls `stop` between its internal launches).
//!   - Any panic inside the backend is caught (`catch_unwind`) → `None`.
//!   - A nonsensical measurement (no completed sweep, no time elapsed, or a
//!     non-finite rate) → `None`.
//! On `None` the caller simply skips the suggest and mines normally.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::backend::MiningBackend;

/// A target of all-zero bytes. `sha256d(header) <= target` is true only if every
/// byte of the hash is `<= 0`, i.e. the hash is all-zero — which never happens in
/// a bounded sweep — so `hash_range` against this target always sweeps the full
/// range and returns `None`. That makes the sweep a pure rate measurement.
const NEVER_FOUND_TARGET: [u8; 32] = [0u8; 32];

/// The default startup-benchmark budget. Kept short so it adds little to startup
/// (the `--auto-tune-secs` precedent is 5s per candidate); the caller may pass a
/// smaller value but this is the fleet default.
pub const DEFAULT_BENCH_BUDGET: Duration = Duration::from_secs(3);

/// The absolute hard cap on benchmark wall time, independent of the requested
/// `budget`. Even a caller that passes a huge budget (or a backend whose single
/// chunk is pathologically slow) can never block startup longer than this. This
/// is the "honor a hard cap" guarantee the feature spec requires.
pub const HARD_CAP: Duration = Duration::from_secs(8);

/// Convert a swept-nonce count and an elapsed wall-clock duration into a
/// hashes-per-second rate, or `None` if the inputs can't yield an honest figure.
///
/// `Some(nonces / secs)` only when `secs > 0`, `nonces > 0`, and the quotient is
/// finite; otherwise `None`. This is the single rate-calc choke point for the
/// benchmark: a zero count (no work), a zero/degenerate interval (divide-by-zero),
/// or a non-finite product/quotient all collapse to `None` so the caller skips the
/// suggest and mines normally — never a bogus `0.0`, `inf`, or `NaN` on the wire.
pub fn throughput_hps(nonces: u128, elapsed: Duration) -> Option<f64> {
    if nonces == 0 {
        return None;
    }
    let secs = elapsed.as_secs_f64();
    if !(secs > 0.0) {
        return None;
    }
    let hps = (nonces as f64) / secs;
    hps.is_finite().then_some(hps)
}

/// Measure `backend`'s hashrate in **hashes per second**, or `None` if the
/// measurement is unusable (panic, no completed sweep, non-finite rate).
///
/// Mirrors [`crate::backends::cuda::benchmark_geometry`]: repeatedly sweeps the
/// FULL `[0, u32::MAX)` nonce range against an all-zero target (never satisfiable
/// ⇒ each sweep runs the whole span and returns `None`), so the OpenCL/CUDA/CPU
/// backend runs its REAL saturating launch geometry — not a tiny under-filled
/// slice. Each fully-completed sweep contributes `u32::MAX` nonces; the time
/// budget is checked after every sweep.
///
/// Bounding: a single full-u32 sweep on a slow card can exceed the budget and is
/// only interruptible between the backend's internal launches (it polls `stop`
/// there — opencl.rs / cuda.rs). So a timer thread sets a local stop once
/// [`HARD_CAP`] elapses (OR'd with the caller's `stop`), cutting an over-long
/// sweep off mid-flight. Only sweeps that completed *before* any stop fired are
/// counted; if none completed → `None` (fail-safe: skip the suggest, mine
/// normally). Never panics (the backend is wrapped in `catch_unwind`); never
/// blocks meaningfully past `HARD_CAP`.
pub fn benchmark_hashrate<B: MiningBackend + ?Sized>(
    backend: &B,
    budget: Duration,
    stop: &AtomicBool,
) -> Option<f64> {
    let budget = budget.min(HARD_CAP);
    if budget.is_zero() {
        return None;
    }

    // A fixed 84-byte header skeleton; the exact bytes don't matter for a rate
    // measurement (we never submit), only that the backend hashes them.
    let mut header = [0u8; 84];
    header[0..4].copy_from_slice(&1u32.to_le_bytes()); // a sane version field

    // The stop flag actually handed to the backend. It fires when EITHER the
    // caller's `stop` is set OR the HARD_CAP timer trips. A full-u32 sweep can no
    // longer be interrupted between fixed chunks (we sweep the whole range in one
    // call), so this flag — which `hash_range` polls between its internal launches
    // — is the only way to cut an over-long sweep off.
    let sweep_stop = Arc::new(AtomicBool::new(stop.load(Ordering::Relaxed)));

    // Timer thread: set `sweep_stop` once HARD_CAP elapses. A dedicated `done`
    // flag lets us retire the timer promptly when the sweeps finish under budget,
    // so it doesn't linger a full HARD_CAP into the process.
    let done = Arc::new(AtomicBool::new(false));
    let timer_stop = Arc::clone(&sweep_stop);
    let timer_done = Arc::clone(&done);
    let timer = thread::spawn(move || {
        let deadline = Instant::now() + HARD_CAP;
        while Instant::now() < deadline {
            if timer_done.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        timer_stop.store(true, Ordering::Relaxed);
    });

    let start = Instant::now();
    // The wall-clock instant at/after which the timer is allowed to have tripped
    // `sweep_stop` (it captured its own `Instant::now()` just before `start`, so
    // its true deadline is at or before this — using `start` here is conservative).
    // A sweep that RETURNS before this instant cannot have been cut off by the
    // timer, so it completed naturally and is safe to count even if `sweep_stop`
    // races to `true` in the post-return window. This removes the under-count of a
    // genuinely-completed final sweep whose natural end coincides with the cap.
    let hard_deadline = start + HARD_CAP;

    // Catch any panic the backend may raise so a flaky GPU init/launch can never
    // take the miner down during the benchmark — the whole point is fail-safe.
    let swept = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut nonces_completed: u128 = 0;
        loop {
            // Bail before launching another sweep if the caller asked to stop or
            // the timer already tripped. We only ever start a sweep when neither
            // stop is set, so a sweep is interruptible mid-flight ONLY by the timer
            // (which writes `sweep_stop` at/after `hard_deadline`) — never by a
            // mid-sweep caller-stop. That single-writer-mid-sweep property is what
            // makes the post-sweep "returned before deadline ⇒ completed" check
            // below race-free (no caller-stop can sneak in and turn a real
            // completion into a falsely-counted interruption).
            if stop.load(Ordering::Relaxed) || sweep_stop.load(Ordering::Relaxed) {
                break;
            }
            // One FULL sweep of the whole nonce space — the all-zero target means
            // no early "found" exit, so the backend runs its real geometry over
            // [0, u32::MAX). Returns None at the end (or early if `sweep_stop`
            // fired mid-sweep).
            let _ = backend.hash_range(header, NEVER_FOUND_TARGET, 0, u32::MAX, &sweep_stop);
            let returned_at = Instant::now();
            // Decide whether this sweep COMPLETED or was cut off. It completed
            // naturally iff it returned strictly before the hard deadline — at that
            // point neither the caller-stop (checked at the top each iteration) nor
            // the timer (which can only fire at/after `hard_deadline`) had cut it
            // short. Counting on this proven-completed condition is race-free: a
            // `sweep_stop` that flips to `true` in the post-return window cannot
            // retroactively make a before-deadline return an interruption. If the
            // sweep instead reached the deadline, it was (or may have been) cut off
            // mid-flight ⇒ do NOT count it as a full 2^32-nonce sweep, and stop.
            if returned_at >= hard_deadline || stop.load(Ordering::Relaxed) {
                break;
            }
            nonces_completed = nonces_completed.saturating_add(u32::MAX as u128);
            // Time budget is checked AFTER each completed sweep (the natural
            // granularity now that a sweep is the unit of work).
            if start.elapsed() >= budget {
                break;
            }
        }
        nonces_completed
    }));

    // Retire the timer thread (it may still be sleeping toward HARD_CAP).
    done.store(true, Ordering::Relaxed);
    let _ = timer.join();

    let nonces = match swept {
        Ok(n) => n,
        // A panic inside the backend ⇒ fail-safe None.
        Err(_) => return None,
    };

    throughput_hps(nonces, start.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MiningResult;

    // --- throughput_hps pure-fn tests (the rate calc extracted from the
    //     benchmark: nonces / elapsed_secs, or None if unusable) ---

    #[test]
    fn throughput_hps_zero_nonces_is_none() {
        // No work swept ⇒ no measurement, never a 0.0 rate that looks real.
        assert_eq!(throughput_hps(0, Duration::from_secs(1)), None);
    }

    #[test]
    fn throughput_hps_zero_time_is_none() {
        // No time elapsed ⇒ divide-by-zero territory ⇒ None, never inf.
        assert_eq!(throughput_hps(1_000_000, Duration::ZERO), None);
    }

    #[test]
    fn throughput_hps_known_value() {
        // 1.2e9 nonces over exactly 1s ⇒ ~1.2 GH/s, the real GPU figure the old
        // 200k-chunk code under-reported ~67x.
        let hps = throughput_hps(1_200_000_000, Duration::from_secs(1))
            .expect("a real rate must be Some");
        assert!((hps - 1.2e9).abs() < 1.0, "expected ~1.2e9, got {hps}");
    }

    #[test]
    fn throughput_hps_huge_nonces_stays_finite_or_none() {
        // A multi-sweep accumulation (many * 2^32) over a normal interval must
        // still produce a finite rate (never inf/NaN onto the suggest path).
        let many_sweeps: u128 = (u32::MAX as u128) * 1000;
        let hps =
            throughput_hps(many_sweeps, Duration::from_secs(3)).expect("finite product ⇒ Some");
        assert!(
            hps.is_finite() && hps > 0.0,
            "must be finite+positive, got {hps}"
        );
    }

    // --- full-range sweep tests: the v0.1.16 fix. The benchmark must drive the
    //     backend over the WHOLE [0, u32::MAX) nonce space per sweep (so the real
    //     saturating GPU geometry runs), NOT 200k-nonce chunks (which never fill a
    //     launch ⇒ ~67x under-report on an OpenCL GPU). ---

    /// Records the widest `(start, end)` range any `hash_range` call received, and
    /// always returns `None` so the sweep "completes". The test asserts the
    /// benchmark drove it with the FULL `[0, u32::MAX)` span — which FAILS on the
    /// old 200k-chunk loop (it would only ever see `start=0, end=200_000`).
    struct FullRangeCountingBackend {
        max_start: std::sync::atomic::AtomicU32,
        max_end: std::sync::atomic::AtomicU32,
        calls: std::sync::atomic::AtomicU32,
    }
    impl FullRangeCountingBackend {
        fn new() -> Self {
            Self {
                max_start: std::sync::atomic::AtomicU32::new(0),
                max_end: std::sync::atomic::AtomicU32::new(0),
                calls: std::sync::atomic::AtomicU32::new(0),
            }
        }
    }
    impl MiningBackend for FullRangeCountingBackend {
        fn name(&self) -> &'static str {
            "full-range-counting"
        }
        fn hash_range(
            &self,
            _h: [u8; 84],
            _t: [u8; 32],
            start: u32,
            end: u32,
            _stop: &AtomicBool,
        ) -> Option<MiningResult> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.max_start.fetch_max(start, Ordering::Relaxed);
            self.max_end.fetch_max(end, Ordering::Relaxed);
            None
        }
    }

    /// Blocks inside `hash_range` until `stop` is set (mirrors a real backend
    /// polling `stop` between launches — opencl.rs:214 / cuda.rs:379 — except here
    /// a single sweep would never finish on its own). Used to prove the HARD_CAP
    /// timer thread sets the local stop and unblocks the sweep.
    struct BlockingBackend;
    impl MiningBackend for BlockingBackend {
        fn name(&self) -> &'static str {
            "blocking"
        }
        fn hash_range(
            &self,
            _h: [u8; 84],
            _t: [u8; 32],
            _s: u32,
            _e: u32,
            stop: &AtomicBool,
        ) -> Option<MiningResult> {
            // Spin until the (caller-or-timer) stop fires. A real backend would be
            // doing GPU launches between these polls; we just wait.
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            None
        }
    }

    #[test]
    fn benchmark_drives_backend_over_full_u32_range() {
        // The fix: each sweep must hand the backend start==0 && end==u32::MAX, so
        // OpenCL/CUDA run their real saturating geometry. The OLD 200k-chunk code
        // drove start=0,end=200_000 and FAILS this assert.
        let backend = FullRangeCountingBackend::new();
        let stop = AtomicBool::new(false);
        let r = benchmark_hashrate(&backend, Duration::from_millis(100), &stop);
        assert_eq!(
            backend.max_start.load(Ordering::Relaxed),
            0,
            "every sweep must start at nonce 0"
        );
        assert_eq!(
            backend.max_end.load(Ordering::Relaxed),
            u32::MAX,
            "every sweep must cover the full u32 nonce space (end == u32::MAX)"
        );
        assert!(
            backend.calls.load(Ordering::Relaxed) >= 1,
            "at least one full sweep must have run"
        );
        // An instantly-returning full sweep counts as 2^32 nonces ⇒ a finite,
        // positive rate (not None).
        assert!(
            r.map(|h| h.is_finite() && h > 0.0).unwrap_or(false),
            "a completed full sweep yields a usable rate, got {r:?}"
        );
    }

    #[test]
    fn benchmark_cap_bounds_a_single_long_sweep() {
        // A backend whose single sweep never returns on its own must still be cut
        // off: the timer thread sets the local stop at ~HARD_CAP, the backend's
        // stop-poll unblocks, and the call returns. (We pass a budget ABOVE the
        // hard cap to prove the cap — not the budget — is the bound;
        // benchmark_hashrate clamps budget to HARD_CAP internally.)
        let backend = BlockingBackend;
        let stop = AtomicBool::new(false);
        let t0 = Instant::now();
        let r = benchmark_hashrate(&backend, Duration::from_secs(60), &stop);
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= HARD_CAP.saturating_sub(Duration::from_secs(1)),
            "must run up to ~HARD_CAP before the timer cuts off; took {elapsed:?}"
        );
        assert!(
            elapsed < HARD_CAP + Duration::from_secs(3),
            "must not block much past HARD_CAP; took {elapsed:?}"
        );
        // The single sweep never COMPLETED (it was interrupted), so no full sweep
        // was counted ⇒ fail-safe None ⇒ caller mines normally.
        assert_eq!(r, None, "an interrupted-only sweep counts zero ⇒ None");
    }

    /// A backend that hashes nothing but returns instantly with `None` — it lets
    /// the benchmark spin chunks without real work, exercising the loop/clock
    /// bound. Reports a non-zero (but meaningless) rate; the test only checks it
    /// is finite and positive and that it returns within the budget.
    struct InstantNullBackend;
    impl MiningBackend for InstantNullBackend {
        fn name(&self) -> &'static str {
            "instant-null"
        }
        fn hash_range(
            &self,
            _h: [u8; 84],
            _t: [u8; 32],
            _s: u32,
            _e: u32,
            _stop: &AtomicBool,
        ) -> Option<MiningResult> {
            None
        }
    }

    /// A backend whose `hash_range` always panics — the benchmark MUST catch it
    /// and return `None`, never propagate (no-clawback fleet: a benchmark panic
    /// can't be allowed to abort the miner).
    struct PanicBackend;
    impl MiningBackend for PanicBackend {
        fn name(&self) -> &'static str {
            "panic"
        }
        fn hash_range(
            &self,
            _h: [u8; 84],
            _t: [u8; 32],
            _s: u32,
            _e: u32,
            _stop: &AtomicBool,
        ) -> Option<MiningResult> {
            panic!("backend exploded mid-benchmark");
        }
    }

    #[test]
    fn benchmark_real_cpu_backend_is_fail_safe() {
        // The real CPU backend is now driven over the FULL [0, u32::MAX) span per
        // sweep (the v0.1.16 fix — same geometry production mining uses). A single
        // 2^32-nonce CPU sweep can legitimately exceed HARD_CAP on a slow box, in
        // which case ZERO sweeps complete and the honest result is None (skip the
        // suggest, mine normally). On a fast box at least one sweep completes and
        // the result is a finite positive rate. The contract this test pins is the
        // FAIL-SAFE one that matters on a no-clawback fleet: the call NEVER panics
        // and returns either a clean finite-positive rate or None — never NaN, inf,
        // zero, or negative.
        let backend = crate::backends::cpu::CpuBackend::new(2);
        let stop = AtomicBool::new(false);
        match benchmark_hashrate(&backend, Duration::from_millis(300), &stop) {
            Some(hps) => assert!(
                hps.is_finite() && hps > 0.0,
                "a usable rate must be finite + positive, got {hps}"
            ),
            None => { /* slow box: no full sweep completed in time ⇒ skip suggest */ }
        }
    }

    #[test]
    fn benchmark_completing_backend_yields_positive_finite_rate() {
        // A backend whose full-range sweep COMPLETES (returns promptly) must yield
        // a finite positive rate — proves the happy path end to end: full sweep ⇒
        // 2^32 nonces counted ⇒ throughput_hps ⇒ Some(rate). (The real GPU path
        // behaves like this — a full sweep is ~2 launches, sub-second.)
        let backend = FullRangeCountingBackend::new();
        let stop = AtomicBool::new(false);
        let hps = benchmark_hashrate(&backend, Duration::from_millis(50), &stop)
            .expect("a completed full sweep must yield a measurable rate");
        assert!(
            hps.is_finite() && hps > 0.0,
            "rate must be finite + positive, got {hps}"
        );
    }

    #[test]
    fn benchmark_is_bounded_by_the_hard_cap() {
        // The full-range rewrite means the BUDGET can no longer interrupt a sweep
        // mid-flight (a sweep is now the atomic unit of work, checked only at its
        // boundary). The HARD_CAP timer thread is therefore the real backstop: a
        // real CPU full sweep (4.29 G nonces) on a slow box can run right up to the
        // cap, but NEVER meaningfully past it. This is the "never blocks past the
        // hard cap" guarantee the no-clawback fleet requires.
        let backend = crate::backends::cpu::CpuBackend::new(2);
        let stop = AtomicBool::new(false);
        let t0 = Instant::now();
        let _ = benchmark_hashrate(&backend, Duration::from_millis(200), &stop);
        let elapsed = t0.elapsed();
        assert!(
            elapsed < HARD_CAP + Duration::from_secs(2),
            "benchmark must stay bounded by the hard cap (+ join/jitter margin); took {elapsed:?}"
        );
    }

    #[test]
    fn benchmark_catches_panic_and_returns_none() {
        // A panicking backend must NOT crash the benchmark — it returns None and
        // the caller mines on.
        let stop = AtomicBool::new(false);
        let r = benchmark_hashrate(&PanicBackend, Duration::from_millis(100), &stop);
        assert_eq!(r, None, "a backend panic must be caught → None");
    }

    #[test]
    fn benchmark_zero_budget_is_none() {
        // A zero budget is degenerate — no measurement possible → None, never a
        // divide-by-zero or bogus rate.
        let backend = crate::backends::cpu::CpuBackend::new(1);
        let stop = AtomicBool::new(false);
        assert_eq!(benchmark_hashrate(&backend, Duration::ZERO, &stop), None);
    }

    #[test]
    fn benchmark_honors_stop_flag() {
        // A pre-set stop flag means the sweep exits immediately; with no nonces
        // swept the result is None (no measurable work). Either way it must not
        // hang and must not panic.
        let backend = InstantNullBackend;
        let stop = AtomicBool::new(true);
        let t0 = Instant::now();
        let r = benchmark_hashrate(&backend, Duration::from_secs(3), &stop);
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "stop must short-circuit"
        );
        assert_eq!(r, None, "no work swept under an immediate stop ⇒ None");
    }
}
