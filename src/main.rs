//! csd-pool-miner CLI.
//!
//! The public build mines to the CSD pool by default: it connects to the
//! compiled-in pool endpoint (see [`csd_gpu_miner::endpoint`]) over Stratum v1.
//! There is intentionally **no** node/pool override flag — the only required
//! argument is `--address`, your csd1 payout address.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand, ValueEnum};

use csd_gpu_miner::backends::cpu::CpuBackend;
use csd_gpu_miner::endpoint;
use csd_gpu_miner::logging;
use csd_gpu_miner::mining_config::MiningConfig;
use csd_gpu_miner::stratum::{run_stratum, StratumClient};

#[cfg(feature = "opencl")]
use csd_gpu_miner::backends::opencl::OpenclBackend;

#[cfg(feature = "cuda")]
use csd_gpu_miner::backends::cuda::CudaBackend;

#[derive(Parser, Debug)]
#[command(
    name = "csd-pool-miner",
    version,
    about = "Standalone pool miner for Compute Substrate (mines to the CSD pool)."
)]
struct Cli {
    /// Your csd1 payout address (the pool credits shares to this address).
    /// Required. Accepts 40 lowercase hex chars, optionally `0x`-prefixed (42).
    #[arg(long)]
    address: String,

    /// Backend to use.
    #[arg(long, default_value = "auto")]
    backend: BackendChoice,

    /// Total CPU threads to use for hashing in the CPU backend (or
    /// fallback). Defaults to all logical cores minus `--reserve`.
    #[arg(long)]
    threads: Option<usize>,

    /// CPU threads to leave free for the OS + node + dashboard + the
    /// miner's own I/O. The CPU backend will use `available - reserve`
    /// (clamped to >= 1). Ignored when a GPU backend is active — the
    /// GPU is doing the hashing and CPU threads here only handle I/O.
    #[arg(long, default_value_t = 4)]
    reserve: usize,

    /// GPU launch geometry: blocks per kernel launch.
    #[arg(long, default_value_t = 560)]
    blocks: u32,

    /// GPU launch geometry: threads per block.
    #[arg(long, default_value_t = 256)]
    threads_per_block: u32,

    /// GPU kernel inner loop: nonces tried per thread per launch.
    /// Total nonces per launch = blocks * threads_per_block * nonces_per_thread.
    /// Default 560*256*4096 = 587M nonces/launch.
    #[arg(long, default_value_t = 4096)]
    nonces_per_thread: u32,

    /// Dual mining: CPU worker threads to run alongside the GPU
    /// backend. 0 disables CPU mining (GPU-only).
    /// Range 0..num_cpus. Each worker uses SHA-NI via sha2::compress256.
    /// Presets:  light=6   mid=8   heavy=16
    /// Modern desktop CPUs sustain ~115 MH/s per thread, so 16 threads ≈
    /// 1.8 GH/s on top of GPU.
    #[arg(long, default_value_t = 16)]
    cpu_threads: usize,

    /// Dual mining: fraction of the per-template nonce range the
    /// CPU pool sweeps (0.0..=1.0). GPU takes the rest. Default 0.4 maps
    /// roughly to "CPU+GPU finish their slices in similar wall time" for
    /// a 16-thread CPU + 3+ GH/s GPU. 0.0 disables CPU mining.
    #[arg(long, default_value_t = 0.4)]
    cpu_share: f32,

    /// GPU device index to mine on (see the `devices` subcommand for the list).
    /// Default 0. To use multiple GPUs, run one instance per card, each with a
    /// different --device (e.g. --device 0 and --device 1), all to the same
    /// address — the pool sums their shares.
    #[arg(long, default_value_t = 0)]
    device: usize,

    /// Log directory (rotates previous log on startup).
    #[arg(long, default_value = "logs")]
    log_dir: PathBuf,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// Validate a csd1 payout address and return its canonical 40-lowercase-hex
/// form (the `0x` prefix, if present, is stripped). Accepts exactly 40
/// lowercase hex chars, or 42 chars when `0x`-prefixed. Rejects wrong length,
/// uppercase, and any non-hex character.
///
/// Kept pure (no I/O) so it is unit-testable and so `main` can fail fast with a
/// clear message before opening a socket to the pool.
fn validate_address(addr: &str) -> Result<String> {
    let body = addr.strip_prefix("0x").unwrap_or(addr);
    if body.len() != 40 {
        bail!(
            "--address must be 40 hex chars (or 42 with a 0x prefix); got {} chars",
            addr.len()
        );
    }
    if !body.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        bail!("--address must be lowercase hex (0-9, a-f); got {addr:?}");
    }
    Ok(body.to_string())
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Probe and print available GPU devices, then exit. Use this when
    /// `--backend auto` keeps falling back to CPU and you want to know why.
    Devices,

