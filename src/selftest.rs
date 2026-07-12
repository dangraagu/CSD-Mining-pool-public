//! `csd-gpu-miner selftest` — cross-checks each available backend against
//! the canonical CPU sha256d on randomized inputs.
//!
//! For each of N trials:
//!   1. Generate a random 80-byte header.
//!   2. Pick a target tight enough that we expect ~few hits in [0..range).
//!   3. Run each backend over [0..range).
//!   4. If a backend returns a nonce, re-hash on CPU and assert
//!      - the GPU's reported hash matches CPU
//!      - the hash is in fact <= target
//!      - the nonce is within the requested range
//!   5. Also run the CPU backend over the same range and require that at
//!      least one backend either finds a solution or all backends agree
//!      "no solution".
//!
//! Reports PASS / FAIL per backend and a final summary. Exit code is
//! non-zero on any FAIL.

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::Result;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;

use crate::backend::{MiningBackend, MiningResult};
use crate::backends::cpu::CpuBackend;
use crate::sha256d_cpu::sha256d;

#[cfg(feature = "opencl")]
use crate::backends::opencl::OpenclBackend;

#[cfg(feature = "cuda")]
use crate::backends::cuda::CudaBackend;

#[derive(Clone, Copy, Debug)]
pub struct SelftestOpts {
    pub trials: usize,
    pub nonce_range: u32,
    /// Initial 32-byte BE target. Bytes after `target_zero_bytes` are 0xFF;
    /// the first `target_zero_bytes` bytes are 0x00.
    pub target_zero_bytes: usize,
    pub seed: u64,
    pub blocks: u32,
    pub threads_per_block: u32,
    pub nonces_per_thread: u32,
}

impl Default for SelftestOpts {
    fn default() -> Self {
        Self {
            trials: 4,
            nonce_range: 1 << 20, // 1M
            target_zero_bytes: 2, // ~1/65536 hashes pass — expect ~16 hits in 1M
            seed: 0xC0FFEE,
            blocks: 64,
            threads_per_block: 256,
            nonces_per_thread: 64,
        }
    }
}

#[derive(PartialEq, Eq)]
enum Status {
    Pass,
    Skipped, // not available on this box; not a correctness failure
    Fail,    // correctness divergence: hard fail
}

struct Outcome {
    backend: &'static str,
    status: Status,
    notes: Vec<String>,
    total_solves: usize,
    total_micros: u128,
}

