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
use csd_gpu_miner::nvml::GpuTelemetry;
use csd_gpu_miner::stratum::StratumClient;
use csd_gpu_miner::stratum::loop_stratum::{
    guarded_suggestion, run_stratum_full, suggested_difficulty, WorkSource,
    POOL_DEFAULT_START_DIFFICULTY,
};
use csd_gpu_miner::thermal::{self, ThermalGate};

#[cfg(feature = "opencl")]
use csd_gpu_miner::backends::opencl::OpenclBackend;

#[cfg(feature = "cuda")]
use csd_gpu_miner::backends::cuda::CudaBackend;

// `#[path]` is pinned so these bin-only modules resolve to `src/` even when
// `main.rs` is `#[path]`-included from an integration test under `tests/`.
#[path = "config_file.rs"]
mod config_file;
#[path = "keygen.rs"]
mod keygen;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "csd-pool-miner",
    version,
    about = "Standalone pool miner for Compute Substrate (mines to the CSD pool)."
)]
pub struct Cli {
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

    /// Optional rig/worker name for per-rig visibility on the pool dashboard.
    /// Appended to the payout address as the Stratum username
    /// ("<address>.<worker>") in `mining.authorize` and every `mining.submit`.
    /// DISPLAY-ONLY: the pool credits the payout address exactly as if it were
    /// bare — the rig name never affects PPLNS/payouts. Sanitized to
    /// [A-Za-z0-9_-], max 24 chars (anything after a '.' is dropped).
    /// Precedence when unset: env `CSD_WORKER` > env `WORKER_NAME` (HiveOS) >
    /// this machine's hostname; if nothing resolves, the bare address is used
    /// (identical to older miners). See also `--no-worker`.
    #[arg(long)]
    worker: Option<String>,

    /// Disable the rig-name suffix entirely (also via env `CSD_NO_WORKER=1`):
    /// authorize as the BARE payout address, exactly like pre-0.2.0 miners.
    /// Use this against a pool bridge that predates rig-name support (an old
    /// bridge rejects a dotted username at authorize).
    #[arg(long, default_value_t = false)]
    no_worker: bool,

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

    /// Auto-tune the GPU launch geometry at startup (CUDA only): briefly
    /// benchmark a small set of (blocks, threads-per-block, nonces-per-thread)
    /// geometries, pick the fastest (MH/s), and use it. The winner is PERSISTED
    /// to the config cache (`<config dir>/csd-pool-miner/autotune.toml`, scoped
    /// to this card) so later starts reuse it WITHOUT re-benchmarking. Explicit
    /// `--blocks/--threads-per-block/--nonces-per-thread` are ignored for the
    /// GPU when this runs (it picks them). Adds the tune time to startup
    /// (≈ candidates × `--auto-tune-secs`).
    #[arg(long, default_value_t = false)]
    auto_tune: bool,

    /// Auto-tune: seconds to benchmark EACH candidate geometry (and the per-run
    /// duration of the `bench` subcommand). Longer = steadier numbers, slower
    /// startup. Default 5.
    #[arg(long, default_value_t = 5)]
    auto_tune_secs: u64,

    /// Target share interval (seconds) used to derive the startup
    /// `mining.suggest_difficulty` hint: after connecting, the miner runs a brief
    /// (≤ a few seconds, hard-capped) hashrate benchmark and suggests the share
    /// difficulty that would yield ~1 share every this-many seconds, so the pool's
    /// vardiff starts near-correct instead of ramping up from the diff-8 floor.
    /// Should match the pool's vardiff TARGET_SHARE_SECS (~30s) so the suggestion
    /// targets steady state rather than the pool's faster initial ramp. The pool
    /// is free to clamp or override it. Default 30.0.
    #[arg(long, default_value_t = 30.0)]
    suggest_share_secs: f64,

    /// Disable the startup `mining.suggest_difficulty` hint (also via env
    /// `CSD_NO_SUGGEST_DIFF=1`). When set, the miner skips the startup benchmark
    /// and lets the pool's vardiff ramp from its floor. The hint is ENABLED by
    /// default; it is purely a client-side Stratum suggestion (no consensus / PoW
    /// / share-validation effect) and never blocks mining.
    #[arg(long, default_value_t = false)]
    no_suggest_diff: bool,

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

    /// Disable the hung-GPU / zero-hashrate watchdog. By default a GPU backend is
    /// monitored: if its hashrate flatlines while fresh jobs flow over a healthy
    /// link (a STALLED GPU, not idle/no-work), the miner first attempts an
    /// in-process CUDA recovery and, failing that, exits with code 17 so a
    /// supervisor restarts it. Pass this to turn that off entirely.
    #[arg(long, default_value_t = false)]
    no_gpu_watchdog: bool,

    /// GPU watchdog: hashrate at/below this (GH/s) counts as "floored" (a dead
    /// kernel). Default 0.001 GH/s = 1 MH/s — orders of magnitude under any
    /// working card, so a slow-but-alive GPU never trips. Raise it only if you
    /// know your card's healthy floor is higher.
    #[arg(long, default_value_t = 0.001)]
    gpu_floor: f64,

    /// GPU watchdog: consecutive floored samples (while jobs flow + link healthy)
    /// before acting — the "dwell". At the ~15s sample cadence the default 4 ⇒
    /// ~60s of continuous zero-with-work before the first recovery attempt.
    #[arg(long, default_value_t = 4)]
    gpu_watchdog_dwell: u32,

    /// GPU watchdog: after a recovery attempt, seconds to wait for hashrate to
    /// return before escalating to a process exit. Default 60.
    #[arg(long, default_value_t = 60)]
    gpu_watchdog_recover_secs: u64,

    /// GPU watchdog: maximum in-process recovery attempts before giving up and
    /// exiting for a supervisor restart. Default 3. 0 ⇒ never attempt recovery,
    /// exit immediately on a confirmed stall.
    #[arg(long, default_value_t = 3)]
    gpu_watchdog_max_recoveries: u32,

    /// (OPTIONAL `nvml` build) Thermal safety: pause GPU launches once the GPU
    /// core temperature rises ABOVE this (°C), and resume once it falls back to
    /// `--temp-resume` (default `limit - 5`). Off unless set. Requires the `nvml`
    /// feature AND an NVIDIA GPU with NVML; on any other build/host it is a no-op
    /// (telemetry is unavailable, so the throttle never engages and the miner
    /// runs exactly as without the flag). This is a SOFT cap layered on top of
    /// the card's own hardware thermal protection.
    #[arg(long)]
    temp_limit: Option<f64>,

    /// (OPTIONAL `nvml` build) Thermal safety: resume GPU launches once the GPU
    /// core temperature falls AT/BELOW this (°C) after a `--temp-limit` pause.
    /// Must be below `--temp-limit` (a non-hysteretic value is corrected down to
    /// `limit - 5` with a warning). Ignored unless `--temp-limit` is set.
    #[arg(long)]
    temp_resume: Option<f64>,

    /// (OPTIONAL `nvml` build) Max-pause dead-man (seconds). If the thermal gate
    /// stays CONTINUOUSLY paused this long, the miner briefly forces GPU launches
    /// back on for one window so the hung-GPU watchdog can adjudicate — guarding
    /// the case where a GPU hangs while NVML keeps reporting it hot (which would
    /// otherwise keep the watchdog suppressed forever). Deliberately long (default
    /// 600 = 10 min) so it NEVER disturbs legitimate sustained throttling: a merely
    /// hot card just re-pauses on the next sample, while a genuinely hung card stays
    /// floored and gets recovered/restarted. Ignored unless `--temp-limit` is set;
    /// a no-op on any non-nvml build / non-NVIDIA host (the gate never pauses there).
    #[arg(long, default_value_t = csd_gpu_miner::thermal::DEFAULT_MAX_PAUSE_SECS)]
    temp_max_pause_secs: u64,

