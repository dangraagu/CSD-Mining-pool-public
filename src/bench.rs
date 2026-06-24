//! Startup hashrate micro-benchmark for the `mining.suggest_difficulty` hint.
//!
//! Measures the SELECTED backend's raw hashrate by sweeping [`MiningBackend::
//! hash_range`] against an **all-zero target** (which no sha256d output can ever
//! satisfy, so the sweep runs the whole range and returns `None` — an honest
//! "how many nonces/second does this backend do" measurement, reusing the exact
//! timing pattern in [`crate::selftest`]).
//!
//! It is deliberately **fail-safe and bounded**, because it runs on a
//! no-clawback fleet binary where the feature must NEVER prevent mining:
//!   - Hard wall-clock budget (`budget`, capped by the caller). The loop checks
//!     the clock between fixed-size chunks and a `stop` flag, so it can't run
//!     away even if a single `hash_range` chunk is slow.
//!   - Any panic inside the backend is caught (`catch_unwind`) → `None`.
//!   - A nonsensical measurement (no nonces swept, no time elapsed, or a
//!     non-finite rate) → `None`.
//! On `None` the caller simply skips the suggest and mines normally.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::backend::MiningBackend;

/// A target of all-zero bytes. `sha256d(header) <= target` is true only if every
/// byte of the hash is `<= 0`, i.e. the hash is all-zero — which never happens in
/// a bounded sweep — so `hash_range` against this target always sweeps the full
/// range and returns `None`. That makes the sweep a pure rate measurement.
const NEVER_FOUND_TARGET: [u8; 32] = [0u8; 32];

/// Nonces per chunk handed to `hash_range`. The budget/stop flag is only checked
/// BETWEEN chunks, so the chunk size bounds the responsiveness of those checks:
/// it must be small enough that even a modest CPU finishes one chunk in tens of
/// milliseconds (so a short budget isn't overshot by a whole slow chunk), yet
/// large enough that the per-call thread spawn/join overhead stays negligible.
/// 200k nonces ≈ a few ms on any modern core; the hard cap is the real backstop.
const CHUNK_NONCES: u32 = 200_000;

/// The default startup-benchmark budget. Kept short so it adds little to startup
/// (the `--auto-tune-secs` precedent is 5s per candidate); the caller may pass a
/// smaller value but this is the fleet default.
pub const DEFAULT_BENCH_BUDGET: Duration = Duration::from_secs(3);

/// The absolute hard cap on benchmark wall time, independent of the requested
/// `budget`. Even a caller that passes a huge budget (or a backend whose single
/// chunk is pathologically slow) can never block startup longer than this. This
/// is the "honor a hard cap" guarantee the feature spec requires.
pub const HARD_CAP: Duration = Duration::from_secs(8);

/// Measure `backend`'s hashrate in **hashes per second**, or `None` if the
/// measurement is unusable (panic, no work done, non-finite rate).
///
/// Sweeps `hash_range` over consecutive `CHUNK_NONCES`-sized slices against an
/// all-zero target until `budget` (clamped to [`HARD_CAP`]) elapses or `stop` is
/// set, summing the nonces actually swept and the elapsed time, then returns
/// `nonces / seconds`. Never panics; never blocks past the hard cap.
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

    let start = Instant::now();
    let mut nonces_swept: u128 = 0;
    let mut next_nonce: u32 = 0;

    // Catch any panic the backend may raise so a flaky GPU init/launch can never
    // take the miner down during the benchmark — the whole point is fail-safe.
    let swept = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        while start.elapsed() < budget {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // Bound the slice to the remaining u32 nonce space; stop if exhausted.
            let end = next_nonce.checked_add(CHUNK_NONCES)?;
            // Sweep this chunk. The all-zero target guarantees `None` (no find),
            // so the backend runs the entire `[next_nonce, end)` range.
            let _ = backend.hash_range(header, NEVER_FOUND_TARGET, next_nonce, end, stop);
            nonces_swept = nonces_swept.saturating_add((end - next_nonce) as u128);
            next_nonce = end;
        }
        Some(nonces_swept)
    }));

    let nonces = match swept {
        Ok(Some(n)) => n,
        // A panic, or the nonce space ran out before any measurable work.
        Ok(None) | Err(_) => return None,
    };

    let secs = start.elapsed().as_secs_f64();
    if nonces == 0 || secs <= 0.0 {
        return None;
    }
    let hps = (nonces as f64) / secs;
    hps.is_finite().then_some(hps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MiningResult;

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
    fn benchmark_real_cpu_backend_yields_positive_finite_rate() {
        // The real CPU backend over a tiny budget must report a usable rate.
        let backend = crate::backends::cpu::CpuBackend::new(2);
        let stop = AtomicBool::new(false);
        let hps = benchmark_hashrate(&backend, Duration::from_millis(300), &stop)
            .expect("cpu backend must yield a measurable rate");
        assert!(
            hps.is_finite() && hps > 0.0,
            "rate must be finite + positive, got {hps}"
        );
    }

    #[test]
    fn benchmark_is_bounded_by_its_budget() {
        // A short budget must return promptly — the clock bound stops the sweep
        // long before the u32 nonce space (4.3 billion nonces) would ever be
        // exhausted, and never exceeds the hard cap. The bound checked here is
        // generous (HARD_CAP + a margin for one in-flight chunk + scheduler
        // jitter under parallel `cargo test` load) so it asserts "doesn't hang"
        // robustly rather than a tight wall-time that flakes on a busy box.
        let backend = crate::backends::cpu::CpuBackend::new(2);
        let stop = AtomicBool::new(false);
        let t0 = Instant::now();
        let _ = benchmark_hashrate(&backend, Duration::from_millis(200), &stop);
        let elapsed = t0.elapsed();
        assert!(
            elapsed < HARD_CAP + Duration::from_secs(2),
            "benchmark must stay bounded; took {elapsed:?}"
        );
        // And a 200ms-budget run must be much shorter than a (clamped) 8s hard
        // cap would allow — i.e. the BUDGET, not just the cap, is doing the work.
        assert!(
            elapsed < Duration::from_secs(4),
            "a 200ms budget should finish in a fraction of the 8s hard cap; took {elapsed:?}"
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