pub fn run(opts: SelftestOpts) -> Result<()> {
    let mut rng = ChaCha20Rng::seed_from_u64(opts.seed);

    println!("=== csd-gpu-miner selftest ===");
    println!(
        "trials={} nonce_range={} target=0x{}…ff (zero_bytes={}) seed=0x{:x}",
        opts.trials,
        opts.nonce_range,
        "00".repeat(opts.target_zero_bytes),
        opts.target_zero_bytes,
        opts.seed
    );
    println!();

    // Build target.
    let mut target = [0xFFu8; 32];
    for i in 0..opts.target_zero_bytes.min(32) {
        target[i] = 0x00;
    }

    // CPU backend is the reference. Always present. `mut` is only exercised
    // by the cfg-gated GPU pushes below.
    #[cfg_attr(not(any(feature = "cuda", feature = "opencl")), allow(unused_mut))]
    let mut outcomes: Vec<Outcome> = vec![eval_backend(
        "cpu",
        Box::new(CpuBackend::new(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1))),
        &mut rng,
        &target,
        opts,
    )];

    #[cfg(feature = "opencl")]
    {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            OpenclBackend::new(0, opts.blocks, opts.threads_per_block, opts.nonces_per_thread)
        }));
        match r {
            Ok(Ok(b)) => {
                let mut rng = ChaCha20Rng::seed_from_u64(opts.seed); // identical seed
                outcomes.push(eval_backend("opencl", Box::new(b), &mut rng, &target, opts));
            }
            Ok(Err(e)) => {
                outcomes.push(Outcome {
                    backend: "opencl",
                    status: Status::Skipped,
                    notes: vec![format!("init failed: {}", e)],
                    total_solves: 0,
                    total_micros: 0,
                });
            }
            Err(_) => {
                outcomes.push(Outcome {
                    backend: "opencl",
                    status: Status::Skipped,
                    notes: vec!["init panicked".to_string()],
                    total_solves: 0,
                    total_micros: 0,
                });
            }
        }
    }

    #[cfg(feature = "cuda")]
    {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            CudaBackend::new(0, opts.blocks, opts.threads_per_block, opts.nonces_per_thread)
        }));
        match r {
            Ok(Ok(b)) => {
                let mut rng = ChaCha20Rng::seed_from_u64(opts.seed);
                outcomes.push(eval_backend("cuda", Box::new(b), &mut rng, &target, opts));
            }
            Ok(Err(e)) => {
                outcomes.push(Outcome {
                    backend: "cuda",
                    status: Status::Skipped,
                    notes: vec![format!("init failed: {}", e)],
                    total_solves: 0,
                    total_micros: 0,
                });
            }
            Err(_) => {
                outcomes.push(Outcome {
                    backend: "cuda",
                    status: Status::Skipped,
                    notes: vec!["init panicked (likely cudarc/nvrtc DLL mismatch)".to_string()],
                    total_solves: 0,
                    total_micros: 0,
                });
            }
        }
    }

    println!();
    println!("=== summary ===");
    let mut any_fail = false;
    let mut usable = 0;
    let mut passes = 0;
    for o in &outcomes {
        let tag = match o.status {
            Status::Pass => "PASS",
            Status::Skipped => "SKIP",
            Status::Fail => "FAIL",
        };
        let rate = if o.total_micros > 0 {
            let nonces_total: u128 = (opts.nonce_range as u128) * (opts.trials as u128);
            let mh = (nonces_total as f64) / (o.total_micros as f64);
            format!("{:.1} MH/s", mh)
        } else {
            "n/a".to_string()
        };
        println!(
            "  {:<8} {} - {} solves, {} elapsed total, {}",
            o.backend,
            tag,
            o.total_solves,
            humantime::format_duration(std::time::Duration::from_micros(o.total_micros as u64)),
            rate,
        );
        for n in &o.notes {
            println!("              - {}", n);
        }
        match o.status {
            Status::Fail => {
                any_fail = true;
                usable += 1;
            }
            Status::Pass => {
                usable += 1;
                passes += 1;
            }
            Status::Skipped => {}
        }
    }
    println!();
    if any_fail {
        anyhow::bail!("CORRECTNESS FAILURE: one or more usable backends diverged from canonical sha256d");
    }
    println!(
        "RESULT: {} backend(s) passed, {} skipped (no usable runtime)",
        passes,
        outcomes.len() - usable
    );
    Ok(())
}