    /// (OPTIONAL `nvml` build) Best-effort GPU board power-limit (Watts) applied
    /// via NVML at startup. Clamped to the card's allowed range; needs elevated
    /// privileges (else it logs a notice and leaves the limit unchanged). No-op
    /// on a non-nvml build / non-NVIDIA host. This sets the DRIVER's enforced
    /// power cap; it is not a mining throttle.
    #[arg(long)]
    power_limit: Option<f64>,

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

    /// (Windows, `winsvc` build only) Register this miner with the Windows
    /// Service Control Manager as an auto-start service, then exit. The service
    /// re-launches this exe in `--run-as-service` mode with the SAME mining flags
    /// you pass here (e.g. `--address`, `--backend`), and auto-restarts on crash.
    /// Run from an elevated (Administrator) console. No-op on non-Windows / a
    /// build without the `winsvc` feature (errors with how to enable it).
    #[arg(long)]
    install_service: bool,

    /// (Windows, `winsvc` build only) Stop (if running) and remove the
    /// `csd-pool-miner` Windows service, then exit. Run elevated.
    #[arg(long)]
    uninstall_service: bool,

    /// (Windows, `winsvc` build only) Run under the Windows Service Control
    /// Manager. This is the mode the SCM invokes for the installed service; you
    /// normally do NOT type it (use `--install-service` then `sc start
    /// csd-pool-miner`). Started by hand it will fail to reach the SCM.
    #[arg(long)]
    run_as_service: bool,

    /// Log directory (rotates previous log on startup).
    #[arg(long, default_value = "logs")]
    log_dir: PathBuf,