    /// Cross-check every available backend against the canonical CPU
    /// sha256d on randomized inputs. Exits 0 if all backends agree, 1
    /// on any mismatch.
    Selftest {
        /// Number of randomized trials per backend.
        #[arg(long, default_value_t = 4)]
        trials: usize,

        /// Nonce range to scan per trial (must be <= u32::MAX).
        #[arg(long, default_value_t = 1_048_576)]
        nonce_range: u32,

        /// How many leading zero bytes the target requires (controls
        /// expected hits-per-trial). Default 2 → ~16 hits in 1M.
        #[arg(long, default_value_t = 2)]
        target_zero_bytes: usize,

        /// Deterministic RNG seed so failures are reproducible.
        #[arg(long, default_value_t = 0xC0FFEE)]
        seed: u64,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BackendChoice {
    Auto,
    Cpu,
    Opencl,
    Cuda,
}

fn num_cpus_default() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn cpu_hashing_threads(cli: &Cli) -> usize {
    if let Some(t) = cli.threads {
        return t.max(1);
    }
    let avail = num_cpus_default();
    avail.saturating_sub(cli.reserve).max(1)
}

/// Build the dual-mining MiningConfig from CLI flags.
///
/// When a GPU backend is active, `--cpu-threads`/`--cpu-share` directly
/// drive the in-loop CPU worker pool that races the GPU per launch. When the
/// active backend IS the CPU backend (no GPU usable), we deliberately zero the
/// dual-mining pool: the CPU backend already saturates all its hashing threads
/// internally, so spawning a second pool inside the loop would just contend
/// with itself.
fn build_mining_config(cli: &Cli, backend_is_cpu: bool) -> MiningConfig {
    if backend_is_cpu {
        return MiningConfig {
            cpu_threads: 0,
            cpu_share: 0.0,
        };
    }
    let max_threads = num_cpus_default();
    let cpu_threads = cli.cpu_threads.min(max_threads);
    let cpu_share = cli.cpu_share.clamp(0.0, 1.0);
    MiningConfig {
        cpu_threads,
        cpu_share,
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let _log_guard = logging::init("csd-pool-miner", &cli.log_dir)?;

    if matches!(cli.cmd, Some(Cmd::Devices)) {
        return print_devices();
    }

    if let Some(Cmd::Selftest {
        trials,
        nonce_range,
        target_zero_bytes,
        seed,
    }) = cli.cmd
    {
        return csd_gpu_miner::selftest::run(csd_gpu_miner::selftest::SelftestOpts {
            trials,
            nonce_range,
            target_zero_bytes,
            seed,
            blocks: cli.blocks,
            threads_per_block: cli.threads_per_block,
            nonces_per_thread: cli.nonces_per_thread,
        });
    }

    print_build_features();

    // Validate the payout address up front so a typo fails fast (before we open
    // a socket to the pool) with a clear message.
    let address = validate_address(&cli.address)?;

    // The pool endpoint is compiled in (lightly obfuscated) — the public build
    // has no override flag. Connect over Stratum v1 and authorize as `address`.
    let endpoint = endpoint::pool_endpoint();
    tracing::info!(
        "csd-pool-miner: connecting to pool {endpoint} as address {address}"
    );
    let client = StratumClient::connect(&endpoint, &address)
        .map_err(|e| anyhow::anyhow!("failed to connect to pool {endpoint}: {e}"))?;

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc_lite(move || {
            tracing::warn!("ctrl-c, shutting down");
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }

    match cli.backend {
        BackendChoice::Cpu => {
            let n = cpu_hashing_threads(&cli);
            let b = CpuBackend::new(n);
            tracing::info!(
                "backend=cpu (forced) hashing_threads={} reserved={}",
                b.threads,
                cli.reserve
            );
            run_stratum(&b, &client, stop, build_mining_config(&cli, true))
        }

        #[cfg(feature = "opencl")]
        BackendChoice::Opencl => {
            tracing::info!(
                "backend=opencl (forced) blocks={} tpb={} npt={} - trying init...",
                cli.blocks, cli.threads_per_block, cli.nonces_per_thread,
            );
            let init = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                OpenclBackend::new(cli.device, cli.blocks, cli.threads_per_block, cli.nonces_per_thread)
            }));
            let b = match init {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => {
                    tracing::error!("opencl init failed: {}", e);
                    bail!("opencl init failed: {}", e);
                }
                Err(_) => bail!("opencl init panicked; try --backend cpu"),
            };
            tracing::info!(
                "backend=opencl ready (geom={}x{}x{} = {} nonces/launch, 2-queue pipelined)",
                b.blocks, b.threads_per_block, b.nonces_per_thread,
                (b.blocks as u64) * (b.threads_per_block as u64) * (b.nonces_per_thread as u64),
            );
            run_stratum(&b, &client, stop, build_mining_config(&cli, false))
        }
        #[cfg(not(feature = "opencl"))]
        BackendChoice::Opencl => bail!("opencl backend not compiled in (rebuild with --features opencl)"),

        #[cfg(feature = "cuda")]
        BackendChoice::Cuda => {
            tracing::info!(
                "backend=cuda (forced) blocks={} tpb={} npt={} - trying init...",
                cli.blocks, cli.threads_per_block, cli.nonces_per_thread,
            );
            // cudarc can panic (not just return Err) on a CUDA-toolkit/NVRTC
            // version mismatch; catch it so we exit with a clear message
            // instead of an unwinding backtrace.
            let init = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                CudaBackend::new(cli.device, cli.blocks, cli.threads_per_block, cli.nonces_per_thread)
            }));
            let b = match init {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => {
                    tracing::error!("cuda init failed: {}", e);
                    bail!("cuda init failed: {}", e);
                }
                Err(_) => bail!(
                    "cuda init panicked (likely a cudarc/NVRTC vs CUDA-toolkit mismatch); try --backend opencl or --backend cpu"
                ),
            };
            tracing::info!(
                "backend=cuda ready (geom={}x{}x{} = {} nonces/launch, 2-stream pipelined)",
                b.blocks, b.threads_per_block, b.nonces_per_thread,
                (b.blocks as u64) * (b.threads_per_block as u64) * (b.nonces_per_thread as u64),
            );
            run_stratum(&b, &client, stop, build_mining_config(&cli, false))
        }
        #[cfg(not(feature = "cuda"))]
        BackendChoice::Cuda => bail!("cuda backend not compiled in (rebuild with --features cuda)"),

        BackendChoice::Auto => {
            tracing::info!("backend=auto - probing in order: cuda -> opencl -> cpu");

            #[cfg(feature = "cuda")]
            {
                tracing::info!(
                    "auto: trying CUDA geom={}x{}x{}",
                    cli.blocks, cli.threads_per_block, cli.nonces_per_thread
                );
                // cudarc panics (rather than returning Err) when its
                // hard-coded NVRTC dll name doesn't match the installed
                // CUDA toolkit (e.g. CUDA 13 vs cudarc 0.11 looking for
                // nvrtc64_122.dll). Catch the panic so `auto` can fall
                // through to OpenCL.
                let cuda_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    CudaBackend::new(cli.device, cli.blocks, cli.threads_per_block, cli.nonces_per_thread)
                }));
                match cuda_result {
                    Ok(Ok(b)) => {
                        tracing::info!(
                            "auto: SELECTED cuda (geom={}x{}x{} = {} nonces/launch, 2-stream pipelined)",
                            b.blocks, b.threads_per_block, b.nonces_per_thread,
                            (b.blocks as u64) * (b.threads_per_block as u64) * (b.nonces_per_thread as u64),
                        );
                        return run_stratum(&b, &client, stop, build_mining_config(&cli, false));
                    }
                    Ok(Err(e)) => {
                        tracing::warn!("auto: CUDA init returned error: {}", e);
                    }
                    Err(p) => {
                        let msg = if let Some(s) = p.downcast_ref::<&'static str>() {
                            (*s).to_string()
                        } else if let Some(s) = p.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "<non-string panic>".to_string()
                        };
                        tracing::warn!(
                            "auto: CUDA init panicked (cudarc/nvrtc version mismatch?): {}",
                            msg
                        );
                    }
                }
            }
            #[cfg(not(feature = "cuda"))]
            {
                tracing::warn!("auto: CUDA not compiled in (build with --features cuda to enable)");
            }

            #[cfg(feature = "opencl")]
            {
                tracing::info!(
                    "auto: trying OpenCL geom={}x{}x{}",
                    cli.blocks, cli.threads_per_block, cli.nonces_per_thread
                );
                match OpenclBackend::new(cli.device, cli.blocks, cli.threads_per_block, cli.nonces_per_thread) {
                    Ok(b) => {
                        tracing::info!(
                            "auto: SELECTED opencl (geom={}x{}x{} = {} nonces/launch, 2-queue pipelined)",
                            b.blocks, b.threads_per_block, b.nonces_per_thread,
                            (b.blocks as u64) * (b.threads_per_block as u64) * (b.nonces_per_thread as u64),
                        );
                        return run_stratum(&b, &client, stop, build_mining_config(&cli, false));
                    }
                    Err(e) => {
                        tracing::warn!("auto: OpenCL init failed: {}", e);
                    }
                }
            }
            #[cfg(not(feature = "opencl"))]
            {
                tracing::warn!("auto: OpenCL not compiled in");
            }

            let n = cpu_hashing_threads(&cli);
            let b = CpuBackend::new(n);
            tracing::warn!(
                "auto: SELECTED cpu (no GPU backend usable). hashing_threads={} reserved={}",
                b.threads,
                cli.reserve
            );
            run_stratum(&b, &client, stop, build_mining_config(&cli, true))
        }
    }
}

