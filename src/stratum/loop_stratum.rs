//! Pooled Stratum-v1 mining loop.
//!
//! Polls the Stratum client for the latest job, maps `mining.notify` into a
//! csd1 work template, and races the GPU against a CPU worker pool over the
//! nonce range, gating every find through a CPU re-hash before submit. In a
//! pool, **the server owns canonicity**, and the coinbase extranonce is split
//! `xn1(4) ‖ xn2(4)`: the low half (xn1) is pool-fixed at subscribe time and
//! only the high half (xn2) rolls.
//!
//! Per iteration:
//!   1. `client.latest_job()` (poll). `None` ⇒ no notify yet ⇒ brief sleep +
//!      retry.
//!   2. share target = [`target_from_difficulty`]`(client.current_difficulty())`.
//!   3. map notify → [`crate::csd_consensus::WorkTemplate`] via
//!      [`crate::stratum::mapping::notify_to_template`].
//!   4. roll **xn2** (high 32 bits) per kernel launch; compose the full
//!      extranonce as [`compose_extranonce`]`(xn1_low, xn2)`.
//!   5. on FOUND `(xn2, nonce)`: build the submit field trio with
//!      [`build_submit`]`(xn2, template.time, nonce)` and send it via
//!      [`StratumClient::send_submit`].

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::backend::{MiningBackend, MiningResult};
use crate::coinbase::{coinbase_txid, header_84, merkle_root_from_branch};
use crate::mining_config::{partition_nonce_range, MiningConfig};
use crate::sha256d_cpu::{finish_sha256d_from_midstate_fast, midstate_of_first_chunk_fast};
use crate::stratum::client::StratumClient;
use crate::stratum::mapping::{build_submit, compose_extranonce, notify_to_template};

/// Pool-difficulty-1 target as 32 big-endian bytes:
/// `0x00000000FFFF0000000000000000000000000000000000000000000000000000`.
///
/// This is the standard Stratum "pdiff-1" target, identical to the bridge's
/// `pdiff_1_target()` — bytes [4] and [5] are 0xFF, everything else 0x00. A
/// share at difficulty `d` has target `pdiff_1 / d`.
const PDIFF1_BE: [u8; 32] = {
    let mut t = [0u8; 32];
    t[4] = 0xff;
    t[5] = 0xff;
    t
};

/// Convert a Stratum share difficulty into a 32-byte **big-endian** target,
/// `target = pdiff_1 / round(d)`, matching the bridge's `target_from_difficulty`
/// exactly (it rounds `d` to an integer and divides the pdiff-1 target).
///
/// The output byte order is big-endian (most-significant byte at index 0) so it
/// drops straight into the miner's [`hash_leq_target`] comparator and the
/// backends, which compare the raw sha256d output lexicographically with byte 0
/// as most-significant — the same numeric comparison the bridge performs in
/// `meets_target` (it reverses the LE hash to BE before the BigUint compare).
///
/// `d <= 0.0` clamps to 1 (a defensive belt-and-braces mirror of the bridge:
/// the vardiff loop enforces `MIN_DIFFICULTY = 1.0`, but a stray 0 here would
/// otherwise make `pdiff_1 / 0` undefined). Implemented with pure u256-by-u64
/// long division so the public crate needs no bignum dependency.
pub fn target_from_difficulty(d: f64) -> [u8; 32] {
    let divisor: u64 = if d <= 0.0 {
        1
    } else {
        // Round to nearest integer, floored at 1 — identical to the bridge.
        d.round().max(1.0) as u64
    };
    if divisor <= 1 {
        return PDIFF1_BE;
    }
    u256_div_u64_be(&PDIFF1_BE, divisor)
}