    /// Internal (not a CLI flag): set in `main()` to true when the user passed
    /// any of `--blocks` / `--threads-per-block` / `--nonces-per-thread`
    /// explicitly. A persisted auto-tune geometry is only auto-reused when this
    /// is false, so an explicit geometry override always wins over the cache.
    #[arg(skip)]
    geometry_set_explicitly: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

impl Cli {
    /// Resolve which Windows-service action (if any) the parsed flags request.
    /// Thin wrapper over [`csd_gpu_miner::winsvc::parse_service_mode`] so the
    /// flag→mode mapping (and its mutual-exclusion rule) is reachable from
    /// integration tests without exposing the private flag fields.
    pub fn service_mode(&self) -> Result<Option<csd_gpu_miner::winsvc::ServiceMode>> {
        csd_gpu_miner::winsvc::parse_service_mode(
            self.install_service,
            self.uninstall_service,
            self.run_as_service,
        )
    }
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

/// Maximum length (chars) of a sanitized rig/worker name. Keeps the Stratum
/// username bounded; matches the pool-side display cap.
const RIG_NAME_MAX: usize = 24;

/// Sanitize a raw rig/worker-name candidate into a pool-safe rig label:
/// trim whitespace, keep only the part BEFORE the first `.` (so a FQDN
/// hostname like `rig1.local` becomes `rig1` and the label can never inject an
/// extra Stratum-username separator), drop every char outside `[A-Za-z0-9_-]`,
/// and truncate to [`RIG_NAME_MAX`]. Returns `None` when nothing valid
/// survives — the caller then falls back to the bare payout address (today's
/// exact wire behavior), so a weird hostname can NEVER break authorize.
/// Pure (no I/O) and unit-tested.
fn sanitize_rig(raw: &str) -> Option<String> {
    let first = raw.trim().split('.').next().unwrap_or("");
    let cleaned: String = first
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .take(RIG_NAME_MAX)
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Pure precedence resolver: the first candidate (in order) that sanitizes to
/// a valid rig label wins; an absent or unsalvageable candidate falls through
/// to the next. `None` when no candidate survives.
fn resolve_rig_from(candidates: &[Option<&str>]) -> Option<String> {
    candidates.iter().find_map(|c| c.and_then(sanitize_rig))
}

/// I/O wrapper around [`resolve_rig_from`]: resolve the rig name with
/// precedence `--worker` flag > env `CSD_WORKER` > env `WORKER_NAME` (HiveOS)
/// > this machine's hostname. Every candidate goes through [`sanitize_rig`];
/// `None` (no rig anywhere) means "authorize as the bare address".
fn resolve_rig(flag: Option<&str>) -> Option<String> {
    let csd_worker = std::env::var("CSD_WORKER").ok();
    let hive_worker = std::env::var("WORKER_NAME").ok();
    let host = hostname_fallback();
    resolve_rig_from(&[
        flag,
        csd_worker.as_deref(),
        hive_worker.as_deref(),
        host.as_deref(),
    ])
}

/// Best-effort, dependency-free hostname lookup, FAIL-SAFE to `None` (which
/// downstream means "no rig suffix", never an error): env `COMPUTERNAME`
/// (Windows) / `HOSTNAME`, else `/etc/hostname`, else the `hostname` command.
fn hostname_fallback() -> Option<String> {
    for var in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
        let s = s.trim();
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    if let Ok(out) = std::process::Command::new("hostname").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// Build the Stratum username the pool sees: `"<address>.<rig>"`, or the bare
/// address when no rig resolved. PAYOUT INVARIANT (display-only rig): the part
/// before the first `.` is ALWAYS the validated payout address, byte-identical
/// — the pool strips the suffix before any money-path key, so `<addr>.<rig>`
/// credits exactly like a bare `<addr>`. Pure and unit-tested.
fn stratum_username(address: &str, rig: Option<&str>) -> String {
    match rig {
        Some(r) => format!("{address}.{r}"),
        None => address.to_string(),
    }
}

/// Is the rig-name suffix enabled? ON by default; disabled by `--no-worker` OR
/// the env var `CSD_NO_WORKER=1`. Mirrors [`suggest_diff_enabled`] so the
/// kill-switch precedence is obvious and unit-testable.
fn worker_suffix_enabled(cli: &Cli) -> bool {
    if cli.no_worker {
        return false;
    }
    !matches!(
        std::env::var("CSD_NO_WORKER").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes")
    )
}

#[derive(Subcommand, Debug, Clone)]
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

    /// Reproducible GPU benchmark (CUDA): time the SHA-256d kernel and print a
    /// stable MH/s for the device, then exit (does NOT mine, needs no address).
    /// With `--all-geometries` it benchmarks every auto-tune candidate and
    /// prints the table + the winner; otherwise it benchmarks the single active
    /// geometry (`--blocks/--threads-per-block/--nonces-per-thread`, or the
    /// default). Each geometry runs for `--auto-tune-secs`. Useful for
    /// comparing cards / verifying a tune against docs/BASELINES.md. The
    /// benchmark drives the identical mining kernel, so its MH/s reflects real
    /// hashing throughput; it changes no PoW/hash logic.
    Bench {
        /// Benchmark ALL auto-tune candidate geometries (print the full table +
        /// the winner) instead of just the active/default one.
        #[arg(long, default_value_t = false)]
        all_geometries: bool,
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

/// Build the GPU stall-watchdog config from CLI flags. `enabled` is the master
/// switch: `--no-gpu-watchdog` turns it off, and the CPU arm of `drive` also
/// forces it off (a CPU "backend" has no GPU to watch). The recovery window is
/// given in seconds on the CLI and converted to a `Duration` here.
fn build_gpu_watchdog_cfg(cli: &Cli, enabled: bool) -> csd_gpu_miner::gpu_watchdog::GpuWatchdogCfg {
    csd_gpu_miner::gpu_watchdog::GpuWatchdogCfg {
        enabled: enabled && !cli.no_gpu_watchdog,
        floor_ghs: cli.gpu_floor,
        dwell_samples: cli.gpu_watchdog_dwell,
        recover_window: std::time::Duration::from_secs(cli.gpu_watchdog_recover_secs),
        max_recoveries: cli.gpu_watchdog_max_recoveries,
        ..csd_gpu_miner::gpu_watchdog::GpuWatchdogCfg::default()
    }
}

/// Build the thermal-throttle config from CLI flags (`--temp-limit` /
/// `--temp-resume`). `--temp-limit` absent ⇒ disabled (inert). Warns if an
/// explicit `--temp-resume` had to be corrected down to keep hysteresis (the
/// pure [`thermal::build_thermal_cfg`] does the correction). On the default
/// (non-nvml) build the resulting cfg is still built, but the telemetry poller
/// never has a real temperature to feed it, so it never engages.
fn build_thermal_cfg(cli: &Cli) -> thermal::ThermalCfg {
    let cfg = thermal::build_thermal_cfg(cli.temp_limit, cli.temp_resume, Some(cli.temp_max_pause_secs));
    if let (Some(limit), Some(resume)) = (cli.temp_limit, cli.temp_resume) {
        if resume >= limit {
            tracing::warn!(
                "--temp-resume {resume} is not below --temp-limit {limit} (would flap); using resume={:.0}°C instead",
                cfg.resume_c
            );
        }
    }
    cfg
}

/// Spawn the OPTIONAL GPU telemetry + thermal poller (the `nvml` build).
///
/// Brings up [`GpuTelemetry`] for `--device` (degrading to a disabled handle on
/// any non-NVIDIA host / missing NVML / non-nvml build — it NEVER panics and the
/// miner keeps mining), applies an optional `--power-limit`, then every second:
///   1. reads a [`crate::nvml::TelemetrySample`] (temp/power),
///   2. pushes it to the stats handle (if `--stats-port` is on) so `/1/summary`
///      + HiveOS expose `gpu_temp_c`/`gpu_power_w`, and
///   3. drives the shared [`ThermalGate`] via [`thermal::thermal_tick_with_deadman`]
///      using the sample's temperature, so the mining loop pauses/resumes GPU
///      launches (the dead-man bounds a continuous pause with a watchdog check).
///
/// Returns `None` (spawns nothing) when there is NOTHING to do: no thermal limit
/// AND no stats server AND telemetry is disabled — so the default build with no
/// telemetry flags carries zero extra threads. The poll thread exits when `stop`
/// is set; the caller detaches the handle (it owns its `Arc`s).
fn spawn_telemetry(
    cli: &Cli,
    thermal_cfg: thermal::ThermalCfg,
    gate: Arc<ThermalGate>,
    stats: Option<Arc<StatsHandle>>,
    stop: &Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    // Bring NVML up for this device (disabled handle on non-NVIDIA / non-nvml).
    let telem = GpuTelemetry::init(cli.device, cli.power_limit);
    let want_thermal = thermal_cfg.enabled;
    let want_stats = stats.is_some();

    // Nothing to publish AND no throttle to drive AND no live telemetry ⇒ don't
    // spawn a thread at all (the common default-build case).
    if !want_thermal && !want_stats && !telem.is_enabled() {
        return None;
    }
    if want_thermal && !telem.is_enabled() {
        tracing::warn!(
            "--temp-limit set but GPU telemetry is unavailable (no NVIDIA GPU / NVML, or this build lacks the `nvml` feature); the thermal throttle will NOT engage and the miner runs normally. Rebuild with `--features nvml` on an NVIDIA host to enable it."
        );
    }

    let poll = std::time::Duration::from_secs(1);
    let stop = Arc::clone(stop);
    let handle = std::thread::Builder::new()
        .name("gpu-telemetry".to_string())
        .spawn(move || {
            if thermal_cfg.enabled {
                tracing::info!(
                    "thermal: armed (limit={:.0}°C, resume={:.0}°C, poll={:?})",
                    thermal_cfg.limit_c, thermal_cfg.resume_c, poll,
                );
            }
            let slice = std::time::Duration::from_millis(200).min(poll);
            let mut waited = poll; // evaluate once up front (a hot start pauses promptly)
            // Max-pause dead-man: bounds a CONTINUOUS thermal pause so a stuck-hot
            // hung GPU still gets a watchdog check. Inert unless the gate pauses
            // (so a no-op on the default/non-nvml build, where it never does).
            let mut deadman = thermal::Deadman::new();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if waited >= poll {
                    // Time actually elapsed since the previous evaluation (the loop
                    // sleeps in `slice`s and fires the body once `waited >= poll`).
                    let elapsed = waited;
                    waited = std::time::Duration::ZERO;
                    let sample = telem.sample();
                    // Publish to the stats endpoint (no-op if no stats server).
                    if let Some(s) = &stats {
                        s.set_telemetry(sample);
                    }
                    // Drive the thermal gate from the temperature (no-op when the
                    // throttle is disabled — the gate stays Running and the dead-man
                    // only ever sees the "not paused ⇒ reset" branch).
                    if thermal_cfg.enabled {
                        let (state, changed, forced_open) = thermal::thermal_tick_with_deadman(
                            &gate, &mut deadman, sample.temp_c, thermal_cfg, elapsed,
                        );
                        if forced_open {
                            tracing::warn!(
                                "thermal: gate paused >{}s — forcing a watchdog check in case the GPU is hung, not just hot (resuming launches for ~{}s; a healthy throttling card will simply re-pause)",
                                thermal_cfg.max_pause_secs, thermal::FORCE_RESUME_WINDOW_SECS,
                            );
                        }
                        if changed {
                            match state {
                                thermal::ThermalState::Paused => tracing::warn!(
                                    "thermal: PAUSING GPU launches — temp {:.0}°C above limit {:.0}°C (resume at/below {:.0}°C)",
                                    sample.temp_c.unwrap_or(f64::NAN), thermal_cfg.limit_c, thermal_cfg.resume_c,
                                ),
                                thermal::ThermalState::Running => tracing::info!(
                                    "thermal: RESUMING GPU launches — temp back at/below {:.0}°C",
                                    thermal_cfg.resume_c,
                                ),
                            }
                        }
                    }
                }
                std::thread::sleep(slice);
                waited += slice;
            }
        })
        .ok();
    handle
}

/// Resolve the effective GPU launch geometry for CUDA, in precedence order:
///   1. `--auto-tune` ⇒ benchmark the candidate set NOW, use + persist the
///      winner (ignores the CLI geometry flags for the GPU).
///   2. else a PERSISTED geometry cached for THIS exact card (from a prior
///      `--auto-tune`) ⇒ reuse it (no re-bench).
///   3. else the CLI/config geometry (`--blocks/--threads-per-block/
///      --nonces-per-thread`, defaulting to 560x256x4096).
/// Returns `(blocks, threads_per_block, nonces_per_thread)`. Pure-ish: only
/// reads the device name + the cache file; the bench (case 1) is the only heavy
/// path. Logs which source won. Any auto-tune failure degrades to the CLI
/// geometry (never fatal).
#[cfg(feature = "cuda")]
fn resolve_cuda_geometry(cli: &Cli) -> (u32, u32, u32) {
    use csd_gpu_miner::backends::autotune;
    use csd_gpu_miner::backends::cuda;

    if cli.auto_tune {
        match cuda::auto_tune(cli.device, std::time::Duration::from_secs(cli.auto_tune_secs)) {
            Ok(r) => {
                tracing::info!(
                    "auto-tune: using {}x{}x{} ({:.1} MH/s) for device {}",
                    r.best.blocks, r.best.threads_per_block, r.best.nonces_per_thread,
                    r.best.mh_s, cli.device,
                );
                return (r.best.blocks, r.best.threads_per_block, r.best.nonces_per_thread);
            }
            Err(e) => {
                tracing::warn!(
                    "auto-tune failed ({e}); falling back to geometry {}x{}x{}",
                    cli.blocks, cli.threads_per_block, cli.nonces_per_thread,
                );
                return (cli.blocks, cli.threads_per_block, cli.nonces_per_thread);
            }
        }
    }

    // No --auto-tune: reuse a previously-tuned geometry for THIS card if one was
    // persisted (and the same GPU is still at this index), else the CLI default.
    let dev_name = cudarc::driver::CudaContext::new(cli.device)
        .and_then(|ctx| ctx.name())
        .unwrap_or_default();
    if let Some((b, t, n)) = autotune::load_cached_for_device(cli.device, &dev_name) {
        // Only honour the cache when the user did NOT explicitly override the
        // geometry on the CLI (explicit flags always win, matching merge_config).
        if !cli.geometry_set_explicitly {
            tracing::info!(
                "auto-tune: reusing cached geometry {b}x{t}x{n} for device {} ({dev_name}) — pass --auto-tune to re-benchmark",
                cli.device,
            );
            return (b, t, n);
        }
        tracing::info!(
            "auto-tune: a cached geometry exists for device {} but explicit --blocks/--threads-per-block/--nonces-per-thread were given; using those",
            cli.device,
        );
    }
    (cli.blocks, cli.threads_per_block, cli.nonces_per_thread)
}

/// `bench` subcommand: print a stable MH/s for the GPU (or the per-geometry
/// table + winner with `--all-geometries`), then exit. CUDA only; without the
/// `cuda` feature it explains how to enable it. Does not mine.
fn run_bench(cli: &Cli, all_geometries: bool) -> Result<()> {
    #[cfg(feature = "cuda")]
    {
        use csd_gpu_miner::backends::cuda;
        let per = std::time::Duration::from_secs(cli.auto_tune_secs);
        println!("=== csd-gpu-miner bench (device {}) ===", cli.device);
        println!("per-geometry duration: {}s", cli.auto_tune_secs);
        println!();
        if all_geometries {
            // Reuse the auto-tune sweep but DON'T persist for a plain bench: a
            // benchmark should be side-effect-free. auto_tune persists; so here
            // we run the candidate loop directly via benchmark_geometry.
            use csd_gpu_miner::backends::autotune::{candidate_geometries, pick_best, GeometryMeasurement};
            let mut ms: Vec<GeometryMeasurement> = Vec::new();
            for (b, t, n) in candidate_geometries() {
                match cuda::benchmark_geometry(cli.device, b, t, n, per) {
                    Ok(mh) => {
                        println!("  {b:>5} x {t:>4} x {n:>5}  =  {mh:>8.1} MH/s");
                        ms.push(GeometryMeasurement::new(b, t, n, mh));
                    }
                    Err(e) => {
                        println!("  {b:>5} x {t:>4} x {n:>5}  =  FAILED ({e})");
                        ms.push(GeometryMeasurement::new(b, t, n, 0.0));
                    }
                }
            }
            println!();
            match pick_best(&ms) {
                Some(best) => println!(
                    "WINNER: {}x{}x{} at {:.1} MH/s",
                    best.blocks, best.threads_per_block, best.nonces_per_thread, best.mh_s
                ),
                None => {
                    anyhow::bail!("bench: no geometry produced a usable measurement");
                }
            }
        } else {
            let (b, t, n) = (cli.blocks, cli.threads_per_block, cli.nonces_per_thread);
            let mh = cuda::benchmark_geometry(cli.device, b, t, n, per)
                .map_err(|e| anyhow::anyhow!("bench: {e}"))?;
            println!("geometry {b}x{t}x{n}: {mh:.1} MH/s");
        }
        return Ok(());
    }
    #[cfg(not(feature = "cuda"))]
    {
        let _ = (cli, all_geometries);
        bail!("bench requires the CUDA backend: rebuild with `cargo build --release --features cuda`");
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
    // `worker` has no clap default (like `address`), so the file value fills it
    // only when the flag is absent.
    if cli.worker.is_none() {
        cli.worker = file.worker;
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

    // Record whether the user explicitly set any GPU geometry flag on the CLI
    // (used by the CUDA geometry resolver so an explicit override beats a
    // persisted auto-tune geometry). Mirrors merge_config's precedence check.
    {
        use clap::parser::ValueSource;
        let explicit = |id: &str| matches.value_source(id) == Some(ValueSource::CommandLine);
        cli.geometry_set_explicitly =
            explicit("blocks") || explicit("threads_per_block") || explicit("nonces_per_thread");
    }

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

    if let Some(Cmd::Bench { all_geometries }) = cli.cmd {
        return run_bench(&cli, all_geometries);
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
        // The pool endpoint is compiled in and cannot be overridden.
        let endpoints = vec![endpoint::pool_endpoint()];
        csd_gpu_miner::selftest::print_reachability(&endpoints);
        return result;
    }

    // Windows-service modes (OPTIONAL `winsvc` build). `--install-service` /
    // `--uninstall-service` talk to the SCM and exit; `--run-as-service` hands
    // the mining body to the SCM dispatcher (which supplies the stop flag its
    // Stop/Shutdown handler flips). All three are off in the default build and
    // a no-op on non-Windows (a clear error). Resolved here, before mining
    // setup, so the install/uninstall paths never open a socket.
    match cli.service_mode()? {
        Some(csd_gpu_miner::winsvc::ServiceMode::Run) => {
            // Mine under the SCM: it owns the stop flag, so we do NOT install the
            // Ctrl-C handler here (`install_ctrlc = false`).
            let cli_for_service = cli.clone();
            return csd_gpu_miner::winsvc::run_service_action(
                csd_gpu_miner::winsvc::ServiceMode::Run,
                Box::new(move |stop| run_mining(&cli_for_service, stop, false)),
            );
        }
        Some(mode) => {
            // Install / uninstall: no mining body needed (the closure is never
            // invoked for these modes), so hand a clearly-unreachable one.
            return csd_gpu_miner::winsvc::run_service_action(
                mode,
                Box::new(|_stop| {
                    unreachable!("install/uninstall do not run the miner body")
                }),
            );
        }
        None => {}
    }

    // Normal foreground mining: own the stop flag + install the Ctrl-C handler.
    let stop = Arc::new(AtomicBool::new(false));
    run_mining(&cli, stop, true)
}

/// Validate inputs, connect to the pool, optionally start telemetry, and drive
/// the mining loop until `stop` is set. Shared by the normal foreground path and
/// the Windows-service `--run-as-service` path; the only difference is who owns
/// the stop flag and whether the Ctrl-C handler is installed (`install_ctrlc`).
fn run_mining(
    cli: &Cli,
    stop: Arc<AtomicBool>,
    install_ctrlc: bool,
) -> Result<()> {
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

    // Install the Ctrl-C handler only for the foreground path; under the Windows
    // SCM the service's Stop/Shutdown handler owns the same `stop` flag instead.
    if install_ctrlc {
        let stop = stop.clone();
        ctrlc_lite(move || {
            tracing::warn!("ctrl-c, shutting down");
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }

    // Resolve the optional rig/worker name and build the Stratum username the
    // pool sees. This is the SINGLE injection point: the same string flows into
    // the authorize handshake, every mining.submit (via `worker_addr()`), every
    // reconnect re-handshake, and the stats `worker_id` — so rig identity can
    // never diverge between them. The rig label is DISPLAY-ONLY: `address`
    // (the validated bare payout address) is what the pool credits.
    //
    // ⚠ DEPLOY ORDER (BRIDGE-FIRST): a pool bridge that predates rig-name
    // support validates the WHOLE authorize username as a 40-hex address and
    // REJECTS "<addr>.<rig>" (error 24) — the miner would then never mine. The
    // rig-suffix-aware bridge MUST be deployed before this miner is released
    // to the fleet. Per-rig off-switch: `--no-worker` / `CSD_NO_WORKER=1`.
    let rig = if worker_suffix_enabled(cli) {
        resolve_rig(cli.worker.as_deref())
    } else {
        tracing::info!(
            "worker: rig-name suffix disabled (--no-worker / CSD_NO_WORKER=1); authorizing as the bare address"
        );
        None
    };
    let stratum_user = stratum_username(&address, rig.as_deref());

    // The pool endpoint is compiled in and cannot be overridden: the shipped
    // binary always mines to the official pool. `connect_failover` still takes a
    // list, so we hand it the single baked-in endpoint.
    let endpoints = vec![endpoint::pool_endpoint()];
    let endpoint = endpoints[0].clone();
    tracing::info!(
        "csd-pool-miner: connecting to pool {endpoint} as address {address} (worker: {})",
        rig.as_deref().unwrap_or("<none — bare address>")
    );
    // Hand the full ordered list to the client so the reader's reconnect path can
    // fail over to a backup pool (and fail back to the primary). With one
    // endpoint this is the same single-pool, no-failover behavior as before.
    let mut client = StratumClient::connect_failover(&endpoints, &stratum_user)
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
    // it never touches the share/submit/header path. The handle is kept so the
    // OPTIONAL GPU telemetry poller can push gpu_temp_c/gpu_power_w into it too.
    let stats_handle: Option<Arc<StatsHandle>> = if cli.stats_port.is_some() {
        let handle = Arc::new(StatsHandle::new());
        client.attach_stats(handle.clone());
        Some(handle)
    } else {
        None
    };
    // The stats `worker_id` mirrors the Stratum username (address.rig when a
    // rig resolved), so dashboards see the same identity the pool sees. The
    // HiveOS h-stats mapping does not key on worker_id, and csd-dashboard only
    // displays it.
    let _stats_server = match &stats_handle {
        Some(handle) => Some(spawn_stats(handle.clone(), &stratum_user, cli, &stop)?),
        None => None,
    };

    // OPTIONAL (nvml build) GPU telemetry + thermal throttle. The gate is shared
    // into the mining loop; the poller drives it from NVML temperature and also
    // feeds gpu_temp_c/gpu_power_w to the stats endpoint. On the default build
    // (or a non-NVIDIA host) telemetry is disabled: the gate is never paused, the
    // poller may not even spawn, and the loop runs exactly as the fleet build.
    let thermal_cfg = build_thermal_cfg(cli);
    let thermal_gate = Arc::new(ThermalGate::new());
    let _telemetry = spawn_telemetry(
        cli,
        thermal_cfg,
        Arc::clone(&thermal_gate),
        stats_handle.clone(),
        &stop,
    );

    drive(&client, cli, stop, thermal_gate)
}

/// Select the backend (cuda → opencl → cpu, honoring `--backend`) and run the
/// shared `run_stratum` loop against `work`. Generic over the [`WorkSource`] so
/// the same selection logic drives the pool `StratumClient` (and the test mock);
/// the backend arms are independent of the work-source argument.
fn drive<W: csd_gpu_miner::stratum::loop_stratum::WorkSource + Sync>(
    work: &W,
    cli: &Cli,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thermal_gate: Arc<ThermalGate>,
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
            // CPU backend has no GPU to watch ⇒ watchdog disabled.
            maybe_suggest_difficulty(work, cli, &b, &stop);
            run_stratum_full(
                &b,
                work,
                stop,
                build_mining_config(cli, true),
                build_gpu_watchdog_cfg(cli, false),
                Arc::clone(&thermal_gate),
            )
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
            maybe_suggest_difficulty(work, cli, &b, &stop);
            run_stratum_full(
                &b,
                work,
                stop,
                build_mining_config(cli, false),
                build_gpu_watchdog_cfg(cli, true),
                Arc::clone(&thermal_gate),
            )
        }
        #[cfg(not(feature = "opencl"))]
        BackendChoice::Opencl => bail!("opencl backend not compiled in (rebuild with --features opencl)"),

        #[cfg(feature = "cuda")]
        BackendChoice::Cuda => {
            // Resolve geometry: --auto-tune (bench+persist) > cached-for-card >
            // CLI/default. Done before init so the chosen geometry is what we
            // build the backend with.
            let (g_blocks, g_tpb, g_npt) = resolve_cuda_geometry(cli);
            tracing::info!(
                "backend=cuda (forced) blocks={} tpb={} npt={} - trying init...",
                g_blocks, g_tpb, g_npt,
            );
            // cudarc can panic (not just return Err) on a low-level driver/context
            // error during init; catch it so we exit with a clear message instead
            // of an unwinding backtrace.
            let init = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                CudaBackend::new(cli.device, g_blocks, g_tpb, g_npt)
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
            maybe_suggest_difficulty(work, cli, &b, &stop);
            run_stratum_full(
                &b,
                work,
                stop,
                build_mining_config(cli, false),
                build_gpu_watchdog_cfg(cli, true),
                Arc::clone(&thermal_gate),
            )
        }
        #[cfg(not(feature = "cuda"))]
        BackendChoice::Cuda => bail!("cuda backend not compiled in (rebuild with --features cuda)"),

        BackendChoice::Auto => {
            tracing::info!("backend=auto - probing in order: cuda -> opencl -> cpu");

            #[cfg(feature = "cuda")]
            {
                // Resolve geometry first (--auto-tune / cached / default). The
                // bench itself opens the device, so a card that can't init will
                // surface here; auto_tune degrades to the default on failure.
                let (g_blocks, g_tpb, g_npt) = resolve_cuda_geometry(cli);
                tracing::info!("auto: trying CUDA geom={}x{}x{}", g_blocks, g_tpb, g_npt);
                // cudarc can panic (rather than return Err) on a low-level driver
                // or context error during init. Catch the panic so `auto` can fall
                // through to OpenCL instead of crashing.
                let cuda_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    CudaBackend::new(cli.device, g_blocks, g_tpb, g_npt)
                }));
                match cuda_result {
                    Ok(Ok(b)) => {
                        tracing::info!(
                            "auto: SELECTED cuda (geom={}x{}x{} = {} nonces/launch, 2-stream pipelined)",
                            b.blocks, b.threads_per_block, b.nonces_per_thread,
                            (b.blocks as u64) * (b.threads_per_block as u64) * (b.nonces_per_thread as u64),
                        );
                        maybe_suggest_difficulty(work, cli, &b, &stop);
                        return run_stratum_full(
                            &b,
                            work,
                            stop,
                            build_mining_config(cli, false),
                            build_gpu_watchdog_cfg(cli, true),
                            Arc::clone(&thermal_gate),
                        );
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
                        maybe_suggest_difficulty(work, cli, &b, &stop);
                        return run_stratum_full(
                            &b,
                            work,
                            stop,
                            build_mining_config(cli, false),
                            build_gpu_watchdog_cfg(cli, true),
                            Arc::clone(&thermal_gate),
                        );
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
            // Fell back to CPU ⇒ no GPU to watch ⇒ watchdog disabled.
            maybe_suggest_difficulty(work, cli, &b, &stop);
            run_stratum_full(
                &b,
                work,
                stop,
                build_mining_config(cli, true),
                build_gpu_watchdog_cfg(cli, false),
                Arc::clone(&thermal_gate),
            )
        }
    }
}

/// Is the startup `mining.suggest_difficulty` hint enabled? ON by default;
/// disabled by `--no-suggest-diff` OR the env var `CSD_NO_SUGGEST_DIFF=1`. Pure
/// (env read aside) so the precedence is obvious and unit-testable.
fn suggest_diff_enabled(cli: &Cli) -> bool {
    if cli.no_suggest_diff {
        return false;
    }
    !matches!(
        std::env::var("CSD_NO_SUGGEST_DIFF").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes")
    )
}

/// Run the bounded startup hashrate benchmark against the SELECTED backend and,
/// if it yields a usable rate, derive a Stratum share difficulty and send the
/// pool a `mining.suggest_difficulty` hint (cached so it re-sends on reconnect).
///
/// HARD BRICK-SAFETY (no-clawback fleet rule): this can NEVER prevent mining.
/// It is gated off by `--no-suggest-diff` / `CSD_NO_SUGGEST_DIFF`; the benchmark
/// is time-bounded and panic-safe (returns `None` on any failure); and on `None`
/// — or a non-positive rate — it logs one info line and returns, leaving the
/// miner to start normally and let the pool's vardiff ramp from its floor. A
/// wrong-but-finite suggestion is harmless: the pool clamps it to its band and
/// vardiff overrides it within a few shares. No consensus / PoW / share path is
/// touched — this is a client-side Stratum suggestion only.
fn maybe_suggest_difficulty<W, B>(work: &W, cli: &Cli, backend: &B, stop: &Arc<AtomicBool>)
where
    W: WorkSource + Sync,
    B: csd_gpu_miner::backend::MiningBackend,
{
    if !suggest_diff_enabled(cli) {
        tracing::info!(
            "suggest-diff: disabled (--no-suggest-diff / CSD_NO_SUGGEST_DIFF); skipping"
        );
        return;
    }
    let budget = csd_gpu_miner::bench::DEFAULT_BENCH_BUDGET;
    tracing::info!(
        "suggest-diff: benchmarking {} for ~{:?} to derive a starting difficulty…",
        backend.name(),
        budget,
    );
    match csd_gpu_miner::bench::benchmark_hashrate(backend, budget, stop) {
        Some(hps) if hps > 0.0 => {
            let d = suggested_difficulty(hps, cli.suggest_share_secs);
            // Only forward a suggestion that BEATS the pool's vardiff start floor.
            // The v0.1.15 bug forwarded an under-reported diff (e.g. 1.0) that was
            // BELOW the pool's diff-8 start — actively slowing a fast rig. Gate it.
            match guarded_suggestion(d, POOL_DEFAULT_START_DIFFICULTY) {
                Some(d) => {
                    tracing::info!(
                        "suggest-diff: measured {:.2} MH/s ⇒ suggesting difficulty {:.2} (target ~{:.0}s/share)",
                        hps / 1e6,
                        d,
                        cli.suggest_share_secs,
                    );
                    // Cache + send (best-effort; logged by the impl). Re-sent on reconnect.
                    work.apply_suggest_difficulty(d);
                }
                None => {
                    tracing::info!(
                        "suggest-diff: derived diff <= pool default; skipping (mine normally)"
                    );
                }
            }
        }
        _ => {
            // Benchmark failed / timed out / returned no usable rate. Mine anyway.
            tracing::info!(
                "suggest-diff: benchmark produced no usable rate; skipping suggest (mining normally)"
            );
        }
    }
}

/// Spawn the D2 xmrig-compatible `/1/summary` stats server bound to
/// `cli.stats_bind:cli.stats_port`, serving until `stop` is set. Factored out so
/// the bind-parse + spawn happens in exactly one place; the caller builds the
/// [`StatsHandle`], attaches it to its work source
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
    if let Err(e) = ctrlc::set_handler(handler) {
        tracing::warn!("could not install ctrl-c handler ({e}); Ctrl-C will hard-stop");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_gpu_watchdog_cfg, build_mining_config, num_cpus_default, resolve_rig_from,
        sanitize_rig, stratum_username, suggest_diff_enabled, validate_address,
        worker_suffix_enabled, Cli,
    };
    use clap::Parser;
    use std::sync::Mutex;

    /// Serializes the two tests that mutate the process-global `CSD_NO_SUGGEST_DIFF`
    /// env var, so they can't race each other under parallel test execution.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn suggest_diff_flags_parse_with_sane_defaults() {
        // Default run: hint is ENABLED, kill-switch off, and the target share
        // interval defaults to 30s — matching the pool's vardiff TARGET_SHARE_SECS
        // so the suggestion targets steady state, not the pool's faster ramp.
        let cli = Cli::try_parse_from(["csd-pool-miner", "--address", &"a".repeat(40)]).unwrap();
        assert!(!cli.no_suggest_diff, "suggest-diff is ON by default");
        assert_eq!(cli.suggest_share_secs, 30.0);

        // --no-suggest-diff flips the kill switch; --suggest-share-secs overrides T.
        let cli = Cli::try_parse_from([
            "csd-pool-miner",
            "--address",
            &"a".repeat(40),
            "--no-suggest-diff",
            "--suggest-share-secs",
            "45",
        ])
        .unwrap();
        assert!(cli.no_suggest_diff);
        assert_eq!(cli.suggest_share_secs, 45.0);
    }

    #[test]
    fn suggest_diff_enabled_default_on_flag_off() {
        let _guard = ENV_LOCK.lock().unwrap();
        // No flag, no env override ⇒ enabled. (Guard: clear the env var first so a
        // stray value in the test environment can't flip the default.)
        std::env::remove_var("CSD_NO_SUGGEST_DIFF");
        let cli = Cli::try_parse_from(["csd-pool-miner", "--address", &"a".repeat(40)]).unwrap();
        assert!(suggest_diff_enabled(&cli), "default must be ENABLED");

        // The --no-suggest-diff flag disables it regardless of env.
        let cli = Cli::try_parse_from([
            "csd-pool-miner",
            "--address",
            &"a".repeat(40),
            "--no-suggest-diff",
        ])
        .unwrap();
        assert!(
            !suggest_diff_enabled(&cli),
            "--no-suggest-diff must disable"
        );
    }

    #[test]
    fn suggest_diff_enabled_env_kill_switch() {
        let _guard = ENV_LOCK.lock().unwrap();
        // CSD_NO_SUGGEST_DIFF=1 disables even without the flag; clearing it
        // re-enables. Set + restore around the assertions (process-global env).
        let prev = std::env::var("CSD_NO_SUGGEST_DIFF").ok();
        let cli = Cli::try_parse_from(["csd-pool-miner", "--address", &"a".repeat(40)]).unwrap();

        std::env::set_var("CSD_NO_SUGGEST_DIFF", "1");
        assert!(!suggest_diff_enabled(&cli), "env=1 must disable");

        std::env::set_var("CSD_NO_SUGGEST_DIFF", "0");
        assert!(suggest_diff_enabled(&cli), "env=0 must NOT disable");

        std::env::remove_var("CSD_NO_SUGGEST_DIFF");
        assert!(suggest_diff_enabled(&cli), "no env var ⇒ enabled");

        // Restore whatever was there before so we don't leak into sibling tests.
        match prev {
            Some(v) => std::env::set_var("CSD_NO_SUGGEST_DIFF", v),
            None => std::env::remove_var("CSD_NO_SUGGEST_DIFF"),
        }
    }

    // --- worker/rig name (mining.authorize "<address>.<rig>") ----------------

    #[test]
    fn sanitize_rig_keeps_simple_names_verbatim() {
        assert_eq!(sanitize_rig("rig1").as_deref(), Some("rig1"));
        assert_eq!(sanitize_rig("GPU-box_02").as_deref(), Some("GPU-box_02"));
    }

    #[test]
    fn sanitize_rig_takes_part_before_first_dot() {
        // A FQDN-ish hostname must never inject extra '.' separators into the
        // Stratum username (the pool splits the payout address on the FIRST
        // '.', so the rig label itself must be dot-free).
        assert_eq!(sanitize_rig("rig1.local").as_deref(), Some("rig1"));
        assert_eq!(sanitize_rig("a.b.c").as_deref(), Some("a"));
    }

    #[test]
    fn sanitize_rig_filters_to_allowed_charset() {
        // Anything outside [A-Za-z0-9_-] is dropped, never substituted.
        assert_eq!(sanitize_rig("my rig!#7").as_deref(), Some("myrig7"));
        assert_eq!(sanitize_rig("kjøkken-rig").as_deref(), Some("kjkken-rig"));
        // Whitespace is trimmed before the first-dot split.
        assert_eq!(sanitize_rig("  rig2  ").as_deref(), Some("rig2"));
    }

    #[test]
    fn sanitize_rig_truncates_to_24_chars() {
        let long = "a".repeat(40);
        let got = sanitize_rig(&long).unwrap();
        assert_eq!(got.len(), 24);
        assert_eq!(got, "a".repeat(24));
    }

    #[test]
    fn sanitize_rig_nothing_valid_is_none() {
        // An authorize-breaking candidate must resolve to None (bare-address
        // fallback = today's exact behavior), never to an empty/garbage label.
        assert_eq!(sanitize_rig(""), None);
        assert_eq!(sanitize_rig("   "), None);
        assert_eq!(sanitize_rig("..."), None);
        assert_eq!(sanitize_rig(".rig"), None); // empty before the first dot
        assert_eq!(sanitize_rig("!!!"), None);
    }

    #[test]
    fn resolve_rig_from_first_valid_candidate_wins() {
        // Precedence list is ordered: --worker flag > CSD_WORKER > WORKER_NAME
        // > hostname. The first candidate that SANITIZES to something wins.
        assert_eq!(
            resolve_rig_from(&[Some("flagrig"), Some("envrig"), Some("host")]).as_deref(),
            Some("flagrig")
        );
        // An all-invalid candidate falls through to the next one.
        assert_eq!(
            resolve_rig_from(&[Some("..."), Some("envrig")]).as_deref(),
            Some("envrig")
        );
        // Absent candidates are skipped.
        assert_eq!(
            resolve_rig_from(&[None, None, Some("host.domain")]).as_deref(),
            Some("host")
        );
        // Nothing valid anywhere -> None (bare address).
        assert_eq!(resolve_rig_from(&[None, Some("!!!"), None]), None);
        assert_eq!(resolve_rig_from(&[]), None);
    }

    #[test]
    fn stratum_username_appends_rig_after_a_dot() {
        let addr = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            stratum_username(addr, Some("rig1")),
            format!("{addr}.rig1")
        );
    }

    #[test]
    fn stratum_username_without_rig_is_byte_identical_to_address() {
        // MONEY-PATH INVARIANT: with no rig the username IS the bare validated
        // payout address, byte-identical — exactly what pre-0.2.0 miners send.
        let addr = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(stratum_username(addr, None), addr);
    }

    #[test]
    fn stratum_username_payout_address_recoverable_before_first_dot() {
        // MONEY-PATH INVARIANT: whatever rig label is appended, splitting the
        // username on the FIRST '.' must recover the payout address
        // byte-identically — "<addr>.<rig>" must credit exactly like "<addr>".
        let addr = "0123456789abcdef0123456789abcdef01234567";
        for rig in [None, Some("rig1"), Some("a-b_C9")] {
            let user = stratum_username(addr, rig);
            assert_eq!(user.split('.').next().unwrap(), addr);
        }
        // And a sanitized rig can never smuggle a '.' in (sanitize_rig strips
        // at the first dot), so the split is unambiguous.
        assert!(!sanitize_rig("rig.local").unwrap().contains('.'));
    }

    #[test]
    fn worker_flags_parse_with_sane_defaults() {
        // Default run: no --worker (rig resolution falls to env/hostname),
        // suffix enabled (kill-switch off).
        let cli = Cli::try_parse_from(["csd-pool-miner", "--address", &"a".repeat(40)]).unwrap();
        assert_eq!(cli.worker, None);
        assert!(!cli.no_worker, "rig-name suffix is ON by default");

        // --worker names the rig; --no-worker flips the kill switch.
        let cli = Cli::try_parse_from([
            "csd-pool-miner",
            "--address",
            &"a".repeat(40),
            "--worker",
            "rig1",
            "--no-worker",
        ])
        .unwrap();
        assert_eq!(cli.worker.as_deref(), Some("rig1"));
        assert!(cli.no_worker);
    }

    #[test]
    fn worker_suffix_enabled_default_on_flag_off() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("CSD_NO_WORKER");
        let cli = Cli::try_parse_from(["csd-pool-miner", "--address", &"a".repeat(40)]).unwrap();
        assert!(worker_suffix_enabled(&cli), "default must be ENABLED");

        let cli = Cli::try_parse_from([
            "csd-pool-miner",
            "--address",
            &"a".repeat(40),
            "--no-worker",
        ])
        .unwrap();
        assert!(!worker_suffix_enabled(&cli), "--no-worker must disable");
    }

    #[test]
    fn worker_suffix_enabled_env_kill_switch() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("CSD_NO_WORKER").ok();
        let cli = Cli::try_parse_from(["csd-pool-miner", "--address", &"a".repeat(40)]).unwrap();

        std::env::set_var("CSD_NO_WORKER", "1");
        assert!(!worker_suffix_enabled(&cli), "env=1 must disable");

        std::env::set_var("CSD_NO_WORKER", "0");
        assert!(worker_suffix_enabled(&cli), "env=0 must NOT disable");

        std::env::remove_var("CSD_NO_WORKER");
        assert!(worker_suffix_enabled(&cli), "no env var => enabled");

        match prev {
            Some(v) => std::env::set_var("CSD_NO_WORKER", v),
            None => std::env::remove_var("CSD_NO_WORKER"),
        }
    }