fn eval_backend(
    name: &'static str,
    backend: Box<dyn MiningBackend>,
    rng: &mut ChaCha20Rng,
    target: &[u8; 32],
    opts: SelftestOpts,
) -> Outcome {
    let mut notes: Vec<String> = Vec::new();
    let mut pass = true;
    let mut total_solves = 0usize;
    let mut total_micros: u128 = 0;
    let stop = AtomicBool::new(false);
    // Never-set so selftest sweeps deterministically to completion (bit-exact
    // verification must never be preempted; job-change preemption is mining-only).
    let never_new_job = AtomicBool::new(false);

    for trial in 0..opts.trials {
        // Fresh random header for each trial.
        let mut header = [0u8; 84];
        rng.fill(&mut header[..]);
        // Make sure version field is reasonable (set to 1).
        header[0..4].copy_from_slice(&1u32.to_le_bytes());

        let started = Instant::now();
        let res = backend.hash_range(header, *target, 0, opts.nonce_range, &stop, &never_new_job);
        let elapsed_micros = started.elapsed().as_micros();
        total_micros += elapsed_micros;

        // IMP-1b: a device fault is now distinguishable from a clean empty
        // sweep — report it as its own failure note instead of the misleading
        // "backend missed a solution" the old None-mapping produced.
        let res = match res {
            Ok(r) => r,
            Err(e) => {
                notes.push(format!("trial {}: DEVICE ERROR: {}", trial, e));
                pass = false;
                continue;
            }
        };

        match res {
            Some(MiningResult { nonce, hash }) => {
                total_solves += 1;

                // Check 1: nonce in range
                if nonce >= opts.nonce_range {
                    notes.push(format!(
                        "trial {}: out-of-range nonce {} >= {}",
                        trial, nonce, opts.nonce_range
                    ));
                    pass = false;
                    continue;
                }

                // Check 2: CPU re-hash matches
                let mut hdr_check = header;
                hdr_check[80..84].copy_from_slice(&nonce.to_le_bytes());
                let cpu_hash = sha256d(&hdr_check);
                if cpu_hash != hash {
                    notes.push(format!(
                        "trial {}: HASH MISMATCH at nonce={}: backend=0x{} cpu=0x{}",
                        trial,
                        nonce,
                        hex::encode(hash),
                        hex::encode(cpu_hash),
                    ));
                    pass = false;
                    continue;
                }

                // Check 3: hash <= target
                if cpu_hash > *target {
                    notes.push(format!(
                        "trial {}: hash 0x{} above target 0x{}",
                        trial,
                        hex::encode(cpu_hash),
                        hex::encode(target),
                    ));
                    pass = false;
                    continue;
                }
            }
            None => {
                // No solution in this trial. Verify by exhaustive CPU search
                // — at this target there should usually be at least one hit
                // in 1M nonces (probability ~1 - (1 - 2^-16)^(2^20) ≈ 1 in
                // billions). If a hit exists, the backend missed it.
                if let Some(n) = exhaustive_cpu_scan(&header, target, opts.nonce_range) {
                    notes.push(format!(
                        "trial {}: CPU found a solution that backend missed at nonce={}",
                        trial, n
                    ));
                    pass = false;
                }
            }
        }
    }

    Outcome {
        backend: name,
        status: if pass { Status::Pass } else { Status::Fail },
        notes,
        total_solves,
        total_micros,
    }
}

fn exhaustive_cpu_scan(header: &[u8; 84], target: &[u8; 32], range: u32) -> Option<u32> {
    let mut h = *header;
    for n in 0..range {
        h[80..84].copy_from_slice(&n.to_le_bytes());
        let d = sha256d(&h);
        if d <= *target {
            return Some(n);
        }
    }
    None
}

/// Probe whether a `host:port` pool endpoint is reachable: resolve it and open a
/// TCP connection with a short timeout (then drop it immediately — this is
/// non-destructive, it never subscribes). `Ok(())` = reachable; `Err(msg)` =
/// unresolvable or unreachable. (v0.1.9 #4)
pub fn probe_reachable(endpoint: &str, timeout: Duration) -> Result<(), String> {
    let addr = endpoint
        .to_socket_addrs()
        .map_err(|e| format!("resolve failed: {e}"))?
        .next()
        .ok_or_else(|| "no address resolved".to_string())?;
    TcpStream::connect_timeout(&addr, timeout)
        .map(|_| ())
        .map_err(|e| format!("connect failed: {e}"))
}

/// Print a pool-reachability report (PASS/FAIL per endpoint). DIAGNOSTIC ONLY +
/// NON-FATAL — it never changes the selftest exit code (which stays
/// backend-correctness-only) — so one `selftest` answers both "are the backends
/// correct?" and "can I actually reach the pool?".
pub fn print_reachability(endpoints: &[String]) {
    println!();
    println!("=== pool reachability ===");
    for ep in endpoints {
        match probe_reachable(ep, Duration::from_secs(5)) {
            Ok(()) => println!("  {ep:<28} PASS - tcp connect ok"),
            Err(e) => println!("  {ep:<28} FAIL - {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn probe_reachable_connects_to_a_live_listener() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(probe_reachable(&format!("127.0.0.1:{port}"), Duration::from_secs(2)).is_ok());
    }

    #[test]
    fn probe_reachable_fails_on_refused_and_unresolvable() {
        // Nothing listening on loopback port 1 → connection refused.
        assert!(probe_reachable("127.0.0.1:1", Duration::from_millis(500)).is_err());
        // `.invalid` is a reserved never-resolvable TLD → resolve failure.
        assert!(probe_reachable("nonexistent.invalid:3333", Duration::from_millis(500)).is_err());
    }
}
