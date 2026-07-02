//! CPU mining backend.
//!
//! Uses the precomputed 64-byte midstate so the inner loop only runs one
//! SHA-256 compression over the 20-byte tail (merkle_tail | time | bits | nonce)
//! followed by the outer SHA-256 over the 32-byte digest. This matches the GPU
//! kernel exactly and is also the only backend that runs out-of-the-box
//! on every platform — perfect for end-to-end smoke testing.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use crate::backend::{DeviceError, MiningBackend, MiningResult};
use crate::gpu_watchdog::Recoverable;
use crate::sha256d_cpu::{finish_sha256d_from_midstate_fast, midstate_of_first_chunk_fast};

pub struct CpuBackend {
    pub threads: usize,
}

impl CpuBackend {
    pub fn new(threads: usize) -> Self {
        Self {
            threads: threads.max(1),
        }
    }
}

#[inline]
fn hash_leq_target(hash: &[u8; 32], target: &[u8; 32]) -> bool {
    // Lexicographic big-endian compare. hash <= target.
    for i in 0..32 {
        if hash[i] < target[i] {
            return true;
        }
        if hash[i] > target[i] {
            return false;
        }
    }
    true
}

impl MiningBackend for CpuBackend {
    fn name(&self) -> &'static str {
        "cpu"
    }

    fn hash_range(
        &self,
        header_84: [u8; 84],
        target: [u8; 32],
        nonce_start: u32,
        nonce_end: u32,
        stop: &AtomicBool,
    ) -> Result<Option<MiningResult>, DeviceError> {
        if nonce_end <= nonce_start {
            return Ok(None);
        }
        let midstate = midstate_of_first_chunk_fast(&header_84);

        // The 16 fixed bytes of the tail (merkle_tail|time|bits) come from the
        // header and don't change inside the loop; nonce is appended per attempt.
        let mut tail_template = [0u8; 20];
        tail_template[..16].copy_from_slice(&header_84[64..80]);

        // Nonces are u32 on the wire, but the shared work counter is u64 so
        // that "the range is exhausted" stays representable at the top of the
        // nonce space: with a u32 counter, `fetch_add(CHUNK)` wraps past
        // `u32::MAX` back to ~0 without ever yielding a value >= nonce_end
        // (the exact terminator 0xffff_ffff is hit only when the start is
        // congruent to it mod CHUNK), so a sweep ending at the top never
        // self-terminates. In u64 the counter is monotonic and the
        // `start >= nonce_end` check fires as soon as the range is spent.
        let next_nonce = AtomicU64::new(u64::from(nonce_start));
        let nonce_end = u64::from(nonce_end);
        let found = std::sync::Arc::new(std::sync::Mutex::new(None::<MiningResult>));
        let local_stop = AtomicBool::new(false);

        thread::scope(|scope| {
            for _ in 0..self.threads {
                let midstate = midstate;
                let tail_template = tail_template;
                let target = target;
                let next_nonce = &next_nonce;
                let found = found.clone();
                let local_stop = &local_stop;

                scope.spawn(move || {
                    let mut tail = tail_template;
                    loop {
                        if stop.load(Ordering::Relaxed) || local_stop.load(Ordering::Relaxed) {
                            return;
                        }
                        // Grab a small chunk of nonces so threads don't
                        // hammer the atomic.
                        const CHUNK: u64 = 4096;
                        let start = next_nonce.fetch_add(CHUNK, Ordering::Relaxed);
                        if start >= nonce_end {
                            return;
                        }
                        let end = start.saturating_add(CHUNK).min(nonce_end);
                        for n in start..end {
                            // end <= nonce_end <= u32::MAX, so this never
                            // truncates.
                            let n = n as u32;
                            tail[16..20].copy_from_slice(&n.to_le_bytes());
                            let h = finish_sha256d_from_midstate_fast(&midstate, &tail);
                            if hash_leq_target(&h, &target) {
                                let mut g = found.lock().unwrap();
                                if g.is_none() {
                                    *g = Some(MiningResult { nonce: n, hash: h });
                                }
                                local_stop.store(true, Ordering::Relaxed);
                                return;
                            }
                        }
                    }
                });
            }
        });

        // The CPU backend has no device to fault: every sweep is clean, so
        // this is always `Ok` (the `DeviceError` arm exists only for the GPU
        // backends — type plumbing, no CPU behaviour change).
        let g = found.lock().unwrap();
        Ok(*g)
    }
}