    #[test]
    fn merge_config_worker_file_fills_only_when_flag_absent() {
        use clap::{CommandFactory, FromArgMatches};

        // No --worker on the CLI -> the config-file value fills it.
        let matches = Cli::command()
            .get_matches_from(["csd-pool-miner", "--address", &"a".repeat(40)]);
        let mut cli = Cli::from_arg_matches(&matches).unwrap();
        let file = super::config_file::FileConfig {
            worker: Some("filerig".to_string()),
            ..Default::default()
        };
        super::merge_config(&mut cli, &matches, file);
        assert_eq!(cli.worker.as_deref(), Some("filerig"));

        // --worker on the CLI wins over the config file (CLI > file > default).
        let matches = Cli::command().get_matches_from([
            "csd-pool-miner",
            "--address",
            &"a".repeat(40),
            "--worker",
            "flagrig",
        ]);
        let mut cli = Cli::from_arg_matches(&matches).unwrap();
        let file = super::config_file::FileConfig {
            worker: Some("filerig".to_string()),
            ..Default::default()
        };
        super::merge_config(&mut cli, &matches, file);
        assert_eq!(cli.worker.as_deref(), Some("flagrig"));
    }

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

    // --- backend→config mapping (build_mining_config / build_gpu_watchdog_cfg) ---
    // Pins the safety-relevant CLI→config mapping so the supervised dedup of
    // drive()'s six launch tails can't silently swap a bool (e.g. a GPU watchdog
    // wired onto a CPU rig, or a second CPU pool contending inside the CPU loop).
    // Call-site truth being characterized (main.rs drive()):
    //   CPU arms pass (backend_is_cpu=true,  watchdog_enabled=false)
    //   GPU arms pass (backend_is_cpu=false, watchdog_enabled=true)

