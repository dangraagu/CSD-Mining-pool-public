//! csd-pool-miner CLI.
//!
//! The public build mines to the CSD pool by default: it connects to the
//! compiled-in pool endpoint (see [`csd_gpu_miner::endpoint`]) over Stratum v1.
//! There is intentionally **no** node/pool override flag — the only required
//! argument is `--address`, your addr20 payout address.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{bail, Result};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

use csd_gpu_miner::backends::cpu::CpuBackend;
use csd_gpu_miner::endpoint;
use csd_gpu_miner::logging;
use csd_gpu_miner::mining_config::MiningConfig;
use csd_gpu_miner::notify::{self, DiscordNotifier};
use csd_gpu_miner::http;
use csd_gpu_miner::stats_server::{self, StatsHandle};
use csd_gpu_miner::{hiveos, selfupdate};
use csd_gpu_miner::stratum::{run_stratum, StratumClient};

#[cfg(feature = "opencl")]
use csd_gpu_miner::backends::opencl::OpenclBackend;

#[cfg(feature = "cuda")]
use csd_gpu_miner::backends::cuda::CudaBackend;

mod config_file;
mod keygen;

#[derive(Parser, Debug)]
#[command(
    name = "csd-pool-miner",
    version,
    about = "Standalone pool miner for Compute Substrate (mines to the CSD pool)."
)]
struct Cli {
    /// Path to a TOML config file. If omitted, `./config.toml` then the platform
    /// config dir (`~/.config/csd-pool-miner/config.toml`, or on Windows
    /// `%APPDATA%\csd-pool-miner\config.toml`) are tried. Explicit CLI flags
    /// always override the config file. See `config.example.toml`.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Your addr20 payout address (the pool credits shares to this address).
    /// 40 lowercase hex chars, optionally `0x`-prefixed (42). Provide it here or
    /// as `address =` in the config file; this flag wins if both are set.
    #[arg(long)]
    address: Option<String>,

    /// Pool endpoint override(s) as `host:port`, comma-separated or repeated
    /// (alias `--url`). The first is the primary; the rest back failover. If
    /// omitted, the compiled-in default pool is used.
    #[arg(long = "pool", visible_alias = "url", value_delimiter = ',')]
    pool: Vec<String>,

    /// Telemetry: serve an xmrig-compatible `/1/summary` JSON endpoint on this
    /// port (plus `/healthz`) for dashboards/monitoring. Omitted ⇒ no server.
    #[arg(long)]
    stats_port: Option<u16>,

    /// Telemetry: bind IP for `--stats-port`. Default 127.0.0.1 (localhost only);
    /// set 0.0.0.0 to expose on the LAN (an info-leak — pair with
    /// `--stats-password`). Must be an IP, not a hostname.
    #[arg(long, default_value = "127.0.0.1")]
    stats_bind: String,

    /// Telemetry: require this token on `/1/summary` (via `Authorization: Bearer`
    /// or `?token=`). `/healthz` stays open. Omitted ⇒ no auth.
    #[arg(long)]
    stats_password: Option<String>,

    /// Discord webhook URL for accepted-share milestone alerts. Must be an https
    /// Discord webhook.
    #[arg(long)]
    discord_webhook: Option<String>,

    /// Stay silent: suppress the accepted-share milestone pings. (Vestigial in
    /// pool-only mode — a pool miner never solves a full block.)
    #[arg(long)]
    discord_solutions_only: bool,

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

    /// GPU include-list as a comma-separated list of device indices (e.g.
    /// `--gpu-id 0,2`). This single process still mines ONE device (`--device`);
    /// the include-list is the launcher contract — `mine-auto`/`mine-all-gpus`
    /// read it to decide which cards to spawn a process for (so you can skip a
    /// bad card). Parsed + validated here (junk is rejected early via
    /// [`csd_gpu_miner::hiveos::parse_gpu_ids`]) so it is also future-proof for
    /// in-process multi-GPU (v0.2). Empty/absent ⇒ no filter (all cards).
    #[arg(long)]
    gpu_id: Option<String>,