/// The CPU backend can't meaningfully "recover" a wedge in-process (it has no
/// driver state to rebuild — a thread pool that produced zero-with-work would
/// need a full process restart). It uses the default no-op `recover()` (returns
/// `false`), so a confirmed CPU stall escalates straight to a supervisor restart
/// — the only lever that helps here. In practice the GPU watchdog runs only when
/// a GPU backend is active, so this impl exists for trait-completeness/uniform
/// dispatch, not because a CPU rig is expected to trip it.
impl Recoverable for CpuBackend {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// No sha256d output can ever be `<=` an all-zero target (that would
    /// require a 32-zero-byte digest, i.e. a SHA-256 preimage of zero), so a
    /// sweep against it can only end by exhausting its nonce range. Same
    /// trick as `bench::NEVER_FOUND_TARGET`.
    const NEVER_FOUND_TARGET: [u8; 32] = [0u8; 32];

    /// Characterization (IMP-2): a no-match sweep whose end sits at the TOP of
    /// the u32 nonce space must exhaust its range, return `None`, and
    /// TERMINATE.
    ///
    /// The pre-fix code kept the shared work counter in an `AtomicU32`: from a
    /// start near the top, `fetch_add(CHUNK)` wraps past `u32::MAX` back to ~0
    /// without ever yielding a value `>= nonce_end` (here `0xffff_f000 + 4096`
    /// wraps to exactly 0, and every later value is `≡ 0 mod 4096`, so the
    /// exact terminator `0xffff_ffff` is never reached) — the "range
    /// exhausted" check never fires and the sweep spins over the whole 2^32
    /// space forever. Latent-only today: production sweeps are bounded and the
    /// bench has a hard timer.
    ///
    /// The test is bounded so the OLD code FAILS FAST instead of hanging the
    /// suite: the sweep runs on its own thread against a deadline; on timeout
    /// we set `stop` (releasing the spin) and fail. Fixed code sweeps the
    /// 4095-nonce range in well under a millisecond.
    #[test]
    fn sweep_to_top_of_nonce_space_self_terminates_with_none() {
        let backend = CpuBackend::new(2);
        let stop = Arc::new(AtomicBool::new(false));
        let header = [0u8; 84];

        let stop_for_sweep = stop.clone();
        let handle = thread::spawn(move || {
            backend.hash_range(
                header,
                NEVER_FOUND_TARGET,
                0xffff_f000,
                u32::MAX,
                &stop_for_sweep,
            )
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let finished_on_its_own = handle.is_finished();
        // Release the (old code's) spin either way so join() always returns
        // and the suite never wedges on this test.
        stop.store(true, Ordering::Relaxed);
        let result = handle.join().expect("sweep thread must not panic");

        assert!(
            finished_on_its_own,
            "hash_range must self-terminate once [0xffff_f000, 0xffff_ffff) is \
             exhausted; still sweeping after 5s — the u32 work counter wrapped \
             past nonce_end instead of exhausting"
        );
        assert!(
            matches!(result, Ok(None)),
            "no nonce can satisfy an all-zero target; a clean exhausted sweep \
             returns Ok(None)"
        );
    }

    /// Characterization: the FOUND path for high-bit nonces near the top of
    /// the range. Compute every hash in `[start, end)` with the same
    /// primitives the backend uses, set the target to the unique minimum, and
    /// require `hash_range` to return exactly that nonce + hash. Pins the
    /// little-endian u32 nonce serialization and the found-nonce result across
    /// the counter-width fix (must be green BEFORE and AFTER — the sweep is
    /// byte-identical for non-exhausting ranges).
    #[test]
    fn found_nonce_near_top_of_range_is_exact() {
        let mut header = [0u8; 84];
        for (i, b) in header.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        let start: u32 = 0xffff_fe00;
        let end: u32 = 0xffff_ff00;

        let midstate = midstate_of_first_chunk_fast(&header);
        let mut tail = [0u8; 20];
        tail[..16].copy_from_slice(&header[64..80]);
        let (best_nonce, best_hash) = (start..end)
            .map(|n| {
                tail[16..20].copy_from_slice(&n.to_le_bytes());
                (n, finish_sha256d_from_midstate_fast(&midstate, &tail))
            })
            .min_by(|a, b| a.1.cmp(&b.1))
            .expect("range is non-empty");

        // Single-threaded so the sweep order is deterministic (one ascending
        // chunk covers the whole 256-nonce range).
        let backend = CpuBackend::new(1);
        let stop = AtomicBool::new(false);
        let res = backend
            .hash_range(header, best_hash, start, end, &stop)
            .expect("a CPU sweep never faults")
            .expect("the minimum-hash nonce satisfies hash <= target");
        assert_eq!(res.nonce, best_nonce, "must find the argmin nonce");
        assert_eq!(res.hash, best_hash, "returned hash must be its sha256d");
    }
}