    #[test]
    fn build_mining_config_cpu_backend_always_zeroes_dual_pool() {
        // The CPU backend saturates its own hashing threads, so the in-loop dual
        // pool is deliberately zeroed regardless of --cpu-threads / --cpu-share.
        let cli = Cli::try_parse_from([
            "csd-pool-miner",
            "--address",
            &"a".repeat(40),
            "--cpu-threads",
            "12",
            "--cpu-share",
            "0.9",
        ])
        .unwrap();
        let cfg = build_mining_config(&cli, true);
        assert_eq!(cfg.cpu_threads, 0, "CPU backend must zero the dual pool threads");
        assert_eq!(cfg.cpu_share, 0.0, "CPU backend must zero the dual pool share");

        // Even the defaults path (no explicit cpu flags) zeroes under the CPU arm.
        let cli_default =
            Cli::try_parse_from(["csd-pool-miner", "--address", &"a".repeat(40)]).unwrap();
        let cfg_default = build_mining_config(&cli_default, true);
        assert_eq!(cfg_default.cpu_threads, 0);
        assert_eq!(cfg_default.cpu_share, 0.0);
    }

    #[test]
    fn build_mining_config_gpu_backend_carries_clamped_cli_values() {
        // 2 threads is <= any real core count, so it passes through unchanged;
        // 0.25 is in-range, so it survives the clamp verbatim. This pins the
        // "carry the operator's dual-mining knobs" path for the GPU arms.
        let cli = Cli::try_parse_from([
            "csd-pool-miner",
            "--address",
            &"a".repeat(40),
            "--cpu-threads",
            "2",
            "--cpu-share",
            "0.25",
        ])
        .unwrap();
        let cfg = build_mining_config(&cli, false);
        assert_eq!(cfg.cpu_threads, 2, "in-range threads pass through under a GPU backend");
        assert_eq!(cfg.cpu_share, 0.25, "in-range share passes through under a GPU backend");

        // cpu_threads is clamped to the machine's core count (num_cpus_default),
        // asserted via the exact formula rather than a hardcoded core count so the
        // test is machine-independent.
        let cli_big = Cli::try_parse_from([
            "csd-pool-miner",
            "--address",
            &"a".repeat(40),
            "--cpu-threads",
            "100000",
        ])
        .unwrap();
        let cfg_big = build_mining_config(&cli_big, false);
        assert_eq!(cfg_big.cpu_threads, cli_big.cpu_threads.min(num_cpus_default()));
        assert!(cfg_big.cpu_threads <= num_cpus_default());

        // cpu_share is clamped to 0.0..=1.0. `=`-form avoids clap reading the
        // leading '-' of a negative value as a flag.
        let cli_hi = Cli::try_parse_from([
            "csd-pool-miner",
            "--address",
            &"a".repeat(40),
            "--cpu-share",
            "2.0",
        ])
        .unwrap();
        assert_eq!(build_mining_config(&cli_hi, false).cpu_share, 1.0, "share clamps up-bound to 1.0");

        let cli_lo = Cli::try_parse_from([
            "csd-pool-miner",
            "--address",
            &"a".repeat(40),
            "--cpu-share=-0.5",
        ])
        .unwrap();
        assert_eq!(build_mining_config(&cli_lo, false).cpu_share, 0.0, "share clamps low-bound to 0.0");
    }