fn print_build_features() {
    let cuda = cfg!(feature = "cuda");
    let opencl = cfg!(feature = "opencl");
    tracing::info!(
        "build features: cuda={} opencl={}",
        cuda,
        opencl
    );
    if !cuda {
        tracing::info!("  to enable CUDA: cargo build -p csd-pool-miner --release --features cuda");
    }
}

fn print_devices() -> Result<()> {
    println!("=== csd-gpu-miner devices ===");
    println!();
    println!("build features: cuda={} opencl={}", cfg!(feature = "cuda"), cfg!(feature = "opencl"));
    println!();

    #[cfg(feature = "cuda")]
    {
        println!("CUDA:");
        // cudarc 0.19: CudaDevice -> CudaContext.
        match cudarc::driver::CudaContext::device_count() {
            Ok(n) if n > 0 => {
                for i in 0..n {
                    match cudarc::driver::CudaContext::new(i as usize) {
                        Ok(ctx) => {
                            let name = ctx.name().unwrap_or_else(|_| "<unknown>".into());
                            println!("  [{}] {}", i, name);
                        }
                        Err(e) => println!("  [{}] (init failed: {})", i, e),
                    }
                }
            }
            Ok(_) => println!("  (no CUDA devices)"),
            Err(e) => println!("  (CUDA driver not reachable: {})", e),
        }
        println!();
    }
    #[cfg(not(feature = "cuda"))]
    {
        println!("CUDA: backend not compiled in (build with --features cuda)");
        println!();
    }

    #[cfg(feature = "opencl")]
    {
        println!("OpenCL:");
        use opencl3::device::{get_all_devices, Device, CL_DEVICE_TYPE_ALL};
        match get_all_devices(CL_DEVICE_TYPE_ALL) {
            Ok(devs) if !devs.is_empty() => {
                for (i, d) in devs.iter().enumerate() {
                    let dev = Device::new(*d);
                    let name = dev.name().unwrap_or_default();
                    let vendor = dev.vendor().unwrap_or_default();
                    let version = dev.version().unwrap_or_default();
                    println!("  [{}] {} ({}) - {}", i, name, vendor, version);
                }
            }
            Ok(_) => println!("  (no OpenCL devices)"),
            Err(e) => println!("  (OpenCL not reachable: {:?})", e),
        }
    }
    #[cfg(not(feature = "opencl"))]
    {
        println!("OpenCL: backend not compiled in (build with --features opencl)");
    }
    Ok(())
}

