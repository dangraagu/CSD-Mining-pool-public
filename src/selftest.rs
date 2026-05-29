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

use std::sync::atomic::AtomicBool;
use std::time::Instant;

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

    let mut outcomes: Vec<Outcome> = Vec::new();

    // CPU backend is the reference. Always present.
    outcomes.push(eval_backend(
        "cpu",
        Box::new(CpuBackend::new(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1))),
        &mut rng,
        &target,
        opts,
    ));

    #[cfg(feature = "opencl")]
    {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            OpenclBackend::new(opts.blocks, opts.threads_per_block, opts.nonces_per_thread)
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
            CudaBackend::new(opts.blocks, opts.threads_per_block, opts.nonces_per_thread)
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

    for trial in 0..opts.trials {
        // Fresh random header for each trial.
        let mut header = [0u8; 84];
        rng.fill(&mut header[..]);
        // Make sure version field is reasonable (set to 1).
        header[0..4].copy_from_slice(&1u32.to_le_bytes());

        let started = Instant::now();
        let res = backend.hash_range(header, *target, 0, opts.nonce_range, &stop);
        let elapsed_micros = started.elapsed().as_micros();
        total_micros += elapsed_micros;

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