    #[test]
    fn build_gpu_watchdog_cfg_disabled_arm_is_always_off() {
        // The CPU arm passes enabled=false; the watchdog must be off regardless of
        // --no-gpu-watchdog (a CPU backend has no GPU to watch).
        let cli = Cli::try_parse_from(["csd-pool-miner", "--address", &"a".repeat(40)]).unwrap();
        assert!(!build_gpu_watchdog_cfg(&cli, false).enabled);

        let cli_nowd = Cli::try_parse_from([
            "csd-pool-miner",
            "--address",
            &"a".repeat(40),
            "--no-gpu-watchdog",
        ])
        .unwrap();
        assert!(!build_gpu_watchdog_cfg(&cli_nowd, false).enabled);
    }

    #[test]
    fn build_gpu_watchdog_cfg_enabled_arm_honors_flag_and_maps_knobs() {
        use std::time::Duration;

        // GPU arm passes enabled=true; the master switch is then !--no-gpu-watchdog.
        let cli = Cli::try_parse_from(["csd-pool-miner", "--address", &"a".repeat(40)]).unwrap();
        let cfg = build_gpu_watchdog_cfg(&cli, true);
        assert!(cfg.enabled, "GPU arm + no --no-gpu-watchdog ⇒ enabled");
        // Defaults map straight from the CLI defaults.
        assert_eq!(cfg.floor_ghs, 0.001);
        assert_eq!(cfg.dwell_samples, 4);
        assert_eq!(cfg.recover_window, Duration::from_secs(60));
        assert_eq!(cfg.max_recoveries, 3);

        // --no-gpu-watchdog forces enabled=false even on the GPU arm.
        let cli_off = Cli::try_parse_from([
            "csd-pool-miner",
            "--address",
            &"a".repeat(40),
            "--no-gpu-watchdog",
        ])
        .unwrap();
        assert!(!build_gpu_watchdog_cfg(&cli_off, true).enabled);

        // The tuning knobs map from their flags (floor/dwell/recover-window/max).
        let cli_tuned = Cli::try_parse_from([
            "csd-pool-miner",
            "--address",
            &"a".repeat(40),
            "--gpu-floor",
            "0.5",
            "--gpu-watchdog-dwell",
            "7",
            "--gpu-watchdog-recover-secs",
            "120",
            "--gpu-watchdog-max-recoveries",
            "9",
        ])
        .unwrap();
        let cfg_tuned = build_gpu_watchdog_cfg(&cli_tuned, true);
        assert!(cfg_tuned.enabled);
        assert_eq!(cfg_tuned.floor_ghs, 0.5);
        assert_eq!(cfg_tuned.dwell_samples, 7);
        assert_eq!(cfg_tuned.recover_window, Duration::from_secs(120));
        assert_eq!(cfg_tuned.max_recoveries, 9);
    }
}