/// Big-endian 256-bit / 64-bit long division. `dividend` is 32 big-endian
/// bytes; returns the 32-big-endian-byte quotient (remainder discarded — share
/// targets only need the floor, exactly as integer `BigUint` division gives).
///
/// Schoolbook base-2^8 long division: walk the dividend most-significant byte
/// first, carrying the running remainder in a u128 (wide enough that
/// `rem * 256 + byte` can never overflow for a u64 divisor).
fn u256_div_u64_be(dividend: &[u8; 32], divisor: u64) -> [u8; 32] {
    debug_assert!(divisor >= 1);
    let mut quotient = [0u8; 32];
    let mut rem: u128 = 0;
    let div = divisor as u128;
    for i in 0..32 {
        let acc = (rem << 8) | (dividend[i] as u128);
        quotient[i] = (acc / div) as u8;
        rem = acc % div;
    }
    quotient
}

/// Lexicographic big-endian compare: `hash <= target`. Byte 0 is the most
/// significant. Identical to the comparator in `loop_.rs` / `backends/cpu.rs`
/// (kept as a private copy so this module doesn't reach into the node loop's
/// internals — the function there is not `pub`).
#[inline]
fn hash_leq_target(hash: &[u8; 32], target: &[u8; 32]) -> bool {
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

/// A CPU worker's find. Mirrors `loop_::CpuFind` (private there); duplicated so
/// the Stratum loop's worker pool is self-contained.
#[derive(Clone, Copy, Debug)]
struct CpuFind {
    thread_idx: usize,
    nonce: u32,
    hash: [u8; 32],
}

/// Run the pooled Stratum mining loop until `stop` is set.
///
/// `client` must already be connected (handshake done, reader thread running).
/// Work is pulled from its background-updated `latest_job()` / difficulty; found
/// shares are submitted via `client.send_submit`. The CPU+GPU split honours the
/// `MiningConfig` knobs `cpu_threads` and `cpu_share`.
pub fn run_stratum<B: MiningBackend>(
    backend: &B,
    client: &StratumClient,
    stop: Arc<AtomicBool>,
    cfg: MiningConfig,
) -> Result<()> {
    // Re-derive fresh work this often even if no new notify arrived, so a
    // long-lived job picks up difficulty changes and ntime drift promptly.
    let refresh_every = Duration::from_secs(2);

    // Hashrate tracking (mirrors the node loop's 10s cadence).
    let mut last_hashrate_log = Instant::now();
    let mut gpu_nonces_since_log: u128 = 0;
    let mut cpu_nonces_since_log: u128 = 0;

    // Rate-limit the "waiting for first job" notice so a slow pool start
    // doesn't spam the log.
    let mut last_wait_log = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .unwrap_or_else(Instant::now);

    // The miner-rolled high half of the coinbase extranonce. Bumped once per
    // exhausted launch. The low half (xn1) is pool-fixed and NEVER rolled here.
    let mut xn2: u32 = 0;

    if cfg.cpu_threads > 0 && cfg.cpu_share > 0.0 {
        tracing::info!(
            "stratum: cpu mining enabled (threads={} share={:.2}); racing GPU per launch",
            cfg.cpu_threads, cfg.cpu_share,
        );
    } else {
        tracing::info!(
            "stratum: cpu mining disabled (cpu_threads={} cpu_share={:.2}); GPU-only",
            cfg.cpu_threads, cfg.cpu_share,
        );
    }

    while !stop.load(Ordering::Relaxed) {
        // --- work intake: poll the pool's latest pushed job ---
        let job = match client.latest_job() {
            Some(j) => j,
            None => {
                // No notify yet (just connected, or mid-reconnect). This is the
                // Stratum analogue of WorkOutcome::Hold — but there is no
                // last-good template to mine through on, so we idle briefly.
                if last_wait_log.elapsed() >= Duration::from_secs(10) {
                    tracing::info!("stratum: waiting for first mining.notify from pool…");
                    last_wait_log = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
        };

        // Share target from the current pool difficulty.
        let difficulty = client.current_difficulty();
        let share_target = target_from_difficulty(difficulty);

        // Map notify → WorkTemplate (+ job_id + xn1_low). A malformed notify
        // (bad hex, wrong extranonce1 width, etc.) is logged and we retry —
        // never panic the loop on pool-side garbage.
        let xn1_bytes = match hex::decode(&job.extranonce1_hex) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    "stratum: extranonce1 {:?} is not valid hex ({e}); waiting for next job",
                    job.extranonce1_hex
                );
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
        };
        let mapped = match notify_to_template(&job.notify, &xn1_bytes, share_target) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("stratum: cannot map job {}: {e}; waiting for next job", job.notify.job_id);
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
        };
        let template = &mapped.template;
        let branch: Vec<[u8; 32]> = template.merkle_branch.iter().map(|b| b.0).collect();

        tracing::info!(
            "stratum got_job id={} height(n/a) prev=0x{} diff={:.4} share_target=0x{}…",
            mapped.job_id,
            hex::encode(template.prev),
            difficulty,
            &hex::encode(share_target)[..16],
        );

        let last_refresh = Instant::now();

        // Inner per-launch loop for THIS job. Re-derives the coinbase/header
        // each launch from the rolled xn2, races the backends, submits on find.
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if last_refresh.elapsed() > refresh_every {
                break; // re-poll latest_job (may be the same; may be newer)
            }
            // If the pool pushed a new job, abandon this one immediately so we
            // never mine stale work past a clean_jobs boundary.
            if let Some(j) = client.latest_job() {
                if j.notify.job_id != mapped.job_id {
                    break;
                }
            }

            // Compose the full 8-byte extranonce: low = pool xn1, high = our xn2.
            let extranonce = compose_extranonce(mapped.xn1_low, xn2);

            // Coinbase txid + merkle root for this extranonce, then the header
            // skeleton (nonce overwritten by the backend per attempt).
            let cb_txid = coinbase_txid(
                &template.coinbase_prefix,
                extranonce,
                &template.coinbase_suffix,
            );
            let merkle = merkle_root_from_branch(cb_txid, &branch, 0);
            let hdr = header_84(
                template.version,
                &template.prev,
                &merkle,
                template.time,
                template.bits,
                0,
            );

            // Partition the nonce range between GPU + CPU pool (same helper the
            // node loop uses).
            let (gpu_range, cpu_ranges) = partition_nonce_range(
                template.nonce_start,
                template.nonce_end,
                cfg.cpu_share,
                cfg.cpu_threads,
            );

            // Shared cancellation + winner slot for this launch.
            let iter_stop = Arc::new(AtomicBool::new(false));
            let cpu_winner: Arc<Mutex<Option<CpuFind>>> = Arc::new(Mutex::new(None));
            let found_for_template_id = Arc::new(AtomicU64::new(0));
            let template_id = template.id;
            let cpu_swept = Arc::new(AtomicU64::new(0));
            let gpu_stop = Arc::new(AtomicBool::new(stop.load(Ordering::Relaxed)));

            let gpu_result: Mutex<Option<MiningResult>> = Mutex::new(None);
            let gpu_result_ref = &gpu_result;

            let midstate = midstate_of_first_chunk_fast(&hdr);
            let mut tail_template = [0u8; 20];
            tail_template[..16].copy_from_slice(&hdr[64..80]);
            let target = template.target;

            thread::scope(|scope| {
                // CPU workers: one per cpu_ranges entry.
                for (thread_idx, (cstart, cend)) in cpu_ranges.iter().copied().enumerate() {
                    let iter_stop_ = iter_stop.clone();
                    let cpu_winner_ = cpu_winner.clone();
                    let found_for_template_id_ = found_for_template_id.clone();
                    let cpu_swept_ = cpu_swept.clone();
                    let stop_ = stop.clone();
                    let midstate = midstate;
                    let tail_template = tail_template;
                    let target = target;
                    scope.spawn(move || {
                        if cend <= cstart {
                            return;
                        }
                        let mut tail = tail_template;
                        let mut local_swept: u64 = 0;
                        for (i, n) in (cstart..cend).enumerate() {
                            if i & 0xff == 0 {
                                if stop_.load(Ordering::Relaxed)
                                    || iter_stop_.load(Ordering::Relaxed)
                                {
                                    break;
                                }
                                if found_for_template_id_.load(Ordering::Acquire) == template_id {
                                    break;
                                }
                            }
                            tail[16..20].copy_from_slice(&n.to_le_bytes());
                            let h = finish_sha256d_from_midstate_fast(&midstate, &tail);
                            local_swept += 1;
                            if hash_leq_target(&h, &target) {
                                let mut g = cpu_winner_.lock().unwrap();
                                if g.is_none() {
                                    *g = Some(CpuFind {
                                        thread_idx,
                                        nonce: n,
                                        hash: h,
                                    });
                                    found_for_template_id_.store(template_id, Ordering::Release);
                                    iter_stop_.store(true, Ordering::Release);
                                }
                                break;
                            }
                        }
                        cpu_swept_.fetch_add(local_swept, Ordering::Relaxed);
                    });
                }

                // Bridge poller: forwards stop || iter_stop into gpu_stop so the
                // GPU backend wakes up on a CPU win.
                let stop_b = stop.clone();
                let iter_stop_b = iter_stop.clone();
                let gpu_stop_b = gpu_stop.clone();
                scope.spawn(move || loop {
                    let s = stop_b.load(Ordering::Relaxed) || iter_stop_b.load(Ordering::Relaxed);
                    gpu_stop_b.store(s, Ordering::Relaxed);
                    if s {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                });

                // GPU sweep on its assigned sub-range (main scope thread).
                let (gstart, gend) = gpu_range;
                let res = if gend > gstart {
                    backend.hash_range(hdr, target, gstart, gend, &gpu_stop)
                } else {
                    None
                };
                *gpu_result_ref.lock().unwrap() = res;
                iter_stop.store(true, Ordering::Release);
            });

            let gpu_found = gpu_result.into_inner().unwrap();
            let cpu_found = cpu_winner.lock().unwrap().clone();
            let cpu_swept_n = cpu_swept.load(Ordering::Relaxed) as u128;
            let gpu_swept = (gpu_range.1 as u128).saturating_sub(gpu_range.0 as u128);
            gpu_nonces_since_log = gpu_nonces_since_log.saturating_add(gpu_swept);
            cpu_nonces_since_log = cpu_nonces_since_log.saturating_add(cpu_swept_n);

            enum WinSource {
                Gpu(MiningResult),
                Cpu(CpuFind),
            }
            let win: Option<WinSource> = match (gpu_found, cpu_found) {
                (Some(g), _) => Some(WinSource::Gpu(g)),
                (None, Some(c)) => Some(WinSource::Cpu(c)),
                (None, None) => None,
            };

            match win {
                Some(src) => {
                    let (device, thread_label, nonce, claimed_hash) = match src {
                        WinSource::Gpu(mr) => ("gpu", None, mr.nonce, mr.hash),
                        WinSource::Cpu(cf) => ("cpu", Some(cf.thread_idx), cf.nonce, cf.hash),
                    };

                    // CORRECTNESS GATE: re-hash on CPU before submitting. Catches
                    // any kernel bug / driver miscompile. (Also runs for CPU
                    // wins — cheap + uniform.)
                    let mut hdr_check = hdr;
                    hdr_check[80..84].copy_from_slice(&nonce.to_le_bytes());
                    let cpu_hash = crate::sha256d_cpu::sha256d(&hdr_check);
                    if cpu_hash != claimed_hash {
                        tracing::error!(
                            "stratum device={device} HASH MISMATCH job={} xn2={xn2} nonce={nonce}: claimed=0x{} cpu=0x{} - skipping",
                            mapped.job_id,
                            hex::encode(claimed_hash),
                            hex::encode(cpu_hash),
                        );
                        xn2 = xn2.wrapping_add(1);
                        continue;
                    }
                    if !hash_leq_target(&cpu_hash, &share_target) {
                        tracing::error!(
                            "stratum device={device} hash ABOVE share target job={} nonce={nonce}: hash=0x{} target=0x{} - skipping",
                            mapped.job_id,
                            hex::encode(cpu_hash),
                            hex::encode(share_target),
                        );
                        xn2 = xn2.wrapping_add(1);
                        continue;
                    }

                    match thread_label {
                        Some(t) => tracing::info!(
                            "stratum SHARE device={device} thread={t} job={} xn2={xn2} nonce={nonce} hash=0x{}",
                            mapped.job_id, hex::encode(cpu_hash),
                        ),
                        None => tracing::info!(
                            "stratum SHARE device={device} job={} xn2={xn2} nonce={nonce} hash=0x{}",
                            mapped.job_id, hex::encode(cpu_hash),
                        ),
                    }

                    // Build the submit field trio and send it. xn2 is the high
                    // half we rolled; time/nonce produced the winning hash.
                    let fields = build_submit(xn2, template.time, nonce);
                    let submit_start = Instant::now();
                    match client.send_submit(
                        client.worker_addr(),
                        &mapped.job_id,
                        &fields.extranonce2_hex,
                        &fields.ntime_hex,
                        &fields.nonce_hex,
                    ) {
                        Ok(()) => tracing::info!(
                            "stratum submit OK job={} xn2_hex={} ntime={} nonce={} latency_ms={}",
                            mapped.job_id,
                            fields.extranonce2_hex,
                            fields.ntime_hex,
                            fields.nonce_hex,
                            submit_start.elapsed().as_millis(),
                        ),
                        Err(e) => tracing::warn!(
                            "stratum submit FAILED job={} xn2_hex={}: {e}",
                            mapped.job_id, fields.extranonce2_hex,
                        ),
                    }

                    // Roll xn2 for the next launch and re-derive work (next job
                    // poll happens at the top of the inner loop).
                    xn2 = xn2.wrapping_add(1);
                }
                None => {
                    // Exhausted this launch's nonce range with no share. Roll
                    // xn2 and try the next slice of coinbase space.
                    if last_hashrate_log.elapsed() >= Duration::from_secs(10) {
                        let elapsed = last_hashrate_log.elapsed().as_secs_f64();
                        let ghs_gpu = (gpu_nonces_since_log as f64) / 1e9 / elapsed;
                        let mhs_cpu = (cpu_nonces_since_log as f64) / 1e6 / elapsed;
                        let combined_ghs = ghs_gpu + (mhs_cpu / 1000.0);
                        tracing::info!(
                            "stratum hashrate gpu={:.2} GH/s cpu={:.2} MH/s combined={:.2} GH/s (job={}, diff={:.2})",
                            ghs_gpu, mhs_cpu, combined_ghs, mapped.job_id, difficulty,
                        );
                        last_hashrate_log = Instant::now();
                        gpu_nonces_since_log = 0;
                        cpu_nonces_since_log = 0;
                    }
                    xn2 = xn2.wrapping_add(1);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MiningBackend;
    use crate::backends::cpu::CpuBackend;
    use crate::stratum::protocol::NotifyParams;

    // --- target_from_difficulty tests ---

    #[test]
    fn pdiff1_constant_is_standard_share_target() {
        // 0x00000000FFFF0000…0000 — bytes [4],[5] are 0xFF, rest 0x00.
        assert_eq!(PDIFF1_BE[4], 0xff);
        assert_eq!(PDIFF1_BE[5], 0xff);
        for (i, b) in PDIFF1_BE.iter().enumerate() {
            if i != 4 && i != 5 {
                assert_eq!(*b, 0x00, "byte {i} must be zero");
            }
        }
    }

    #[test]
    fn difficulty_one_is_pdiff1() {
        assert_eq!(target_from_difficulty(1.0), PDIFF1_BE);
    }

    #[test]
    fn difficulty_zero_and_negative_clamp_to_one() {
        // Defensive clamp: d<=0 → divisor 1 → pdiff-1 (never divide by zero).
        assert_eq!(target_from_difficulty(0.0), PDIFF1_BE);
        assert_eq!(target_from_difficulty(-5.0), PDIFF1_BE);
    }

    #[test]
    fn difficulty_two_halves_the_target() {
        // pdiff_1 / 2: the 0xFFFF at [4..6] becomes 0x7FFF8000 spilling into
        // [4..8] (0xFFFF0000… >> 1 = 0x7FFF8000…). Verify against a hand u256
        // divide so we pin the exact bytes, not just "smaller".
        let half = target_from_difficulty(2.0);
        let expect = u256_div_u64_be(&PDIFF1_BE, 2);
        assert_eq!(half, expect);
        // Sanity: half target must be < pdiff-1 (numerically), i.e. lexically
        // less-or-equal and not equal.
        assert!(hash_leq_target(&half, &PDIFF1_BE));
        assert_ne!(half, PDIFF1_BE);
        // And specifically 0xFFFF0000 >> 1 = 0x7FFF8000 lands at [4..8].
        assert_eq!(half[4], 0x7f);
        assert_eq!(half[5], 0xff);
        assert_eq!(half[6], 0x80);
        assert_eq!(half[7], 0x00);
    }

    #[test]
    fn difficulty_rounds_to_nearest_int_like_bridge() {
        // The bridge rounds d before dividing; 1.4 → 1, 1.6 → 2.
        assert_eq!(target_from_difficulty(1.4), target_from_difficulty(1.0));
        assert_eq!(target_from_difficulty(1.6), target_from_difficulty(2.0));
    }

    #[test]
    fn higher_difficulty_yields_smaller_target() {
        // Monotonic: bigger difficulty ⇒ numerically smaller (harder) target.
        let d1 = target_from_difficulty(1.0);
        let d16 = target_from_difficulty(16.0);
        let d256 = target_from_difficulty(256.0);
        assert!(hash_leq_target(&d16, &d1) && d16 != d1);
        assert!(hash_leq_target(&d256, &d16) && d256 != d16);
    }

    #[test]
    fn u256_div_matches_simple_known_values() {
        // 0xFFFF0000…(at [4..6]) / 0xFFFF == 0x00010000…? No: pdiff_1 / 0xFFFF.
        // Easier known value: divide a target with a single 0x02 at [31] by 2.
        let mut two = [0u8; 32];
        two[31] = 0x02;
        let one = u256_div_u64_be(&two, 2);
        let mut expect_one = [0u8; 32];
        expect_one[31] = 0x01;
        assert_eq!(one, expect_one);
        // Divide by a larger-than-value divisor → floor 0.
        let zero = u256_div_u64_be(&two, 5);
        assert_eq!(zero, [0u8; 32]);
    }

    // --- xn2-only rolling: composing the extranonce keeps xn1 fixed ---

    #[test]
    fn rolling_xn2_keeps_xn1_low_fixed() {
        let xn1_low: u32 = 0xddccbbaa; // arbitrary pool-fixed low half
        for xn2 in [0u32, 1, 2, 0xdead_beef, u32::MAX] {
            let e = compose_extranonce(xn1_low, xn2);
            let le = e.to_le_bytes();
            // Low 4 LE bytes are always xn1 (never change as xn2 rolls).
            assert_eq!(&le[0..4], &xn1_low.to_le_bytes());
            // High 4 LE bytes track xn2.
            assert_eq!(&le[4..8], &xn2.to_le_bytes());
        }
    }

    // --- end-to-end-ish: the loop finds a share against an easy target ---

    /// A trivial in-memory backend that always returns "no GPU find" so the
    /// CPU worker pool is what discovers shares. Keeps the test deterministic
    /// (no GPU dependency) while still exercising the real CPU race path.
    struct NullGpu;
    impl MiningBackend for NullGpu {
        fn name(&self) -> &'static str {
            "null-gpu"
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

    fn fixture_notify() -> NotifyParams {
        let prev_be: String = (0u8..32).map(|i| format!("{:02x}", i)).collect();
        NotifyParams {
            job_id: "job-test".to_string(),
            prev_hash_be_hex: prev_be,
            coinb1_hex: "01000000aabbcc".to_string(),
            coinb2_hex: "ffeeddccbbaa99".to_string(),
            merkle_branches_hex: vec![],
            version_hex: "20000000".to_string(),
            nbits_hex: "1d00ffff".to_string(),
            ntime_hex: "665544cc".to_string(),
            clean_jobs: true,
        }
    }

    /// Drive the inner FOUND logic directly: with an all-0xff share target
    /// (every hash qualifies) and the CPU pool over a tiny nonce range, the
    /// loop must produce a share whose submit fields equal what `build_submit`
    /// yields for the winning (xn2, nonce).
    ///
    /// We can't run `run_stratum` itself without a live `StratumClient` socket,
    /// so this test reproduces the loop's per-launch hashing + submit-field
    /// construction with the SAME helpers the loop calls, asserting the wiring
    /// (mapping → compose_extranonce → header → CPU find → build_submit) is
    /// internally consistent and matches the mapping module's contract.
    #[test]
    fn cpu_finds_share_and_submit_fields_match_build_submit() {
        let notify = fixture_notify();
        let xn1 = [0xaa, 0xbb, 0xcc, 0xdd];
        // Easy target: all 0xff ⇒ hash_leq_target is always true ⇒ first nonce
        // in the CPU range is an immediate "share".
        let easy_target = [0xffu8; 32];
        let mapped = notify_to_template(&notify, &xn1, easy_target).unwrap();
        let template = &mapped.template;
        let branch: Vec<[u8; 32]> = template.merkle_branch.iter().map(|b| b.0).collect();

        let xn2: u32 = 7;
        let extranonce = compose_extranonce(mapped.xn1_low, xn2);
        let cb = coinbase_txid(&template.coinbase_prefix, extranonce, &template.coinbase_suffix);
        let merkle = merkle_root_from_branch(cb, &branch, 0);
        let hdr = header_84(
            template.version,
            &template.prev,
            &merkle,
            template.time,
            template.bits,
            0,
        );

        // CPU sweep a tiny range; with the easy target the first nonce wins.
        let midstate = midstate_of_first_chunk_fast(&hdr);
        let mut tail = [0u8; 20];
        tail[..16].copy_from_slice(&hdr[64..80]);
        let nonce_start = 0u32;
        let mut found: Option<(u32, [u8; 32])> = None;
        for n in nonce_start..(nonce_start + 64) {
            tail[16..20].copy_from_slice(&n.to_le_bytes());
            let h = finish_sha256d_from_midstate_fast(&midstate, &tail);
            if hash_leq_target(&h, &easy_target) {
                found = Some((n, h));
                break;
            }
        }
        let (nonce, hash) = found.expect("easy target must yield a share immediately");
        assert_eq!(nonce, nonce_start, "first nonce qualifies under all-0xff target");

        // Correctness gate: full-header re-hash must agree with the midstate path.
        let mut hdr_check = hdr;
        hdr_check[80..84].copy_from_slice(&nonce.to_le_bytes());
        assert_eq!(crate::sha256d_cpu::sha256d(&hdr_check), hash);

        // The submit fields the loop would send.
        let fields = build_submit(xn2, template.time, nonce);
        // extranonce2 is the rolled xn2 as 4 LE bytes (NOT the full 8-byte
        // extranonce — only the high half travels in the submit).
        assert_eq!(fields.extranonce2_hex, hex::encode(xn2.to_le_bytes()));
        assert_eq!(fields.ntime_hex, format!("{:08x}", template.time as u32));
        assert_eq!(fields.nonce_hex, format!("{:08x}", nonce));

        // And the null GPU contributes nothing (CPU is the only finder here).
        let g = NullGpu;
        assert!(g
            .hash_range(hdr, easy_target, 0, 64, &AtomicBool::new(false))
            .is_none());
    }

    // --- live wiring: run_stratum against a fake bridge over a real socket ---

    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// Drive the REAL `run_stratum` against a localhost listener that plays the
    /// bridge (handshake + one set_difficulty + one notify), with the CPU
    /// backend. We can't force a real share in a fast test (a pdiff-1 share is
    /// genuine PoW), so this asserts the live path up to and including hashing:
    /// connect → `latest_job()` → `notify_to_template` → backend dispatch →
    /// clean shutdown on `stop`. The exact submit-field correctness is pinned
    /// separately by `cpu_finds_share_and_submit_fields_match_build_submit`.
    ///
    /// The bridge records any bytes the client sends after the handshake; we
    /// assert the loop did NOT emit a malformed/early submit (no `mining.submit`
    /// is expected because no share clears pdiff-1 in the brief run window).
    #[test]
    fn run_stratum_connects_maps_and_shuts_down_cleanly() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut br = BufReader::new(sock.try_clone().unwrap());

            // Handshake: read subscribe (id=1) + authorize (id=2).
            let mut req = String::new();
            br.read_line(&mut req).unwrap();
            req.clear();
            br.read_line(&mut req).unwrap();

            // Reply: subscribe (xn1="aabbccdd", xn2_size=4), authorize true,
            // then a difficulty and a real-looking notify.
            sock.write_all(
                b"{\"id\":1,\"result\":[[[\"mining.notify\",\"1\"]],\"aabbccdd\",4],\"error\":null}\n",
            )
            .unwrap();
            sock.write_all(b"{\"id\":2,\"result\":true,\"error\":null}\n").unwrap();
            sock.write_all(
                b"{\"id\":null,\"method\":\"mining.set_difficulty\",\"params\":[1024.0]}\n",
            )
            .unwrap();
            sock.write_all(
                b"{\"id\":null,\"method\":\"mining.notify\",\"params\":[\"jobZ\",\"00000000000000000000000000000000000000000000000000000000000000ff\",\"01000000\",\"00000000\",[],\"20000000\",\"1d00ffff\",\"60c0babe\",true]}\n",
            )
            .unwrap();
            sock.flush().unwrap();

            // Collect anything the client sends back during the run window. With
            // a 1024-difficulty (hard) target and a sub-second window, the loop
            // should NOT emit a `mining.submit`.
            sock.set_read_timeout(Some(Duration::from_millis(600))).ok();
            let mut post_handshake = String::new();
            let mut buf = String::new();
            loop {
                buf.clear();
                match br.read_line(&mut buf) {
                    Ok(0) => break,         // client closed
                    Ok(_) => post_handshake.push_str(&buf),
                    Err(_) => break,        // read timeout → done collecting
                }
            }
            post_handshake
        });

        let client = StratumClient::connect(&addr.to_string(), "csd1testworker")
            .expect("connect ok");

        // Wait for the reader to surface the pushed job (async).
        let mut got_job = false;
        for _ in 0..50 {
            if client.latest_job().is_some() {
                got_job = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(got_job, "client should surface the pushed job");
        assert_eq!(client.current_difficulty(), 1024.0);

        // Run the real loop in a thread; stop it shortly after.
        let stop = Arc::new(AtomicBool::new(false));
        let backend = CpuBackend::new(1);
        let stop_for_loop = stop.clone();
        let handle = std::thread::spawn(move || {
            // cpu_threads=0 → the CpuBackend's own internal threads do the
            // hashing (the in-loop dual pool is disabled, as in `--backend cpu`).
            let cfg = MiningConfig {
                cpu_threads: 0,
                cpu_share: 0.0,
            };
            run_stratum(&backend, &client, stop_for_loop, cfg)
        });

        // Let the loop spin briefly (it will map the job and start hashing),
        // then ask it to stop.
        std::thread::sleep(Duration::from_millis(300));
        stop.store(true, Ordering::Relaxed);

        let result = handle.join().expect("loop thread did not panic");
        assert!(result.is_ok(), "run_stratum returned Ok on clean shutdown");

        let post_handshake = server.join().unwrap_or_default();
        // No share clears pdiff-1/1024 in 300ms, so no submit should have been
        // sent. (If this ever flakes by *finding* a share, that's a 2^-32 event
        // and would still be a correct submit — but practically it won't.)
        assert!(
            !post_handshake.contains("mining.submit"),
            "did not expect a submit in the brief hard-difficulty window, got: {post_handshake:?}"
        );
    }
}