    /// List the GPU devices this build can see, then exit (flag alias for the
    /// `devices` subcommand). Use it when `--backend auto` keeps falling back to
    /// CPU and you want to know why.
    #[arg(long)]
    list_devices: bool,

    /// Log directory (rotates previous log on startup).
    #[arg(long, default_value = "logs")]
    log_dir: PathBuf,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// Validate an addr20 payout address and return its canonical 40-lowercase-hex
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
    /// Create a brand-new CSD payout wallet (keypair + addr20) locally, print
    /// it, and save it to ./csd-wallet.txt. The private key is generated on
    /// this machine and is NEVER sent anywhere — back it up, losing it loses
    /// the coins. The address it prints is what you pass to `--address`.
    Newwallet,

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

    /// Self-update helper (P4): semver-compare two versions. Prints
    /// `update-available` + exits 0 if `latest` is newer than `current`, else
    /// prints `up-to-date` + exits 1. Lets the launcher scripts gate an update on
    /// ONE tested semver compare (`0.1.10 > 0.1.9`) instead of a fragile string
    /// `!=`. Exits immediately; does not mine.
    CheckUpdate {
        /// The currently-installed version (e.g. the running binary's version).
        #[arg(long)]
        current: String,
        /// The candidate version (e.g. the latest release tag).
        #[arg(long)]
        latest: String,
    },

    /// Self-update helper (P4): verify a downloaded file's SHA-256 before it is
    /// swapped in + executed. Reads `file`, compares its digest to `sha256`
    /// (case-insensitive hex). Prints `ok` + exits 0 on match, `MISMATCH` + exits
    /// 1 on mismatch, or the read error + exits 2 if the file can't be read.
    /// Exits immediately; does not mine.
    VerifyFile {
        /// Path to the downloaded file to verify.
        file: PathBuf,
        /// Expected SHA-256 hex digest (from the release `SHA256SUMS`).
        sha256: String,
    },