/// Install a Ctrl-C handler that runs `handler` (which sets the stop flag) on
/// interrupt, so the miner shuts down cleanly instead of being hard-killed.
fn ctrlc_lite<F: Fn() + Send + 'static>(handler: F) {
    if let Err(e) = ctrlc::set_handler(move || handler()) {
        tracing::warn!("could not install ctrl-c handler ({e}); Ctrl-C will hard-stop");
    }
}

#[cfg(test)]
mod tests {
    use super::validate_address;

    #[test]
    fn accepts_40_lowercase_hex() {
        let addr = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(addr.len(), 40);
        assert_eq!(validate_address(addr).unwrap(), addr);
    }

    #[test]
    fn accepts_0x_prefixed_and_strips_it() {
        let body = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
        let prefixed = format!("0x{body}");
        assert_eq!(prefixed.len(), 42);
        // The canonical form drops the 0x prefix.
        assert_eq!(validate_address(&prefixed).unwrap(), body);
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(validate_address("abcd").is_err()); // too short
        assert!(validate_address(&"a".repeat(39)).is_err()); // 39
        assert!(validate_address(&"a".repeat(41)).is_err()); // 41 (no 0x)
        assert!(validate_address(&format!("0x{}", "a".repeat(39))).is_err()); // 0x + 39
        assert!(validate_address(&format!("0x{}", "a".repeat(41))).is_err()); // 0x + 41
    }

    #[test]
    fn rejects_non_hex() {
        // 'g' is not a hex digit.
        assert!(validate_address("0123456789abcdef0123456789abcdef0123456g").is_err());
    }

    #[test]
    fn rejects_uppercase() {
        // Uppercase hex is rejected (csd1 addresses are lowercase hex).
        assert!(validate_address("0123456789ABCDEF0123456789abcdef01234567").is_err());
    }
}