    /// HiveOS integration (P4): scrape this miner's own `/1/summary` stats
    /// endpoint and print the HiveOS h-stats JSON on stdout, for `h-stats.sh` to
    /// relay. On ANY failure (server down, non-200, parse error) prints a valid
    /// zero h-stats object so HiveOS reads the rig as alive-but-zero rather than
    /// broken. Never panics, never hangs. Exits immediately; does not mine.
    HiveosStats {
        /// Port the local `--stats-port` server is listening on (default 3380).
        #[arg(long, default_value_t = 3380)]
        stats_port: u16,
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

/// Parse a backend name from the config file into a `BackendChoice`.
fn parse_backend(s: &str) -> Option<BackendChoice> {
    match s.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(BackendChoice::Auto),
        "cpu" => Some(BackendChoice::Cpu),
        "opencl" => Some(BackendChoice::Opencl),
        "cuda" => Some(BackendChoice::Cuda),
        _ => None,
    }
}

/// Merge `file` config values into `cli` IN PLACE, but only for fields the user
/// did NOT set explicitly on the command line — giving precedence
/// CLI > config file > built-in default. `address` has no clap default, so it is
/// taken from the file only when absent on the CLI.
fn merge_config(cli: &mut Cli, matches: &clap::ArgMatches, file: config_file::FileConfig) {
    use clap::parser::ValueSource;
    let explicit = |id: &str| matches.value_source(id) == Some(ValueSource::CommandLine);

    if cli.address.is_none() {
        cli.address = file.address;
    }
    if !explicit("backend") {
        if let Some(s) = file.backend.as_deref() {
            match parse_backend(s) {
                Some(b) => cli.backend = b,
                None => tracing::warn!(backend = s, "config: unknown backend, keeping default"),
            }
        }
    }
    if !explicit("threads") {
        if let Some(v) = file.threads {
            cli.threads = Some(v);
        }
    }
    if !explicit("reserve") {
        if let Some(v) = file.reserve {
            cli.reserve = v;
        }
    }
    if !explicit("blocks") {
        if let Some(v) = file.blocks {
            cli.blocks = v;
        }
    }
    if !explicit("threads_per_block") {
        if let Some(v) = file.threads_per_block {
            cli.threads_per_block = v;
        }
    }
    if !explicit("nonces_per_thread") {
        if let Some(v) = file.nonces_per_thread {
            cli.nonces_per_thread = v;
        }
    }
    if !explicit("cpu_threads") {
        if let Some(v) = file.cpu_threads {
            cli.cpu_threads = v;
        }
    }
    if !explicit("cpu_share") {
        if let Some(v) = file.cpu_share {
            cli.cpu_share = v;
        }
    }
    if !explicit("device") {
        if let Some(v) = file.device {
            cli.device = v;
        }
    }
}

fn main() -> Result<()> {
    let matches = Cli::command().get_matches();
    let mut cli = Cli::from_arg_matches(&matches).map_err(|e| anyhow::anyhow!("{e}"))?;
    let _log_guard = logging::init("csd-pool-miner", &cli.log_dir)?;

    // Merge an optional TOML config file. Precedence: explicit CLI flag > config
    // file value > built-in default. Done before any subcommand so values set in
    // the config (geometry, etc.) also apply to `selftest`. Logging is already
    // up, so a parse-failed config produces a visible warning (then is ignored).
    let (file_cfg, loaded_from) = config_file::FileConfig::load(cli.config.as_deref());
    if let Some(p) = &loaded_from {
        tracing::info!(config = %p.display(), "loaded config file");
    }
    merge_config(&mut cli, &matches, file_cfg);

    if matches!(cli.cmd, Some(Cmd::Newwallet)) {
        // No network, no address needed: generate a key locally and exit.
        return keygen::run();
    }

    // `--list-devices` is a flag alias for the `devices` subcommand: same probe,
    // same early exit (xmrig/lolMiner spell it as a flag, not only a subcommand).
    if matches!(cli.cmd, Some(Cmd::Devices)) || cli.list_devices {
        return print_devices();
    }

    // P4 self-update + HiveOS helpers. Each runs a pure check and exits with a
    // shell-meaningful code (so the launcher scripts can `if csd-gpu-miner
    // check-update …; then …`), rather than continuing into the mining path.
    if let Some(Cmd::CheckUpdate { current, latest }) = &cli.cmd {
        if selfupdate::should_update(current, latest) {
            println!("update-available");
            std::process::exit(0);
        } else {
            println!("up-to-date");
            std::process::exit(1);
        }
    }

    if let Some(Cmd::VerifyFile { file, sha256 }) = &cli.cmd {
        match std::fs::read(file) {
            Ok(bytes) => {
                if selfupdate::verify_sha256(&bytes, sha256) {
                    println!("ok");
                    std::process::exit(0);
                } else {
                    println!("MISMATCH");
                    std::process::exit(1);
                }
            }
            Err(e) => {
                println!("error reading {}: {e}", file.display());
                std::process::exit(2);
            }
        }
    }

    if let Some(Cmd::HiveosStats { stats_port }) = &cli.cmd {
        // Scrape our own /1/summary and emit the HiveOS h-stats JSON. On ANY
        // failure (server down, non-200, unparseable body) emit a zero-but-valid
        // object from an empty summary so HiveOS still gets well-formed JSON.
        let url = format!("http://127.0.0.1:{stats_port}/1/summary");
        let stats = match http::http_get(&url) {
            Ok((200, body)) => match serde_json::from_str::<serde_json::Value>(&body) {
                Ok(v) => hiveos::hiveos_stats_from_summary(&v),
                Err(_) => hiveos::hiveos_stats_from_summary(&serde_json::json!({})),
            },
            _ => hiveos::hiveos_stats_from_summary(&serde_json::json!({})),
        };
        println!("{stats}");
        std::process::exit(0);
    }

    if let Some(Cmd::Selftest {
        trials,
        nonce_range,
        target_zero_bytes,
        seed,
    }) = cli.cmd
    {
        let result = csd_gpu_miner::selftest::run(csd_gpu_miner::selftest::SelftestOpts {
            trials,
            nonce_range,
            target_zero_bytes,
            seed,
            blocks: cli.blocks,
            threads_per_block: cli.threads_per_block,
            nonces_per_thread: cli.nonces_per_thread,
        });
        // v0.1.9 #4: also probe pool reachability (non-fatal, diagnostic only) so
        // one `selftest` answers both "backends OK?" and "can I reach the pool?".
        let endpoints = endpoint::resolve_endpoints(&cli.pool, &endpoint::pool_endpoint())
            .unwrap_or_else(|_| vec![endpoint::pool_endpoint()]);
        csd_gpu_miner::selftest::print_reachability(&endpoints);
        return result;
    }

    print_build_features();

    // Validate the payout address up front so a typo fails fast (before we open
    // a socket to the pool or node) with a clear message. It may come from
    // --address or the config file's `address =` key.
    let address = match cli.address.as_deref() {
        Some(a) => validate_address(a)?,
        None => bail!(
            "no payout address: pass --address <addr20>, or set `address = \"<addr20>\"` in a config file (see config.example.toml / the README)"
        ),
    };

    // Fail loud on a nonsensical mining parameter (e.g. a typo like `--blocks 0`
    // or `--cpu-share 5`) instead of silently clamping it. v0.1.9 #3.
    csd_gpu_miner::mining_config::validate_mining_config(
        cli.cpu_share,
        cli.threads,
        cli.blocks,
        cli.threads_per_block,
        cli.nonces_per_thread,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Validate `--gpu-id` up front (junk fails fast, before any socket opens).
    // v0.1.8 mines ONE device per process (`--device`); the include-list is a
    // launcher-level filter (mine-auto/mine-all-gpus read it to pick which cards
    // to spawn), so here we only parse + log it. When set, `--device` should be
    // one of the listed ids — we warn (not error) if it isn't, since a single
    // process legitimately runs one card of the set.
    if let Some(list) = cli.gpu_id.as_deref() {
        let ids = hiveos::parse_gpu_ids(list).map_err(|e| anyhow::anyhow!("--gpu-id: {e}"))?;
        if ids.is_empty() {
            tracing::info!("--gpu-id is empty: no GPU filter (all cards eligible)");
        } else {
            tracing::info!(
                "--gpu-id include-list: {ids:?} (launcher filter; this process mines --device {})",
                cli.device
            );
            if !ids.contains(&cli.device) {
                tracing::warn!(
                    "--device {} is not in --gpu-id {ids:?}; this single process mines --device {} regardless (--gpu-id is the launcher's per-process card selector)",
                    cli.device,
                    cli.device
                );
            }
        }
    }

    // G6: optional Discord notifier. Built once here (fail-fast on a bad webhook,
    // before any socket opens) and shared into whichever arm runs. `None` ⇒ the
    // notifier is OFF: no fire points post, and behaviour is byte-identical to a
    // build without this flag. The URL is validated to an https Discord endpoint
    // here (the only place validation happens); `DiscordNotifier::new` does not
    // re-validate.
    let notifier: Option<Arc<DiscordNotifier>> = match cli.discord_webhook.as_deref() {
        Some(url) => {
            notify::validate_webhook_url(url).map_err(|e| anyhow::anyhow!(e))?;
            tracing::info!(
                "discord: notifications enabled (solutions_only={})",
                cli.discord_solutions_only
            );
            Some(Arc::new(DiscordNotifier::new(
                url.to_string(),
                cli.discord_solutions_only,
            )))
        }
        None => None,
    };

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc_lite(move || {
            tracing::warn!("ctrl-c, shutting down");
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }

    // Resolve the pool endpoint(s): the operator's --url/--pool override(s) if
    // given (validated host:port), else the compiled-in default. The first is
    // the primary we connect to now; the full list will back failover (P1 §3).
    let endpoints = endpoint::resolve_endpoints(&cli.pool, &endpoint::pool_endpoint())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let endpoint = endpoints[0].clone();
    if endpoints.len() > 1 {
        tracing::info!(
            "csd-pool-miner: {} pool endpoints (failover order): {endpoints:?}",
            endpoints.len()
        );
    }
    tracing::info!("csd-pool-miner: connecting to pool {endpoint} as address {address}");
    // Hand the full ordered list to the client so the reader's reconnect path can
    // fail over to a backup pool (and fail back to the primary). With one
    // endpoint this is the same single-pool, no-failover behavior as before.
    let mut client = StratumClient::connect_failover(&endpoints, &address)
        .map_err(|e| anyhow::anyhow!("failed to connect to pool {endpoint}: {e}"))?;

    // G6: wire the Discord notifier into the pool client (the 30s heartbeat posts
    // an accepted-share milestone when the total grows, unless
    // --discord-solutions-only). No-op when --discord-webhook is unset.
    if let Some(n) = &notifier {
        client.attach_notifier(n.clone());
    }

    // D2: optional xmrig-compatible /1/summary telemetry server. Off unless the
    // operator passes --stats-port. It reads live hashrate + health from the
    // shared StatsHandle (the mining loop pushes into it via record_hashrate);
    // it never touches the share/submit/header path. Shares `spawn_stats` with
    // the solo arm.
    let _stats_server = if cli.stats_port.is_some() {
        let handle = Arc::new(StatsHandle::new());
        client.attach_stats(handle.clone());
        Some(spawn_stats(handle, &address, &cli, &stop)?)
    } else {
        None
    };

    drive(&client, &cli, stop)
}

/// Select the backend (cuda → opencl → cpu, honoring `--backend`) and run the
/// shared `run_stratum` loop against `work`. Generic over the [`WorkSource`] so
/// the SAME selection logic drives both the pool `StratumClient` and the solo
/// `NodeWorkSource` — the backend arms are unchanged; only the work-source
/// argument differs from the pre-solo inline version.
fn drive<W: csd_gpu_miner::stratum::loop_stratum::WorkSource>(
    work: &W,
    cli: &Cli,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<()> {
    match cli.backend {
        BackendChoice::Cpu => {
            let n = cpu_hashing_threads(cli);
            let b = CpuBackend::new(n);
            tracing::info!(
                "backend=cpu (forced) hashing_threads={} reserved={}",
                b.threads,
                cli.reserve
            );
            run_stratum(&b, work, stop, build_mining_config(cli, true))
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
            run_stratum(&b, work, stop, build_mining_config(cli, false))
        }
        #[cfg(not(feature = "opencl"))]
        BackendChoice::Opencl => bail!("opencl backend not compiled in (rebuild with --features opencl)"),

        #[cfg(feature = "cuda")]
        BackendChoice::Cuda => {
            tracing::info!(
                "backend=cuda (forced) blocks={} tpb={} npt={} - trying init...",
                cli.blocks, cli.threads_per_block, cli.nonces_per_thread,
            );
            // cudarc can panic (not just return Err) on a low-level driver/context
            // error during init; catch it so we exit with a clear message instead
            // of an unwinding backtrace.
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
                    "cuda init panicked (driver/context error during init); try --backend opencl or --backend cpu"
                ),
            };
            tracing::info!(
                "backend=cuda ready (geom={}x{}x{} = {} nonces/launch, 2-stream pipelined)",
                b.blocks, b.threads_per_block, b.nonces_per_thread,
                (b.blocks as u64) * (b.threads_per_block as u64) * (b.nonces_per_thread as u64),
            );
            run_stratum(&b, work, stop, build_mining_config(cli, false))
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
                // cudarc can panic (rather than return Err) on a low-level driver
                // or context error during init. Catch the panic so `auto` can fall
                // through to OpenCL instead of crashing.
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
                        return run_stratum(&b, work, stop, build_mining_config(cli, false));
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
                        return run_stratum(&b, work, stop, build_mining_config(cli, false));
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

            let n = cpu_hashing_threads(cli);
            let b = CpuBackend::new(n);
            tracing::warn!(
                "auto: SELECTED cpu (no GPU backend usable). hashing_threads={} reserved={}",
                b.threads,
                cli.reserve
            );
            tracing::warn!(
                "auto: no GPU backend was usable — run `csd-gpu-miner devices` (or `--list-devices`) for the probe, or rebuild with `--features cuda` / `--features opencl` to compile GPU support in"
            );
            run_stratum(&b, work, stop, build_mining_config(cli, true))
        }
    }
}

/// Spawn the D2 xmrig-compatible `/1/summary` stats server bound to
/// `cli.stats_bind:cli.stats_port`, serving until `stop` is set. Shared by the
/// pool and solo arms so the bind-parse + spawn happens in exactly one place;
/// the caller builds the [`StatsHandle`], attaches it to its work source
/// (`work.attach_stats(handle.clone())`), then passes the same handle here. The
/// server reads health via a closure over the handle's last-pushed snapshot.
fn spawn_stats(
    handle: Arc<StatsHandle>,
    address: &str,
    cli: &Cli,
    stop: &Arc<AtomicBool>,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    let port = cli.stats_port.expect("spawn_stats called only when stats_port is set");
    let bind: std::net::SocketAddr = format!("{}:{}", cli.stats_bind, port)
        .parse()
        .map_err(|e| {
            anyhow::anyhow!("invalid --stats-bind/--stats-port {}:{port}: {e}", cli.stats_bind)
        })?;
    let server = stats_server::spawn(
        bind,
        handle.clone(),
        Box::new({
            let h = handle.clone();
            move || h.health()
        }),
        address.to_string(),
        cli.stats_password.clone(),
        stop.clone(),
    )
    .map_err(|e| anyhow::anyhow!("failed to bind stats port {bind}: {e}"))?;
    tracing::info!(
        "stats: xmrig /1/summary on http://{bind} (auth={})",
        cli.stats_password.is_some()
    );
    Ok(server)
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
    use super::{validate_address, Cli};
    use clap::Parser;

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
        // Uppercase hex is rejected (addr20 addresses are lowercase hex).
        assert!(validate_address("0123456789ABCDEF0123456789abcdef01234567").is_err());
    }

    // --- P4 device-UX flag plumbing ---------------------------------------

    #[test]
    fn list_devices_flag_parses_and_defaults_off() {
        // Default: the flag is OFF (a no-flags run is byte-identical to pre-P4).
        let cli = Cli::try_parse_from(["csd-pool-miner", "--address", &"a".repeat(40)]).unwrap();
        assert!(!cli.list_devices);
        assert!(cli.gpu_id.is_none());

        // Present: the flag is recognised and set.
        let cli = Cli::try_parse_from(["csd-pool-miner", "--list-devices"]).unwrap();
        assert!(cli.list_devices);
    }

    #[test]
    fn gpu_id_flag_captures_raw_list_for_validation() {
        // clap captures the raw string; main() validates it via parse_gpu_ids.
        let cli =
            Cli::try_parse_from(["csd-pool-miner", "--address", &"a".repeat(40), "--gpu-id", "0,2"])
                .unwrap();
        assert_eq!(cli.gpu_id.as_deref(), Some("0,2"));
        // The shared parser (tested fully in hiveos.rs) turns it into indices and
        // rejects junk — this is the exact call main() makes to fail fast.
        assert_eq!(
            csd_gpu_miner::hiveos::parse_gpu_ids(cli.gpu_id.as_deref().unwrap()).unwrap(),
            vec![0, 2]
        );
        assert!(csd_gpu_miner::hiveos::parse_gpu_ids("0,x").is_err());
    }
}
