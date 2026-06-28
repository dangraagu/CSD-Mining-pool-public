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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::backend::{MiningBackend, MiningResult};
use crate::coinbase::{coinbase_txid, header_84, merkle_root_from_branch};
use crate::gpu_watchdog::{
    gpu_watchdog_tick, GpuWatchdogCfg, GpuWatchdogState, GpuWatchdogView, Recoverable,
};
use crate::mining_config::{partition_nonce_range, MiningConfig};
use crate::sha256d_cpu::{finish_sha256d_from_midstate_fast, midstate_of_first_chunk_fast};
use crate::stratum::client::{HealthSnapshot, StratumClient, StratumJob};
use crate::stratum::mapping::{build_submit, compose_extranonce, notify_to_template};
use crate::stratum::watchdog::{spawn_watchdog, WatchdogCfg, WatchdogView};
use crate::thermal::ThermalGate;
use crate::consensus_types::WorkTemplate;

/// A solved share, handed from the mining loop to [`WorkSource::submit_solution`].
/// The pool submit (`build_submit`) consumes `{job_id, xn2, time, nonce}`.
#[derive(Clone, Debug)]
pub struct Solution {
    /// Stratum job id (pool submit) + local logging / staleness.
    pub job_id: String,
    /// Rolled xn2 high-half (pool submit via `build_submit`).
    pub xn2: u32,
    /// ntime used for this share.
    pub time: u64,
    /// Winning nonce.
    pub nonce: u32,
}

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

/// Number of hashes per share at pool-difficulty 1 (≈ 2^32). A pdiff-1 target
/// has its top set bytes at indices [4],[5] (`0x00000000FFFF0000…`), so on
/// average ~2^32 hashes are needed to find one share at difficulty 1, and ~`d`
/// times that at difficulty `d`. The exact pdiff-1 expectation is
/// `2^256 / (pdiff_1_target + 1)` ≈ 2^32 to one part in 2^16 — close enough for
/// a startup *suggestion* that the pool's vardiff will refine anyway.
const HASHES_PER_PDIFF1_SHARE: f64 = 4_294_967_296.0; // 2^32

/// Inverse of [`target_from_difficulty`] at the rate level: from a measured
/// hashrate `hashrate_hps` (hashes/second) and a desired share interval
/// `target_secs`, return the Stratum share difficulty that yields ~1 share every
/// `target_secs`.
///
/// Derivation: at difficulty `d` a share needs ~`d * 2^32` hashes; in
/// `target_secs` the miner computes `hashrate_hps * target_secs` hashes; setting
/// those equal gives `d = hashrate_hps * target_secs / 2^32`.
///
/// Floors at `1.0` (the pool's minimum) and is **fail-safe**: any non-finite or
/// non-positive input (a bogus/empty benchmark, a zero/NaN target time) returns
/// `1.0` rather than propagating a NaN/inf onto the wire. A wrong-but-finite
/// suggestion is harmless — the pool clamps it into its allowed band and vardiff
/// overrides it within a few shares — but a malformed value must never be sent.
pub fn suggested_difficulty(hashrate_hps: f64, target_secs: f64) -> f64 {
    if !hashrate_hps.is_finite()
        || hashrate_hps <= 0.0
        || !target_secs.is_finite()
        || target_secs <= 0.0
    {
        return 1.0;
    }
    let d = hashrate_hps * target_secs / HASHES_PER_PDIFF1_SHARE;
    // `d` is finite and positive here (finite positives in, finite op), but a
    // pathologically huge product could in principle round to non-finite — guard
    // it so the contract "always returns a finite value >= 1.0" holds absolutely.
    if !d.is_finite() {
        return 1.0;
    }
    d.max(1.0)
}

/// The pool's vardiff INITIAL_DIFFICULTY — the difficulty a freshly-subscribed
/// worker starts at before vardiff ramps. Confirmed `8.0` in the bridge
/// (`stratum::vardiff::INITIAL_DIFFICULTY`). A `mining.suggest_difficulty` hint is
/// only worth sending if it BEATS this floor; suggesting at-or-below it is at best
/// a no-op and at worst (the v0.1.15 bug: an under-reported GPU benchmark deriving
/// diff 1.0) actively SLOWS the rig below where the pool would have started it.
pub const POOL_DEFAULT_START_DIFFICULTY: f64 = 8.0;

/// Upper sanity ceiling for a `mining.suggest_difficulty` hint. A real GPU/CPU
/// worker running this miner tops out around diff ~1000 (even a ~50 GH/s single
/// GPU at the 30 s target ≈ 350); anything far above that is a benchmark
/// malfunction, not a device — e.g. the instant-`None` backend-error path can
/// count phantom full nonce sweeps and derive a difficulty of order 1e6.
/// Suggestions above this ceiling are REJECTED, not clamped: a too-high start
/// would hand the rig near-unsolvable work and it would look dead, so we fall back
/// to the pool default + vardiff, which finds the right difficulty within a few
/// shares. 100k is ~100× above any real single-worker rate and well below the
/// pathology, so it never rejects a legitimate suggestion.
pub const MAX_SUGGEST_DIFFICULTY: f64 = 100_000.0;

/// Gate a derived difficulty before it's forwarded as a `mining.suggest_difficulty`
/// hint. Returns `Some(d)` only when `d` is finite, positive, strictly greater
/// than `pool_default`, and not above [`MAX_SUGGEST_DIFFICULTY`]; otherwise `None`
/// (⇒ the caller skips the suggest and lets the pool's own start difficulty +
/// vardiff take over — never sends a hint that would slow the worker below the pool
/// default, nor an over-read pathology that would stall it with unsolvable work).
pub fn guarded_suggestion(d: f64, pool_default: f64) -> Option<f64> {
    if !d.is_finite() || d <= 0.0 {
        return None;
    }
    if d <= pool_default {
        return None;
    }
    if d > MAX_SUGGEST_DIFFICULTY {
        return None;
    }
    Some(d)
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

/// One unit of work the loop mines, supplied by the pool client (or the test mock).
pub struct LoopWork {
    /// The work template; `template.target` is the gate target (the pool share
    /// target).
    pub template: WorkTemplate,
    /// Job id for logging + staleness (the pool notify id).
    pub job_id: String,
    /// Pool extranonce1 low half.
    pub xn1_low: u32,
}

/// Result of polling a [`WorkSource`] for the next job.
// `Job` carries a full `WorkTemplate` while `Idle` is empty — clippy flags the
// size gap, but boxing would add a heap allocation per job on the hot intake
// path for no benefit (the enum is moved once per job, never stored in bulk).
#[allow(clippy::large_enum_variant)]
pub enum WorkIntake {
    /// A job to mine.
    Job(LoopWork),
    /// No work yet (just connected / mid-reconnect / node 503) — idle + retry.
    Idle,
}

/// Source of mining work + sink for found shares.
///
/// Abstracts the loop's dependency on a concrete [`StratumClient`] so
/// `run_stratum` can be driven by the pool client or the test mock. The
/// pool/Stratum behaviour lives in the DEFAULT methods (`next_work` =
/// latest_job → notify_to_template; `submit_solution` = build_submit →
/// send_submit); `StratumClient` + the mock inherit them, and the test mock may
/// override those two.
pub trait WorkSource {
    /// Latest job pushed by the pool, or `None` if none has arrived yet.
    fn latest_job(&self) -> Option<StratumJob>;
    /// Current share difficulty (defaults to 1.0 until the pool sends one).
    fn current_difficulty(&self) -> f64;
    /// The worker (csd1) address shares are submitted under.
    fn worker_addr(&self) -> &str;
    /// Send a `mining.submit` line for a found share.
    fn send_submit(
        &self,
        worker: &str,
        job_id: &str,
        xn2_hex: &str,
        ntime_hex: &str,
        nonce_hex: &str,
    ) -> Result<()>;

    /// A `'static` watchdog handle, if this source backs the reliability
    /// watchdog. Default `None` (e.g. the test mock); `StratumClient` returns a
    /// handle over its live connection.
    fn watchdog_view(&self) -> Option<Arc<dyn WatchdogView>> {
        None
    }

    /// A snapshot of liveness/share stats for the INFO heartbeat. Default is
    /// empty (the test mock); `StratumClient` fills it from its live session
    /// counters.
    fn health(&self) -> HealthSnapshot {
        HealthSnapshot::default()
    }

    /// Record a combined-hashrate sample (GH/s) for the optional stats endpoint
    /// (D2). Default no-op (the test mock); `StratumClient` routes it to its
    /// attached `StatsHandle`.
    fn record_hashrate(&self, _ghs: f64) {}

    /// Apply a startup-benchmark-derived share difficulty `d`: cache it (so it is
    /// re-sent on every reconnect) AND send a `mining.suggest_difficulty(d)` now.
    /// Default no-op (the test mock, which has no socket); `StratumClient`
    /// overrides it to cache on `Shared` + write the suggest frame. Best-effort —
    /// a send failure is logged by the implementer and never stops mining.
    fn apply_suggest_difficulty(&self, _d: f64) {}

    /// Heartbeat hook for the optional G6 Discord accepted-share milestone.
    /// Called from the loop's 30s heartbeat (NOT the share path). Default no-op:
    /// the test mock inherits it. `StratumClient` overrides it to post the
    /// running accepted total when it has grown.
    fn notify_heartbeat(&self) {}

    /// Poll the next unit of work. **Default = the pool/Stratum path**
    /// (`latest_job` → `notify_to_template`); the test mock may override this.
    /// Decode/mapping failures ⇒ `Idle`.
    fn next_work(&self) -> WorkIntake {
        let job = match self.latest_job() {
            Some(j) => j,
            None => return WorkIntake::Idle,
        };
        let share_target = target_from_difficulty(self.current_difficulty());
        let xn1 = match hex::decode(&job.extranonce1_hex) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    "stratum: extranonce1 {:?} not valid hex ({e}); waiting for next job",
                    job.extranonce1_hex
                );
                return WorkIntake::Idle;
            }
        };
        match notify_to_template(&job.notify, &xn1, share_target) {
            Ok(mapped) => WorkIntake::Job(LoopWork {
                template: mapped.template,
                job_id: mapped.job_id,
                xn1_low: mapped.xn1_low,
            }),
            Err(e) => {
                tracing::warn!(
                    "stratum: cannot map job {}: {e}; waiting for next job",
                    job.notify.job_id
                );
                WorkIntake::Idle
            }
        }
    }

    /// Submit a solved unit. **Default = the pool/Stratum path** (`build_submit`
    /// + `send_submit`, byte-identical to the pre-seam submit); the test mock
    /// may override this.
    fn submit_solution(&self, sol: &Solution) -> Result<()> {
        let fields = build_submit(sol.xn2, sol.time, sol.nonce);
        self.send_submit(
            self.worker_addr(),
            &sol.job_id,
            &fields.extranonce2_hex,
            &fields.ntime_hex,
            &fields.nonce_hex,
        )
    }
}

impl WorkSource for StratumClient {
    fn latest_job(&self) -> Option<StratumJob> {
        StratumClient::latest_job(self)
    }
    fn current_difficulty(&self) -> f64 {
        StratumClient::current_difficulty(self)
    }
    fn worker_addr(&self) -> &str {
        StratumClient::worker_addr(self)
    }
    fn send_submit(
        &self,
        worker: &str,
        job_id: &str,
        xn2_hex: &str,
        ntime_hex: &str,
        nonce_hex: &str,
    ) -> Result<()> {
        StratumClient::send_submit(self, worker, job_id, xn2_hex, ntime_hex, nonce_hex)
    }
    fn watchdog_view(&self) -> Option<Arc<dyn WatchdogView>> {
        Some(self.watchdog_handle())
    }
    fn health(&self) -> HealthSnapshot {
        self.health_snapshot()
    }
    fn record_hashrate(&self, ghs: f64) {
        StratumClient::record_hashrate_sample(self, ghs)
    }
    fn notify_heartbeat(&self) {
        StratumClient::notify_heartbeat_sample(self)
    }
    fn apply_suggest_difficulty(&self, d: f64) {
        // Cache first so the reconnect path always re-sends the latest value,
        // then send it now over the live socket. A send error is non-fatal: the
        // cache still holds it for the next reconnect, and vardiff ramps from the
        // floor in the meantime.
        StratumClient::set_suggest_difficulty(self, d);
        match StratumClient::send_suggest_difficulty(self, d) {
            Ok(()) => tracing::info!("stratum: sent mining.suggest_difficulty({d:.2})"),
            Err(e) => tracing::info!(
                "stratum: suggest_difficulty send failed (continuing, cached for reconnect): {e}"
            ),
        }
    }
}

/// Format the one-line INFO heartbeat from a [`HealthSnapshot`] + the current
/// difficulty. Pure → unit-tested.
fn format_health_line(h: &HealthSnapshot, difficulty: f64, hw_err: u64) -> String {
    let job_age = match h.job_age_s {
        Some(s) => format!("{s}s"),
        None => "n/a".to_string(),
    };
    let pool = if h.endpoint.is_empty() {
        "?"
    } else {
        h.endpoint.as_str()
    };
    format!(
        "health pool={pool} job_age={job_age} diff={difficulty:.2} \
         submitted={} acc={} rej={} stale={} hw_err={hw_err} conn={}/{}",
        h.submitted, h.accepted, h.rejected, h.stale, h.reconnects, h.failovers,
    )
}

/// How recent the last *new* job must be for the GPU watchdog to consider work
/// to be "flowing" (and the link healthy enough to blame the GPU for a zero
/// hashrate). Generous — a stalled GPU stays floored for many samples, so we can
/// afford to wait until jobs are clearly fresh before ever acting. Kept well
/// under the reliability watchdog's 300s job-staleness so that, once jobs go
/// truly stale, THAT watchdog reconnects and the GPU watchdog stands down (its
/// `jobs_flowing` goes false) rather than both firing.
const GPU_WD_JOB_FRESH_SECS: u64 = 120;

/// Pure: from a [`HealthSnapshot`], are fresh jobs flowing? True iff a job has
/// been seen and its age is within [`GPU_WD_JOB_FRESH_SECS`]. `None` job age
/// (no job yet) ⇒ false: a miner waiting for its first job is idle, never a
/// stalled GPU. Factored out so the idle-vs-stall gate is unit-tested directly.
fn jobs_flowing_from_health(h: &HealthSnapshot) -> bool {
    matches!(h.job_age_s, Some(age) if age <= GPU_WD_JOB_FRESH_SECS)
}

/// Pure: from a [`HealthSnapshot`], is the Stratum link healthy enough to hold
/// the GPU responsible for a zero hashrate? We use "a job has arrived recently"
/// as the liveness proxy: a live pool socket pushes work, so a fresh job age
/// means the link is delivering (a half-open/mid-reconnect socket goes job-stale
/// and trips the reliability watchdog instead). Conservative on purpose —
/// `conn_healthy` and `jobs_flowing` both gate the stall decision, so a dead
/// link makes BOTH false and the GPU watchdog stands down.
fn conn_healthy_from_health(h: &HealthSnapshot) -> bool {
    jobs_flowing_from_health(h)
}

/// A live [`GpuWatchdogView`] over the running loop: it reads the GPU-only
/// hashrate the loop publishes into a shared atomic, derives jobs/link health
/// from the work source's [`HealthSnapshot`], forwards recovery to the backend's
/// [`Recoverable::recover`], and escalates by exiting the process with
/// [`crate::gpu_watchdog::EXIT_GPU_STALLED`].
///
/// Borrows `backend` + `client` for the loop's lifetime; it is constructed and
/// driven entirely inside the `thread::scope` in [`run_stratum`], so the borrows
/// never need to be `'static`.
struct LoopGpuView<'a, B: Recoverable, W: WorkSource> {
    backend: &'a B,
    client: &'a W,
    /// Shared GPU-hashrate publication the loop updates at its 10s hashrate site:
    /// `(gpu_ghs_bits, sample_at_ms)`. The hashrate is f64 bits; `sample_at_ms`
    /// is the wall-ms the loop last completed a launch and published a sample.
    /// Read lock-free by the watchdog.
    gpu_sample: Arc<GpuHashrateSample>,
    /// If the loop hasn't published a fresh GPU sample within this many ms, the
    /// view reports 0.0 GH/s — this is the "the kernel is WEDGED and `hash_range`
    /// never returned, so the loop can't even publish" case (distinct from a
    /// kernel that returns instantly with zero work, which publishes ~0.0
    /// directly). Set by `run_stratum` to a few poll intervals.
    sample_stale_after_ms: u64,
    /// Whether escalation really exits the process. Always true in production;
    /// the loop's own tests don't construct this type (they use the mock view in
    /// the gpu_watchdog module), but this keeps the door open and documents that
    /// `escalate_exit` is a hard process kill.
    exit_on_escalate: bool,
    /// The thermal-throttle gate (OPTIONAL `nvml` build). While it is paused, the
    /// loop deliberately skips GPU launches to let the card cool — so the GPU is
    /// idle ON PURPOSE, not hung. The watchdog MUST NOT mistake that for a stall:
    /// `jobs_flowing()` reports FALSE whenever the gate is paused, which makes
    /// `gpu_watchdog_tick` reset the floored streak and stand down for the
    /// duration. On the default build (no thermal limit) the gate is never paused,
    /// so this is a no-op and behaviour is byte-identical to the pre-nvml loop.
    thermal_gate: Arc<ThermalGate>,
}

impl<'a, B: Recoverable + Sync, W: WorkSource + Sync> GpuWatchdogView for LoopGpuView<'a, B, W> {
    fn gpu_ghs(&self) -> f64 {
        let (ghs, at_ms) = self.gpu_sample.read();
        // Before the FIRST published sample (`at_ms == 0`) report a healthy
        // sentinel so a just-started miner — which hasn't completed a launch yet
        // — is never read as floored (the dwell + jobs_flowing gates also guard
        // this, but reporting non-floored here is the clearest "no data yet ⇒ not
        // a stall" stance).
        if at_ms == 0 {
            return f64::INFINITY;
        }
        let now = now_unix_ms();
        if now.saturating_sub(at_ms) > self.sample_stale_after_ms {
            // The loop hasn't completed a launch in too long ⇒ the GPU is wedged
            // INSIDE hash_range (it never returned to publish). Report floored so
            // the watchdog can act — this is exactly the hung-kernel case.
            return 0.0;
        }
        ghs
    }
    fn jobs_flowing(&self) -> bool {
        // A thermal pause makes the GPU intentionally idle: report "no work
        // flowing" so the watchdog treats it as benign idle (resets the floored
        // streak) instead of a hung GPU. This is the SAFETY wire that stops the
        // exit(17) stall path from firing during a deliberate temperature pause.
        if self.thermal_gate.is_paused() {
            return false;
        }
        jobs_flowing_from_health(&self.client.health())
    }
    fn conn_healthy(&self) -> bool {
        // Same rationale as `jobs_flowing`: while thermally paused the GPU is idle
        // by design, so the link-health gate (which also stands the watchdog down)
        // reports false too. Belt-and-braces — either gate alone is sufficient,
        // but both make the "paused ⇒ not a stall" intent explicit.
        if self.thermal_gate.is_paused() {
            return false;
        }
        conn_healthy_from_health(&self.client.health())
    }
    fn recover(&self) -> bool {
        self.backend.recover()
    }
    fn escalate_exit(&self) {
        if self.exit_on_escalate {
            // Hard exit so a supervisor (systemd / HiveOS / launcher .bat)
            // restarts the process clean. Distinct code documents the cause.
            std::process::exit(crate::gpu_watchdog::EXIT_GPU_STALLED);
        }
    }
}

/// Shared, lock-free publication of the GPU's liveness for the stall watchdog —
/// written by the mining loop, read by the GPU watchdog. Two facets:
///   - `ghs_bits`: the latest GPU-only hashrate (GH/s), refreshed at the loop's
///     10s windowed-rate site. This catches a kernel that RETURNS but did ~zero
///     work (it publishes ~0.0 directly).
///   - `at_ms`: a heartbeat stamped on EVERY completed launch (`touch`). This
///     catches a kernel WEDGED inside `hash_range` (it never returns to touch, so
///     the stamp goes stale and the view reports floored).
/// Two atomics rather than a `Mutex` so neither side ever blocks the other (the
/// hot mining loop must never wait on the watchdog).
#[derive(Debug)]
pub(crate) struct GpuHashrateSample {
    ghs_bits: AtomicU64,
    at_ms: AtomicU64,
}

impl GpuHashrateSample {
    fn new() -> Self {
        GpuHashrateSample {
            // Seed the rate to INFINITY = "unknown, treat as healthy" (NOT 0.0).
            // The share-FOUND arm of the loop never publish()es — only touch()es —
            // so a healthy GPU finding a share on nearly every launch (pathologically
            // low diff / first vardiff step before retarget) would, with a 0.0 seed,
            // read as FLOORED while perfectly healthy and trip recover()/exit(17)
            // across the fleet. A genuinely hung GPU finds NO share → takes the 10s
            // no-share arm → publish()es its real ~0.0 rate → still detected. So the
            // INFINITY seed removes only the FALSE positive (deploy-check C1).
            ghs_bits: AtomicU64::new(f64::INFINITY.to_bits()),
            at_ms: AtomicU64::new(0),
        }
    }
    /// Heartbeat: record that the loop just completed a launch at `now_ms`
    /// (whether or not it found a share). Keeps `at_ms` fresh so a fast-spinning
    /// loop is never mistaken for a wedged kernel; a truly hung kernel stops
    /// touching and the stamp ages out.
    fn touch(&self, now_ms: u64) {
        self.at_ms.store(now_ms, Ordering::Relaxed);
    }
    /// Publish the latest GPU-only windowed hashrate (GH/s); also heartbeats.
    fn publish(&self, ghs: f64, now_ms: u64) {
        self.ghs_bits.store(ghs.to_bits(), Ordering::Relaxed);
        self.at_ms.store(now_ms, Ordering::Relaxed);
    }
    /// Read `(ghs, last_heartbeat_ms)`; `at_ms == 0` ⇒ no launch completed yet.
    fn read(&self) -> (f64, u64) {
        (
            f64::from_bits(self.ghs_bits.load(Ordering::Relaxed)),
            self.at_ms.load(Ordering::Relaxed),
        )
    }
}

// NB: `LoopGpuView` is `Send + Sync` automatically when `B: Sync` and `W: Sync`
// (its only fields are shared refs + an `Arc<AtomicU64>` + a bool). The scoped
// watchdog thread in `run_stratum` is created where both bounds already hold, so
// no manual `unsafe impl` is needed — the borrow checker proves the refs outlive
// the scope and the auto-traits prove thread-safety.

/// Pure FNV-1a mix of a process id + an entropy word into a starting `xn2`, so
/// two rigs mining the SAME address (one process per GPU, or across machines)
/// begin at different coinbase regions instead of both sweeping `xn2=0..` and
/// duplicating work. Deterministic for a given `(pid, entropy)`.
fn mix_xn2_seed(pid: u32, entropy: u32) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in pid.to_le_bytes().iter().chain(entropy.to_le_bytes().iter()) {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Wall-clock ms since the Unix epoch (0 if the clock predates it — never
/// panics). Used to stamp/age the GPU hashrate sample for the stall watchdog.
fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Seed `xn2` from this process's id + sub-second startup entropy. Wire-safe:
/// the bridge accepts any 4-byte xn2 (see [`crate::stratum::mapping::build_submit`]).
fn seed_xn2() -> u32 {
    let pid = std::process::id();
    let entropy = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    mix_xn2_seed(pid, entropy)
}

/// Run the pooled Stratum mining loop until `stop` is set.
///
/// `client` must already be connected (handshake done, reader thread running).
/// Work is pulled from its background-updated `latest_job()` / difficulty; found
/// shares are submitted via `client.send_submit`. The CPU+GPU split honours the
/// `MiningConfig` knobs `cpu_threads` and `cpu_share`.
///
/// Thin shim that runs with the GPU watchdog **disabled** — preserves every
/// existing caller/test signature unchanged (modulo the `Sync` bounds, which all
/// real backends/work sources and the test mocks already satisfy). Production
/// wires the watchdog via [`run_stratum_with_gpu_watchdog`].
pub fn run_stratum<B: MiningBackend + Recoverable + Sync, W: WorkSource + Sync>(
    backend: &B,
    client: &W,
    stop: Arc<AtomicBool>,
    cfg: MiningConfig,
) -> Result<()> {
    run_stratum_with_gpu_watchdog(
        backend,
        client,
        stop,
        cfg,
        GpuWatchdogCfg {
            enabled: false,
            ..GpuWatchdogCfg::default()
        },
    )
}

/// Run the pooled Stratum mining loop with BOTH the optional GPU stall watchdog
/// and an optional **thermal-throttle gate** (OPTIONAL `nvml` build).
///
/// Identical to [`run_stratum_with_gpu_watchdog`] but additionally honours a
/// shared [`ThermalGate`]: while the gate is paused (the GPU is over its
/// configured temperature limit), the loop SKIPS GPU launches so the card can
/// cool, and the GPU stall watchdog stands down (the loop view reports the GPU
/// as intentionally idle, NOT floored — so a thermal pause can never trip the
/// hung-GPU exit(17)). A never-paused gate (the default build, or no
/// `--temp-limit`) makes this byte-identical to
/// [`run_stratum_with_gpu_watchdog`]. The gate is driven by the caller's thermal
/// poller (see [`crate::thermal::spawn_thermal_poller`]).
pub fn run_stratum_with_gpu_watchdog<B: MiningBackend + Recoverable + Sync, W: WorkSource + Sync>(
    backend: &B,
    client: &W,
    stop: Arc<AtomicBool>,
    cfg: MiningConfig,
    gpu_wd_cfg: GpuWatchdogCfg,
) -> Result<()> {
    // Default: a gate that is never paused (no thermal throttle). Identical
    // behaviour to the pre-thermal loop.
    run_stratum_full(
        backend,
        client,
        stop,
        cfg,
        gpu_wd_cfg,
        Arc::new(ThermalGate::new()),
    )
}

/// Run the pooled Stratum mining loop with an optional **GPU stall watchdog**
/// plus an optional **thermal-throttle gate**.
///
/// Identical to [`run_stratum`] but additionally samples the GPU-only hashrate
/// every `gpu_wd_cfg.poll`; if it stays floored while fresh jobs flow over a
/// healthy link, it attempts an in-process [`Recoverable::recover`] and, failing
/// that, exits the process with [`crate::gpu_watchdog::EXIT_GPU_STALLED`] for a
/// supervisor restart. The watchdog runs as a **scoped** thread so it can borrow
/// `backend`/`client` without a `'static` bound; it joins automatically when the
/// loop returns. With `gpu_wd_cfg.enabled == false` the watchdog thread exits
/// immediately and behaviour is byte-identical to the pre-watchdog loop.
///
/// `thermal_gate`: while it is paused (GPU over its configured temperature
/// limit), the loop SKIPS GPU launches and the watchdog stands down (the GPU is
/// idle by design, not stalled). A never-paused gate (the default build) is a
/// no-op. Most callers/tests use [`run_stratum_with_gpu_watchdog`], which passes
/// a never-paused gate; the `nvml` build's `main` passes a live gate.
pub fn run_stratum_full<B: MiningBackend + Recoverable + Sync, W: WorkSource + Sync>(
    backend: &B,
    client: &W,
    stop: Arc<AtomicBool>,
    cfg: MiningConfig,
    gpu_wd_cfg: GpuWatchdogCfg,
    thermal_gate: Arc<ThermalGate>,
) -> Result<()> {
    // Re-derive fresh work this often even if no new notify arrived, so a
    // long-lived job picks up difficulty changes and ntime drift promptly.
    let refresh_every = Duration::from_secs(2);

    // Hashrate tracking (mirrors the node loop's 10s cadence).
    let mut last_hashrate_log = Instant::now();
    let mut last_heartbeat = Instant::now();
    // Backend hash-mismatch count (a kernel/driver/overclock fault caught by the
    // pre-submit gate) — surfaced in the heartbeat so operators can spot an
    // unstable overclock (B3 bad-OC signal).
    let mut hash_mismatches: u64 = 0;
    let mut gpu_nonces_since_log: u128 = 0;
    let mut cpu_nonces_since_log: u128 = 0;

    // Rate-limit the "waiting for first job" notice so a slow pool start
    // doesn't spam the log.
    let mut last_wait_log = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .unwrap_or_else(Instant::now);

    // The miner-rolled high half of the coinbase extranonce. Bumped once per
    // exhausted launch. The low half (xn1) is pool-fixed and NEVER rolled here.
    // Seeded per-process so co-fleet rigs (same address, one process per GPU)
    // sweep DIFFERENT coinbase regions instead of duplicating xn2=0.. work.
    let mut xn2: u32 = seed_xn2();

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

    // Reliability watchdog: an out-of-band thread that forces a reconnect on a
    // half-open socket (submits going un-acked) or a dead push channel (no new
    // jobs). Detached — it owns Arc clones and exits when `stop` is set, so the
    // loop never has to join it. Sources without a live connection (the test
    // mock) return `None` and run without it.
    let _watchdog = client
        .watchdog_view()
        .map(|view| spawn_watchdog(view, WatchdogCfg::default(), Arc::clone(&stop)));

    // GPU-only hashrate sample the loop publishes for the GPU stall watchdog
    // (lock-free). Updated at the existing 10s hashrate-log site below — GPU
    // contribution ONLY (the CPU pool's MH/s are excluded so a hung GPU with a
    // busy CPU pool still reads as floored). The companion timestamp lets the
    // watchdog detect a kernel wedged INSIDE `hash_range` (no fresh sample).
    let gpu_sample = Arc::new(GpuHashrateSample::new());

    // Treat the published sample as floored if it is older than 3 poll intervals
    // — long enough that a normal between-launch gap never looks wedged, short
    // enough that a kernel stuck in `synchronize()` is caught within a few polls.
    let sample_stale_after_ms = (gpu_wd_cfg.poll.as_millis() as u64).saturating_mul(3);

    // Drive the mining loop and the GPU stall watchdog inside one `thread::scope`
    // so the watchdog thread can borrow `backend`/`client` (calling
    // `backend.recover()` and `client.health()`) without a `'static` bound. The
    // scope joins the watchdog automatically when the loop returns. When the GPU
    // watchdog is disabled (`gpu_wd_cfg.enabled == false`, e.g. the CPU backend,
    // `--no-gpu-watchdog`, or every existing test via `run_stratum`) its thread
    // exits at once and this is byte-identical to the pre-watchdog loop.
    std::thread::scope(|scope| {
        let gpu_view = LoopGpuView {
            backend,
            client,
            gpu_sample: Arc::clone(&gpu_sample),
            sample_stale_after_ms,
            exit_on_escalate: true,
            thermal_gate: Arc::clone(&thermal_gate),
        };
        let gpu_wd_stop = Arc::clone(&stop);
        scope.spawn(move || {
            if !gpu_wd_cfg.enabled {
                return;
            }
            tracing::info!(
                "gpu-watchdog: armed (floor={:.4} GH/s, dwell={} samples, poll={:?}, recover_window={:?}, max_recoveries={}, exit_code={})",
                gpu_wd_cfg.floor_ghs,
                gpu_wd_cfg.dwell_samples,
                gpu_wd_cfg.poll,
                gpu_wd_cfg.recover_window,
                gpu_wd_cfg.max_recoveries,
                crate::gpu_watchdog::EXIT_GPU_STALLED,
            );
            let mut state = GpuWatchdogState::default();
            let slice = Duration::from_millis(200).min(gpu_wd_cfg.poll);
            let mut waited = Duration::ZERO;
            while !gpu_wd_stop.load(Ordering::Relaxed) {
                if waited >= gpu_wd_cfg.poll {
                    waited = Duration::ZERO;
                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    gpu_watchdog_tick(&gpu_view, gpu_wd_cfg, &mut state, now_ms);
                }
                std::thread::sleep(slice);
                waited += slice;
            }
        });

    'mining: while !stop.load(Ordering::Relaxed) {
        // Emit a health heartbeat at a fixed cadence — even while idle/waiting
        // for the first job — so "connected but 0 h/s / stale" is never silent.
        if last_heartbeat.elapsed() >= Duration::from_secs(30) {
            tracing::info!(
                "{}",
                format_health_line(
                    &client.health(),
                    client.current_difficulty(),
                    hash_mismatches
                )
            );
            // G6: best-effort accepted-share milestone ping (no-op unless a
            // Discord webhook is wired AND the total grew; pool stays silent
            // under --discord-solutions-only). Heartbeat-only — never the share
            // path — so it can't affect submit timing/correctness.
            client.notify_heartbeat();
            last_heartbeat = Instant::now();
        }

        // --- work intake: poll the work source (the pool notify) ---
        let work = match client.next_work() {
            WorkIntake::Job(w) => w,
            WorkIntake::Idle => {
                if last_wait_log.elapsed() >= Duration::from_secs(10) {
                    tracing::info!("stratum: waiting for first job…");
                    last_wait_log = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
        };
        let template = &work.template;
        let branch: Vec<[u8; 32]> = template.merkle_branch.iter().map(|b| b.0).collect();

        tracing::info!(
            "stratum got_job id={} height(n/a) prev=0x{} diff={:.4} share_target=0x{}…",
            work.job_id,
            hex::encode(template.prev),
            client.current_difficulty(),
            &hex::encode(template.target)[..16],
        );

        let last_refresh = Instant::now();

        // Inner per-launch loop for THIS job. Re-derives the coinbase/header
        // each launch from the rolled xn2, races the backends, submits on find.
        loop {
            if stop.load(Ordering::Relaxed) {
                break 'mining; // shutdown: leave the mining loop (scope joins the watchdog)
            }
            if last_refresh.elapsed() > refresh_every {
                break; // re-poll latest_job (may be the same; may be newer)
            }
            // If the pool pushed a new job, abandon this one immediately so we
            // never mine stale work past a clean_jobs boundary.
            if let Some(j) = client.latest_job() {
                if j.notify.job_id != work.job_id {
                    break;
                }
            }

            // THERMAL THROTTLE (OPTIONAL nvml build): if the GPU is over its
            // configured temperature limit, the thermal poller has paused the
            // gate. Skip GPU launches entirely while paused so the card cools —
            // do NOT dispatch `hash_range`, do NOT roll xn2, just nap and re-check
            // (the poller resumes the gate once the temperature falls below the
            // resume threshold). The GPU stall watchdog stands down meanwhile
            // because the loop view reports the GPU as intentionally idle (see
            // `LoopGpuView::jobs_flowing`), so a thermal pause can never be
            // mistaken for a hung GPU. On the default build the gate is never
            // paused, so this branch is never taken. We still break out promptly
            // on `stop` (checked at the top of the loop) and on a refresh/new job.
            if thermal_gate.is_paused() {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }

            // Compose the full 8-byte extranonce: low = pool xn1, high = our xn2.
            let extranonce = compose_extranonce(work.xn1_low, xn2);

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
                                if stop_.load(Ordering::Relaxed) {
                                    // Propagate the global stop into iter_stop so
                                    // the GPU backend (which watches iter_stop in
                                    // dual-mining mode) wakes promptly — this
                                    // replaces the old 5ms poller thread.
                                    iter_stop_.store(true, Ordering::Release);
                                    break;
                                }
                                if iter_stop_.load(Ordering::Relaxed) {
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

                // The GPU backend watches a SINGLE cancel flag (no polling
                // thread). With CPU workers it watches `iter_stop` (set on a CPU
                // win, or by a worker propagating the global stop); GPU-only it
                // watches `stop` directly so it cancels mid-launch on shutdown.
                let backend_stop: &AtomicBool = if cpu_ranges.is_empty() {
                    &stop
                } else {
                    &iter_stop
                };

                // GPU sweep on its assigned sub-range (main scope thread).
                let (gstart, gend) = gpu_range;
                let res = if gend > gstart {
                    backend.hash_range(hdr, target, gstart, gend, backend_stop)
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

            // GPU watchdog heartbeat: this launch completed (hash_range returned),
            // so the kernel is NOT wedged. Stamp it so the watchdog's
            // stale-sample check sees the loop is alive; a hung kernel that never
            // returns from hash_range stops reaching here and the stamp ages out.
            // (Only meaningful when a GPU range was actually dispatched.)
            if gpu_range.1 > gpu_range.0 {
                gpu_sample.touch(now_unix_ms());
            }

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
                            work.job_id,
                            hex::encode(claimed_hash),
                            hex::encode(cpu_hash),
                        );
                        hash_mismatches += 1; // bad-OC / kernel-fault signal (B3)
                        xn2 = xn2.wrapping_add(1);
                        continue;
                    }
                    if !hash_leq_target(&cpu_hash, &template.target) {
                        tracing::error!(
                            "stratum device={device} hash ABOVE share target job={} nonce={nonce}: hash=0x{} target=0x{} - skipping",
                            work.job_id,
                            hex::encode(cpu_hash),
                            hex::encode(template.target),
                        );
                        xn2 = xn2.wrapping_add(1);
                        continue;
                    }

                    match thread_label {
                        Some(t) => tracing::info!(
                            "stratum SHARE device={device} thread={t} job={} xn2={xn2} nonce={nonce} hash=0x{}",
                            work.job_id, hex::encode(cpu_hash),
                        ),
                        None => tracing::info!(
                            "stratum SHARE device={device} job={} xn2={xn2} nonce={nonce} hash=0x{}",
                            work.job_id, hex::encode(cpu_hash),
                        ),
                    }

                    let sol = Solution {
                        job_id: work.job_id.clone(),
                        xn2,
                        time: template.time,
                        nonce,
                    };
                    let submit_start = Instant::now();
                    match client.submit_solution(&sol) {
                        Ok(()) => tracing::info!(
                            "stratum submit OK job={} xn2={} ntime={} nonce={} latency_ms={}",
                            work.job_id, xn2, template.time, nonce, submit_start.elapsed().as_millis(),
                        ),
                        Err(e) => tracing::warn!("stratum submit FAILED job={}: {e}", work.job_id),
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
                        // D2: feed the optional stats endpoint (no-op unless the
                        // operator ran --stats-port). Never touches the share path.
                        client.record_hashrate(combined_ghs);
                        // GPU watchdog: publish the GPU-ONLY windowed rate (CPU
                        // excluded on purpose). A hung-but-returning kernel
                        // publishes ~0.0 here ⇒ the watchdog floors it.
                        gpu_sample.publish(ghs_gpu, now_unix_ms());
                        tracing::info!(
                            "stratum hashrate gpu={:.2} GH/s cpu={:.2} MH/s combined={:.2} GH/s (job={}, diff={:.2})",
                            ghs_gpu, mhs_cpu, combined_ghs, work.job_id, client.current_difficulty(),
                        );
                        last_hashrate_log = Instant::now();
                        gpu_nonces_since_log = 0;
                        cpu_nonces_since_log = 0;
                    }
                    xn2 = xn2.wrapping_add(1);
                }
            }
        }
    } // 'mining while

        // The mining loop has exited (shutdown). Signal the GPU watchdog thread
        // to stop too; the scope joins it on the way out. (Redundant with the
        // outer `stop` the watchdog already shares, but explicit.)
        stop.store(true, Ordering::Relaxed);
    }); // thread::scope — joins the GPU watchdog thread here

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

    // --- guarded_suggestion tests: only forward a suggestion that BEATS the
    //     pool's vardiff start floor; otherwise None ⇒ skip suggest, mine
    //     normally (the v0.1.15 bug was suggesting diff 1.0 — WORSE than the
    //     pool's own diff-8 start — actively hurting a fast rig). ---

    #[test]
    fn guarded_suggestion_below_pool_default_is_none() {
        // THE reported bug: an under-reported benchmark derived diff 1.0, which is
        // BELOW the pool's diff-8 start — forwarding it would slow the rig. Guard
        // ⇒ None ⇒ no suggest ⇒ pool's own (better) default stands.
        assert_eq!(guarded_suggestion(1.0, POOL_DEFAULT_START_DIFFICULTY), None);
    }

    #[test]
    fn guarded_suggestion_equal_to_pool_default_is_none() {
        // Exactly the pool default adds nothing — don't bother suggesting it.
        assert_eq!(guarded_suggestion(8.0, 8.0), None);
    }

    #[test]
    fn guarded_suggestion_above_pool_default_passes_through() {
        // A genuinely-higher derived difficulty (the whole point — a fast GPU
        // wants to START high, not ramp from 8) is forwarded unchanged.
        assert_eq!(guarded_suggestion(250.0, 8.0), Some(250.0));
    }

    #[test]
    fn guarded_suggestion_rejects_non_finite_and_non_positive() {
        // Defensive: NaN/inf/0/negative are never valid suggestions ⇒ None,
        // independent of the pool default.
        assert_eq!(guarded_suggestion(f64::NAN, 8.0), None);
        assert_eq!(guarded_suggestion(f64::INFINITY, 8.0), None);
        assert_eq!(guarded_suggestion(0.0, 8.0), None);
        assert_eq!(guarded_suggestion(-5.0, 8.0), None);
    }

    #[test]
    fn guarded_suggestion_rejects_over_read_above_ceiling() {
        // A benchmark malfunction (the instant-None backend-error path counting
        // phantom nonce sweeps) can derive a diff of order 1e6. Forwarding it would
        // hand the rig near-unsolvable work ⇒ looks dead. Reject ⇒ None ⇒ fall back
        // to pool default + vardiff. Must reject regardless of how far above.
        assert_eq!(guarded_suggestion(1_000_000.0, 8.0), None);
        assert_eq!(guarded_suggestion(MAX_SUGGEST_DIFFICULTY + 1.0, 8.0), None);
    }

    #[test]
    fn guarded_suggestion_at_and_below_ceiling_passes() {
        // The ceiling is generous: a legitimate high-end single-worker suggestion
        // (well under the ceiling) and the boundary value itself pass through.
        assert_eq!(guarded_suggestion(MAX_SUGGEST_DIFFICULTY, 8.0), Some(MAX_SUGGEST_DIFFICULTY));
        assert_eq!(guarded_suggestion(5000.0, 8.0), Some(5000.0));
    }

    #[test]
    fn pool_default_start_difficulty_matches_bridge_initial() {
        // Pin the constant to the bridge's vardiff INITIAL_DIFFICULTY (8.0); if
        // the pool ever changes that floor this test flags the mismatch.
        assert_eq!(POOL_DEFAULT_START_DIFFICULTY, 8.0);
    }

    // --- suggested_difficulty tests (the inverse of target_from_difficulty:
    //     from a measured hashrate, what share difficulty lands ~1 share / T) ---

    #[test]
    fn suggested_difficulty_known_value() {
        // A share at pdiff-1 is ~2^32 hashes. At H hashes/s over T seconds the
        // miner does H*T hashes, so the difficulty that yields ~1 share per T is
        // d = H*T / 2^32. Pick H and T so the math is exact:
        //   H = 2^32 hashes/s, T = 20s  ⇒  d = 20.
        let two_pow_32 = 4_294_967_296.0_f64;
        let d = suggested_difficulty(two_pow_32, 20.0);
        assert!((d - 20.0).abs() < 1e-9, "expected ~20, got {d}");
        // H = 2^32, T = 1  ⇒  d = 1 exactly (the floor case, not the clamp).
        assert!((suggested_difficulty(two_pow_32, 1.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn suggested_difficulty_floors_at_one() {
        // A weak miner (H*T/2^32 < 1) must never suggest below 1.0 — the pool's
        // minimum difficulty. A few-hundred-MH/s CPU over 20s is well under 2^32.
        let d = suggested_difficulty(200_000_000.0, 20.0);
        assert_eq!(d, 1.0, "tiny work must clamp to the diff-1 floor");
    }

    #[test]
    fn suggested_difficulty_rejects_non_finite_and_non_positive() {
        // Defensive: a bogus benchmark (0, NaN, inf, negative) must yield 1.0,
        // never NaN/inf/negative — a wrong-but-sane suggest is harmless (the pool
        // clamps + vardiff overrides), but a NaN on the wire would be malformed.
        assert_eq!(suggested_difficulty(0.0, 20.0), 1.0);
        assert_eq!(suggested_difficulty(-5.0, 20.0), 1.0);
        assert_eq!(suggested_difficulty(f64::NAN, 20.0), 1.0);
        assert_eq!(suggested_difficulty(f64::INFINITY, 20.0), 1.0);
        // A non-finite / non-positive TARGET TIME is equally bogus ⇒ 1.0.
        assert_eq!(suggested_difficulty(1e12, 0.0), 1.0);
        assert_eq!(suggested_difficulty(1e12, f64::NAN), 1.0);
        assert_eq!(suggested_difficulty(1e12, -1.0), 1.0);
    }

    #[test]
    fn suggested_difficulty_large_hashrate_gives_large_d() {
        // A strong GPU rig (~5 GH/s) over 20s should suggest a difficulty far
        // above the diff-8 floor the pool would otherwise ramp from.
        let d = suggested_difficulty(5_000_000_000.0, 20.0);
        // 5e9 * 20 / 2^32 ≈ 23.28
        assert!(d > 20.0 && d < 30.0, "expected ~23, got {d}");
        // Monotonic in H: more hashrate ⇒ strictly higher suggestion.
        assert!(suggested_difficulty(10e9, 20.0) > suggested_difficulty(5e9, 20.0));
        // And finite + positive for a huge but realistic fleet aggregate.
        let big = suggested_difficulty(1e15, 20.0);
        assert!(big.is_finite() && big > 1.0);
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
    impl Recoverable for NullGpu {} // default no-op recover()

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

    // --- P0 headless harness: drive the REAL run_stratum with mock WorkSource
    //     + mock Backend, no socket and no GPU. ---

    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    /// A programmable backend: pops a scripted result per `hash_range` call.
    /// Once the script is exhausted it behaves like a real backend that hashes
    /// until told to stop (so the loop never busy-spins). Lets a test exercise
    /// the FOUND path + correctness gate with no GPU.
    ///
    /// `return_when_empty` selects what an exhausted-script call does:
    ///   - `false` (default, `new`): BLOCK until `stop` — emulates a backend whose
    ///     single launch spans the whole window (the loop dispatches it ~once).
    ///   - `true` (`new_returning`): RETURN `None` immediately — emulates a REAL
    ///     backend that finished a finite nonce sweep finding nothing and returned,
    ///     so the loop re-dispatches it every iteration (the realistic
    ///     returns-from-hash_range path the watchdog must treat as healthy).
    struct MockBackend {
        script: Mutex<VecDeque<Option<MiningResult>>>,
        calls: AtomicUsize,
        /// How many times the GPU watchdog called `recover()` on this backend.
        /// Stays 0 unless the watchdog read the GPU as floored — so a test can
        /// assert a healthy/returning backend is NEVER mistaken for a stall (C1).
        recover_calls: AtomicUsize,
        return_when_empty: bool,
    }
    impl MockBackend {
        fn new(script: Vec<Option<MiningResult>>) -> Self {
            MockBackend {
                script: Mutex::new(script.into()),
                calls: AtomicUsize::new(0),
                recover_calls: AtomicUsize::new(0),
                return_when_empty: false,
            }
        }
        /// A backend whose `hash_range` RETURNS (finite sweep, finds nothing)
        /// instead of blocking — the realistic path the watchdog coverage needs.
        fn new_returning() -> Self {
            MockBackend {
                script: Mutex::new(VecDeque::new()),
                calls: AtomicUsize::new(0),
                recover_calls: AtomicUsize::new(0),
                return_when_empty: true,
            }
        }
    }
    impl MiningBackend for MockBackend {
        fn name(&self) -> &'static str {
            "mock-backend"
        }
        fn hash_range(
            &self,
            _h: [u8; 84],
            _t: [u8; 32],
            _s: u32,
            _e: u32,
            stop: &AtomicBool,
        ) -> Option<MiningResult> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if let Some(found) = self.script.lock().unwrap().pop_front().flatten() {
                return Some(found);
            }
            // No scripted find. A *returning* backend mimics a real finite sweep:
            // return None at once so the loop re-dispatches each iteration (and
            // `touch()`es the GPU sample every launch). Otherwise emulate a
            // backend whose launch spans the window by blocking until told to
            // stop. NOTE: these tests run cpu_threads=0, so run_stratum passes the
            // outer `stop` to the backend directly (C1 removed the poller).
            if self.return_when_empty {
                return None;
            }
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1));
            }
            None
        }
    }
    impl Recoverable for MockBackend {
        // Count recover() calls (observably identical to the default no-op:
        // returns false = "not recovered"). Lets a test prove the watchdog did
        // NOT treat a healthy/returning backend as a stall. The full stall ladder
        // is covered by the gpu_watchdog module's MockView tick tests.
        fn recover(&self) -> bool {
            self.recover_calls.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    /// A socket-free [`WorkSource`]: serves one canned job + difficulty and
    /// records every submit the loop sends, so `run_stratum` can be driven
    /// headless and its submit behaviour asserted.
    struct MockWorkSource {
        job: StratumJob,
        difficulty: f64,
        worker: String,
        submits: Arc<Mutex<Vec<(String, String, String, String, String)>>>,
        /// What `health()` reports. Defaults to `HealthSnapshot::default()`
        /// (`job_age_s = None` ⇒ `jobs_flowing`/`conn_healthy` both FALSE, so the
        /// GPU watchdog's actionable-stall gate never fires). `with_fresh_jobs()`
        /// sets a fresh job age so a test CAN reach the floored→recover path.
        health: HealthSnapshot,
    }
    impl MockWorkSource {
        fn new(job: StratumJob, difficulty: f64) -> Self {
            MockWorkSource {
                job,
                difficulty,
                worker: "csd1mockworker".to_string(),
                submits: Arc::new(Mutex::new(Vec::new())),
                health: HealthSnapshot::default(),
            }
        }
        /// Report a FRESH job age so `jobs_flowing()` + `conn_healthy()` are true —
        /// i.e. "the pool is delivering work over a healthy link". This is what
        /// lets the GPU-stall watchdog actually evaluate (and, on a real floor,
        /// recover/exit). Without it the watchdog stands down on every tick.
        fn with_fresh_jobs(mut self) -> Self {
            self.health = HealthSnapshot {
                job_age_s: Some(1), // 1s old ≪ GPU_WD_JOB_FRESH_SECS ⇒ flowing+healthy
                ..HealthSnapshot::default()
            };
            self
        }
    }
    impl WorkSource for MockWorkSource {
        fn latest_job(&self) -> Option<StratumJob> {
            Some(self.job.clone())
        }
        fn current_difficulty(&self) -> f64 {
            self.difficulty
        }
        fn worker_addr(&self) -> &str {
            &self.worker
        }
        fn health(&self) -> HealthSnapshot {
            self.health.clone()
        }
        fn send_submit(
            &self,
            worker: &str,
            job_id: &str,
            xn2_hex: &str,
            ntime_hex: &str,
            nonce_hex: &str,
        ) -> Result<()> {
            self.submits.lock().unwrap().push((
                worker.to_string(),
                job_id.to_string(),
                xn2_hex.to_string(),
                ntime_hex.to_string(),
                nonce_hex.to_string(),
            ));
            Ok(())
        }
    }

    fn mock_job() -> StratumJob {
        StratumJob {
            notify: fixture_notify(),
            extranonce1_hex: "aabbccdd".to_string(),
        }
    }

    #[test]
    fn next_work_default_maps_pool_job() {
        // The default next_work IS the pool path: latest_job → notify_to_template,
        // with the share target baked into template.target.
        let src = MockWorkSource::new(mock_job(), 1024.0);
        match src.next_work() {
            WorkIntake::Job(w) => {
                assert_eq!(w.template.target, target_from_difficulty(1024.0));
                assert!(!w.job_id.is_empty());
            }
            WorkIntake::Idle => panic!("a mappable job must yield Job, not Idle"),
        }
    }

    #[test]
    fn submit_solution_default_routes_through_send_submit() {
        // The default submit_solution IS the pool path: build_submit + send_submit.
        let src = MockWorkSource::new(mock_job(), 1.0);
        let sol = Solution {
            job_id: "job-xyz".to_string(),
            xn2: 0x1122_3344,
            time: 0x6543_2100,
            nonce: 0xABCD,
        };
        src.submit_solution(&sol).unwrap();
        let recorded = src.submits.lock().unwrap();
        assert_eq!(recorded.len(), 1, "one submit recorded");
        let (worker, job_id, _xn2_hex, _ntime, _nonce_hex) = &recorded[0];
        assert_eq!(worker, "csd1mockworker");
        assert_eq!(job_id, "job-xyz");
    }

    #[test]
    fn health_line_contains_key_fields() {
        let h = HealthSnapshot {
            accepted: 5,
            rejected: 1,
            stale: 2,
            submitted: 9,
            job_age_s: Some(42),
            endpoint: "pool.test:3333".to_string(),
            reconnects: 3,
            failovers: 1,
        };
        let line = format_health_line(&h, 1024.0, 7);
        for needle in [
            "pool=pool.test:3333",
            "job_age=42s",
            "diff=1024.00",
            "submitted=9",
            "acc=5",
            "rej=1",
            "stale=2",
            "hw_err=7",
            "conn=3/1", // reconnects/failovers
        ] {
            assert!(line.contains(needle), "missing {needle:?} in {line:?}");
        }
        // No job yet → n/a; empty endpoint → '?'.
        let h2 = HealthSnapshot {
            job_age_s: None,
            endpoint: String::new(),
            ..h
        };
        let line2 = format_health_line(&h2, 1.0, 0);
        assert!(line2.contains("job_age=n/a"));
        assert!(line2.contains("pool=?"));
        assert!(line2.contains("hw_err=0"));
    }

    #[test]
    fn xn2_seed_diverges_by_pid_and_entropy() {
        // Deterministic for fixed inputs.
        assert_eq!(mix_xn2_seed(1234, 5678), mix_xn2_seed(1234, 5678));
        // A different pid OR different entropy → a different seed.
        assert_ne!(mix_xn2_seed(1234, 5678), mix_xn2_seed(1235, 5678));
        assert_ne!(mix_xn2_seed(1234, 5678), mix_xn2_seed(1234, 5679));
        // Two distinct co-fleet rigs (distinct pids) get distinct seeds.
        assert_ne!(mix_xn2_seed(1000, 42), mix_xn2_seed(2000, 42));
    }

    /// Drive the REAL `run_stratum` with a mock work source + a never-finds
    /// backend: it must map the job, dispatch the backend, and shut down
    /// cleanly on `stop` with no submit — the socket-free analogue of
    /// `run_stratum_connects_maps_and_shuts_down_cleanly`.
    #[test]
    fn run_stratum_via_mock_worksource_shuts_down_clean() {
        let work = Arc::new(MockWorkSource::new(mock_job(), 1024.0));
        let backend = Arc::new(MockBackend::new(vec![])); // never finds
        let stop = Arc::new(AtomicBool::new(false));

        let work_for_loop = Arc::clone(&work);
        let backend_for_loop = Arc::clone(&backend);
        let stop_for_loop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let cfg = MiningConfig {
                cpu_threads: 0,
                cpu_share: 0.0,
            };
            run_stratum(
                backend_for_loop.as_ref(),
                work_for_loop.as_ref(),
                stop_for_loop,
                cfg,
            )
        });

        // Wait (up to ~3s) for the backend to actually be dispatched — robust
        // under CPU load, where a fixed short sleep can race the loop's startup.
        for _ in 0..300 {
            if backend.calls.load(Ordering::Relaxed) > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        let result = handle.join().expect("loop thread did not panic");
        assert!(result.is_ok(), "run_stratum returns Ok on clean shutdown");
        assert!(
            backend.calls.load(Ordering::Relaxed) > 0,
            "the loop must have actually dispatched the backend, not idled"
        );
        assert!(
            work.submits.lock().unwrap().is_empty(),
            "no share should be submitted when the backend never finds"
        );
    }

    /// C1 dual-mining path: with cpu_threads > 0 the GPU backend watches
    /// `iter_stop`, and a CPU worker propagates the global stop into it (no 5ms
    /// poller). The backend never finds and the CPU pool can't clear diff-1024
    /// in the window; setting stop must shut everything down cleanly.
    #[test]
    fn run_stratum_dual_mining_shuts_down_clean() {
        let work = Arc::new(MockWorkSource::new(mock_job(), 1024.0));
        let backend = Arc::new(MockBackend::new(vec![])); // never finds; blocks until iter_stop
        let stop = Arc::new(AtomicBool::new(false));

        let work_for_loop = Arc::clone(&work);
        let backend_for_loop = Arc::clone(&backend);
        let stop_for_loop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let cfg = MiningConfig {
                cpu_threads: 2,
                cpu_share: 0.5,
            };
            run_stratum(
                backend_for_loop.as_ref(),
                work_for_loop.as_ref(),
                stop_for_loop,
                cfg,
            )
        });

        // Wait (up to ~3s) for the backend to actually be dispatched — robust
        // under CPU load, where a fixed short sleep can race the loop's startup.
        for _ in 0..300 {
            if backend.calls.load(Ordering::Relaxed) > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        let result = handle.join().expect("loop thread did not panic");
        assert!(result.is_ok(), "run_stratum returns Ok on clean shutdown");
        assert!(
            backend.calls.load(Ordering::Relaxed) > 0,
            "the GPU backend must have been dispatched alongside the CPU pool"
        );
        assert!(
            work.submits.lock().unwrap().is_empty(),
            "no share at diff 1024 in the brief window"
        );
    }

    /// A backend that returns a find with a BOGUS hash must NOT produce a
    /// submit: the loop's pre-submit CPU re-hash gate catches the mismatch and
    /// skips it. Proves a malfunctioning/lying backend can't push a bad share.
    #[test]
    fn bad_backend_find_is_rejected_by_correctness_gate() {
        let work = Arc::new(MockWorkSource::new(mock_job(), 1024.0));
        // Claimed find with an all-zero hash that will NOT match the real
        // sha256d of the header at that nonce.
        let bogus = MiningResult {
            nonce: 12345,
            hash: [0u8; 32],
        };
        let backend = Arc::new(MockBackend::new(vec![Some(bogus)]));
        let stop = Arc::new(AtomicBool::new(false));

        let work_for_loop = Arc::clone(&work);
        let backend_for_loop = Arc::clone(&backend);
        let stop_for_loop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let cfg = MiningConfig {
                cpu_threads: 0,
                cpu_share: 0.0,
            };
            run_stratum(
                backend_for_loop.as_ref(),
                work_for_loop.as_ref(),
                stop_for_loop,
                cfg,
            )
        });

        // Wait (up to ~3s) for the backend to actually be dispatched — robust
        // under CPU load, where a fixed short sleep can race the loop's startup.
        for _ in 0..300 {
            if backend.calls.load(Ordering::Relaxed) > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        handle
            .join()
            .expect("loop thread did not panic")
            .expect("run_stratum Ok");
        // The backend WAS dispatched and returned the bogus find (calls > 0)...
        assert!(
            backend.calls.load(Ordering::Relaxed) > 0,
            "the loop must have dispatched the backend and processed the find"
        );
        // ...yet nothing was submitted — so the pre-submit gate is provably
        // what rejected it (only the gate sits between FOUND and submit).
        assert!(
            work.submits.lock().unwrap().is_empty(),
            "the correctness gate must reject a backend find whose hash is wrong"
        );
    }

    // --- GPU stall watchdog wiring (pure helpers + the live integration) ---

    #[test]
    fn jobs_flowing_and_conn_healthy_track_job_age() {
        // No job yet ⇒ neither flowing nor healthy (a miner waiting for its first
        // job is idle, never a stalled GPU).
        let none = HealthSnapshot { job_age_s: None, ..Default::default() };
        assert!(!jobs_flowing_from_health(&none));
        assert!(!conn_healthy_from_health(&none));
        // Fresh job (within the freshness window) ⇒ both true.
        let fresh = HealthSnapshot {
            job_age_s: Some(GPU_WD_JOB_FRESH_SECS - 1),
            ..Default::default()
        };
        assert!(jobs_flowing_from_health(&fresh));
        assert!(conn_healthy_from_health(&fresh));
        // Exactly at the window edge ⇒ still fresh (`<=`).
        let edge = HealthSnapshot {
            job_age_s: Some(GPU_WD_JOB_FRESH_SECS),
            ..Default::default()
        };
        assert!(jobs_flowing_from_health(&edge));
        // One second past the window ⇒ stale ⇒ the GPU watchdog stands down (the
        // reliability watchdog owns a truly stale-job link).
        let stale = HealthSnapshot {
            job_age_s: Some(GPU_WD_JOB_FRESH_SECS + 1),
            ..Default::default()
        };
        assert!(!jobs_flowing_from_health(&stale));
        assert!(!conn_healthy_from_health(&stale));
    }

    #[test]
    fn gpu_hashrate_sample_publishes_touches_and_reads() {
        let s = GpuHashrateSample::new();
        // No launch yet: pre-publish rate is INFINITY = "unknown, treat as healthy"
        // (was 0.0 which falsely read as floored — deploy-check C1). at_ms stays 0.
        let (rate0, at0) = s.read();
        assert!(rate0.is_infinite(), "pre-publish rate must be INFINITY (unknown=healthy), got {rate0}");
        assert_eq!(at0, 0);
        // A heartbeat updates only the timestamp; the rate is still the unpublished
        // INFINITY sentinel (touch never sets a rate).
        s.touch(5_000);
        let (rate1, at1) = s.read();
        assert!(rate1.is_infinite(), "touch must not publish a rate; still INFINITY, got {rate1}");
        assert_eq!(at1, 5_000);
        // A publish updates both — and from here read() MUST return the published
        // value, so a genuinely floored *measured* rate is still caught (the load-
        // bearing assertion: this is what detects a real hung-but-returning GPU).
        s.publish(2.5, 6_000);
        assert_eq!(s.read(), (2.5, 6_000));
        // A later heartbeat keeps the last published rate but advances the stamp.
        s.touch(7_000);
        assert_eq!(s.read(), (2.5, 7_000));
    }

    /// The live view reports the GPU as floored (0.0) when the loop's last
    /// heartbeat is older than `sample_stale_after_ms` — the kernel-wedged-inside-
    /// hash_range case — and reports a healthy sentinel before the first sample.
    #[test]
    fn loop_gpu_view_reports_stale_sample_as_floored() {
        // A standalone view over a hand-driven sample + a trivial work source /
        // backend. We only exercise `gpu_ghs()`'s freshness logic here.
        struct Noop;
        impl Recoverable for Noop {}
        let work = MockWorkSource::new(mock_job(), 1.0);
        let backend = Noop;
        let sample = Arc::new(GpuHashrateSample::new());
        let view = LoopGpuView {
            backend: &backend,
            client: &work,
            gpu_sample: Arc::clone(&sample),
            sample_stale_after_ms: 1_000,
            exit_on_escalate: false, // never kill the test runner
            thermal_gate: Arc::new(ThermalGate::new()), // never paused here
        };
        // Before any sample: healthy sentinel (not floored).
        assert!(view.gpu_ghs().is_infinite(), "no sample yet ⇒ not floored");
        // A fresh publish reads through as-is.
        sample.publish(3.0, now_unix_ms());
        let g = view.gpu_ghs();
        assert!((g - 3.0).abs() < 1e-9, "fresh sample reads its rate, got {g}");
        // An ancient heartbeat (2s ago > 1s stale window) ⇒ reported floored 0.0,
        // regardless of the published rate, so the watchdog can catch a wedged
        // kernel that stopped completing launches.
        sample.publish(3.0, now_unix_ms().saturating_sub(2_000));
        assert_eq!(view.gpu_ghs(), 0.0, "stale heartbeat ⇒ floored");
    }

    // --- THERMAL pause must NOT be misread as a hung GPU (the safety wire) ---

    /// A standalone [`LoopGpuView`] with a hand-driven thermal gate + a work
    /// source reporting FRESH jobs. The whole point: while the gate is PAUSED,
    /// `jobs_flowing()`/`conn_healthy()` must report FALSE (the GPU is idle by
    /// design), even though the work source says jobs are flowing — so the stall
    /// watchdog stands down. When the gate is NOT paused, they track the work
    /// source as normal.
    #[test]
    fn loop_gpu_view_thermal_pause_reports_not_flowing() {
        struct Noop;
        impl Recoverable for Noop {}
        // Fresh jobs ⇒ without a thermal pause, jobs_flowing()/conn_healthy() = true.
        let work = MockWorkSource::new(mock_job(), 1.0).with_fresh_jobs();
        let backend = Noop;
        let sample = Arc::new(GpuHashrateSample::new());
        let gate = Arc::new(ThermalGate::new());
        let view = LoopGpuView {
            backend: &backend,
            client: &work,
            gpu_sample: Arc::clone(&sample),
            sample_stale_after_ms: 1_000,
            exit_on_escalate: false,
            thermal_gate: Arc::clone(&gate),
        };
        // NOT paused: fresh jobs ⇒ both true (the watchdog could act on a real floor).
        assert!(view.jobs_flowing(), "fresh jobs + not paused ⇒ flowing");
        assert!(view.conn_healthy(), "fresh jobs + not paused ⇒ healthy");
        // PAUSE the gate (GPU over temperature limit): both go FALSE so the
        // watchdog treats the GPU as benign idle, never a stall.
        gate.apply(crate::thermal::ThermalState::Paused);
        assert!(!view.jobs_flowing(), "thermally paused ⇒ NOT flowing (idle by design)");
        assert!(!view.conn_healthy(), "thermally paused ⇒ NOT healthy-for-stall");
        // Resume: back to tracking the (fresh) work source.
        gate.apply(crate::thermal::ThermalState::Running);
        assert!(view.jobs_flowing(), "resumed ⇒ flowing again");
        assert!(view.conn_healthy(), "resumed ⇒ healthy again");
    }

    /// The end-to-end safety property in pure form: drive `gpu_watchdog_tick`
    /// (the real watchdog decision+state machine) against the live
    /// [`LoopGpuView`] while the GPU reads a FLOORED 0.0 GH/s for many samples,
    /// but the thermal gate is PAUSED. Because the paused view reports
    /// `jobs_flowing == false`, every tick must RESET the floored streak and
    /// return `Ok` — NEVER `Recover`/`Exit`. This is the test that proves an
    /// intentional thermal pause cannot trip the hung-GPU exit(17).
    #[test]
    fn thermal_pause_prevents_watchdog_from_flooring_a_paused_gpu() {
        struct Noop;
        impl Recoverable for Noop {}
        let work = MockWorkSource::new(mock_job(), 1.0).with_fresh_jobs();
        let backend = Noop;
        let sample = Arc::new(GpuHashrateSample::new());
        // Publish a genuinely floored, FRESH sample: absent the thermal pause this
        // would (with jobs flowing) be a textbook stall the watchdog acts on.
        sample.publish(0.0, now_unix_ms());
        let gate = Arc::new(ThermalGate::new());
        gate.apply(crate::thermal::ThermalState::Paused); // GPU paused for temperature
        let view = LoopGpuView {
            backend: &backend,
            client: &work,
            gpu_sample: Arc::clone(&sample),
            sample_stale_after_ms: 60_000, // keep the sample "fresh" for the test
            exit_on_escalate: false,       // never kill the runner even on a bug
            thermal_gate: Arc::clone(&gate),
        };
        // A watchdog cfg that would act FAST on a real floor (dwell 1, recovery on).
        let cfg = GpuWatchdogCfg {
            enabled: true,
            floor_ghs: 0.001,
            dwell_samples: 1,
            recover_window: Duration::from_secs(3600),
            max_recoveries: 3,
            poll: Duration::from_millis(5),
        };
        let mut st = GpuWatchdogState::default();
        let mut now = now_unix_ms();
        for _ in 0..20 {
            let action = gpu_watchdog_tick(&view, cfg, &mut st, now);
            assert_eq!(
                action,
                crate::gpu_watchdog::GpuWatchdogAction::Ok,
                "a thermally-paused GPU must never be acted on (no Recover/Exit)"
            );
            // The streak must stay reset (paused ⇒ not-flowing ⇒ reset every tick).
            assert_eq!(st.floored_streak, 0, "paused ⇒ floored streak stays reset");
            now += 15_000;
        }
        // Now RESUME the gate and keep the SAME floored sample fresh: the watchdog
        // must once again be able to see the floor (jobs flowing) and act — proving
        // the stand-down was due to the pause, not a permanently-broken watchdog.
        gate.apply(crate::thermal::ThermalState::Running);
        sample.publish(0.0, now); // still floored, fresh
        let action = gpu_watchdog_tick(&view, cfg, &mut st, now);
        assert_eq!(
            action,
            crate::gpu_watchdog::GpuWatchdogAction::Recover,
            "once resumed, a real floor with fresh jobs is actionable again"
        );
    }

    /// Drive the REAL `run_stratum_with_gpu_watchdog` with the watchdog ENABLED
    /// but a backend that hashes fine (never stalls): it must map the job,
    /// dispatch the backend, run the watchdog thread alongside, and shut down
    /// cleanly on `stop` — never recovering, never exiting. This is the
    /// end-to-end proof that arming the watchdog doesn't disturb the happy path
    /// (the stall→recover→exit ladder itself is unit-tested in the gpu_watchdog
    /// module against its MockView).
    #[test]
    fn run_with_gpu_watchdog_enabled_happy_path_shuts_down_clean() {
        let work = Arc::new(MockWorkSource::new(mock_job(), 1024.0));
        let backend = Arc::new(MockBackend::new(vec![])); // never finds, blocks on stop
        let stop = Arc::new(AtomicBool::new(false));

        let work_for_loop = Arc::clone(&work);
        let backend_for_loop = Arc::clone(&backend);
        let stop_for_loop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let cfg = MiningConfig { cpu_threads: 0, cpu_share: 0.0 };
            // Watchdog ON, but a generous floor of 0 + fast poll: since the mock
            // GPU blocks in hash_range (no sample ever published, `at_ms == 0`),
            // the view reports the INFINITY sentinel ⇒ never floored ⇒ no action.
            let gpu_wd = GpuWatchdogCfg {
                enabled: true,
                poll: Duration::from_millis(5),
                dwell_samples: 1,
                ..GpuWatchdogCfg::default()
            };
            run_stratum_with_gpu_watchdog(
                backend_for_loop.as_ref(),
                work_for_loop.as_ref(),
                stop_for_loop,
                cfg,
                gpu_wd,
            )
        });

        // Wait for the backend to be dispatched, let the watchdog tick a few
        // times, then stop.
        for _ in 0..300 {
            if backend.calls.load(Ordering::Relaxed) > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(60)); // let the watchdog poll
        stop.store(true, Ordering::Relaxed);
        let result = handle.join().expect("loop thread did not panic");
        assert!(result.is_ok(), "run_stratum_with_gpu_watchdog returns Ok on clean shutdown");
        assert!(
            backend.calls.load(Ordering::Relaxed) > 0,
            "the loop must have dispatched the backend alongside the armed watchdog"
        );
        assert!(
            work.submits.lock().unwrap().is_empty(),
            "no share at diff 1024 in the brief window"
        );
    }

    /// deploy-check C2 coverage: drive the REAL `run_stratum_with_gpu_watchdog`
    /// with the watchdog ARMED and a RETURNING backend (finite sweep, finds
    /// nothing, `hash_range` returns immediately) — the realistic path the other
    /// happy-path test misses (that one BLOCKS in `hash_range`, so it only
    /// exercises the `at_ms == 0` sentinel). Here the loop re-dispatches every
    /// iteration and `touch()`es the GPU sample, so `at_ms != 0` and the sample is
    /// fresh: `gpu_ghs()` reads the INFINITY-seeded rate (a returning, share-heavy
    /// GPU that never hit the 10s publish window reads UNKNOWN = healthy, NOT
    /// floored — exactly the C1 false-floor case). With jobs flowing + link
    /// healthy, the watchdog must take NO recover / NO exit over several poll
    /// cycles. `exit_on_escalate` is true in production, but here a wrong floor
    /// would surface as a recover() (the CPU/no-op backend's recover is a no-op,
    /// so it would loop Recover, never exit the runner) — we assert it does not by
    /// proving a clean Ok shutdown after many polls. Fast: no 10s sleep.
    ///
    // TODO(deploy-check C2): the HEALTHY-MEASURED-RATE publish sub-path — where
    // the loop's `None` arm calls `gpu_sample.publish(ghs_gpu, ..)` with a real
    // >floor rate after the 10s `last_hashrate_log` interval — is NOT exercised
    // here because that interval is hardcoded (`Duration::from_secs(10)`, no
    // injection seam), so reaching it needs a >10s wall-clock run. The measured
    // publish→read path is unit-covered by `loop_gpu_view_reports_stale_sample_as_floored`
    // (fresh publish reads its rate through) + the gpu_watchdog pure tests
    // (`healthy_gpu_is_ok` / `tick_healthy_resets_state_and_does_nothing`). If the
    // 10s interval ever gains a config seam, fold a measured-rate assertion in here.
    #[test]
    fn run_with_gpu_watchdog_returning_backend_is_not_floored() {
        // FRESH jobs so jobs_flowing() + conn_healthy() are TRUE — this is what
        // ARMS the watchdog's actionable-stall gate. Without it the watchdog
        // stands down every tick and the test would prove nothing (a default
        // MockWorkSource reports job_age=None ⇒ never actionable).
        let work = Arc::new(MockWorkSource::new(mock_job(), 1024.0).with_fresh_jobs());
        // Returning backend: finishes its sweep finding nothing and RETURNS each
        // launch (does not block) — drives the realistic re-dispatch + touch path.
        let backend = Arc::new(MockBackend::new_returning());
        let stop = Arc::new(AtomicBool::new(false));

        let work_for_loop = Arc::clone(&work);
        let backend_for_loop = Arc::clone(&backend);
        let stop_for_loop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let cfg = MiningConfig { cpu_threads: 0, cpu_share: 0.0 };
            // Watchdog ON, tiny poll + dwell so MANY ticks happen in the brief
            // window: if a returning backend were wrongly read as floored, the
            // dwell-1 + fast poll would force a Recover within a few ms.
            let gpu_wd = GpuWatchdogCfg {
                enabled: true,
                poll: Duration::from_millis(5),
                dwell_samples: 1,
                max_recoveries: 3,
                recover_window: Duration::from_secs(3600),
                ..GpuWatchdogCfg::default()
            };
            run_stratum_with_gpu_watchdog(
                backend_for_loop.as_ref(),
                work_for_loop.as_ref(),
                stop_for_loop,
                cfg,
                gpu_wd,
            )
        });

        // Wait for the backend to be dispatched (it returns fast, so calls climb
        // quickly), then let the watchdog poll many times before stopping.
        for _ in 0..300 {
            if backend.calls.load(Ordering::Relaxed) > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // ~40 poll intervals at 5ms — far past dwell=1, so a false-floor would
        // have fired a recover() by now.
        std::thread::sleep(Duration::from_millis(200));
        // The PRIMARY C1 guard: a returning/healthy backend must never be read as
        // floored, so the watchdog must NOT have called recover() even once. (With
        // the old 0.0 seed it WOULD: touch() stamps at_ms!=0, gpu_ghs() reads 0.0
        // = floored, dwell=1 ⇒ Recover within a few ms — this assert catches that.)
        // Sampled before stop so a late tick can't race the join.
        let recovers = backend.recover_calls.load(Ordering::Relaxed);
        stop.store(true, Ordering::Relaxed);
        let result = handle.join().expect("loop thread did not panic");
        assert_eq!(
            recovers, 0,
            "watchdog wrongly treated a returning (healthy) backend as a stalled \
             GPU and called recover() {recovers}x — the C1 false-floor"
        );
        assert!(
            result.is_ok(),
            "run_stratum_with_gpu_watchdog returns Ok — a returning backend must \
             NOT be read as a stalled GPU (no exit(17))"
        );
        // The returning backend was re-dispatched many times (not a single
        // blocking launch) — proves we exercised the returns-from-hash_range path.
        assert!(
            backend.calls.load(Ordering::Relaxed) > 1,
            "a returning backend is re-dispatched every iteration, got {} calls",
            backend.calls.load(Ordering::Relaxed)
        );
        assert!(
            work.submits.lock().unwrap().is_empty(),
            "no share at diff 1024 in the brief window"
        );
    }

    /// Drive the REAL `run_stratum_full` with the thermal gate PAUSED from the
    /// start: the loop must SKIP GPU launches (never dispatch the backend) while
    /// paused, and shut down cleanly on `stop`. Then a second run with the gate
    /// un-paused proves the same setup DOES dispatch — so the skip is caused by
    /// the pause, not by a wiring mistake.
    #[test]
    fn thermal_paused_loop_skips_gpu_launches() {
        // --- paused: backend must NOT be dispatched ---
        let work = Arc::new(MockWorkSource::new(mock_job(), 1024.0));
        let backend = Arc::new(MockBackend::new(vec![]));
        let stop = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(ThermalGate::new());
        gate.apply(crate::thermal::ThermalState::Paused); // start paused (GPU hot)

        let work_for_loop = Arc::clone(&work);
        let backend_for_loop = Arc::clone(&backend);
        let stop_for_loop = Arc::clone(&stop);
        let gate_for_loop = Arc::clone(&gate);
        let handle = std::thread::spawn(move || {
            let cfg = MiningConfig { cpu_threads: 0, cpu_share: 0.0 };
            run_stratum_full(
                backend_for_loop.as_ref(),
                work_for_loop.as_ref(),
                stop_for_loop,
                cfg,
                GpuWatchdogCfg { enabled: false, ..GpuWatchdogCfg::default() },
                gate_for_loop,
            )
        });
        // Give the loop ample time to spin: while paused it must keep skipping.
        std::thread::sleep(Duration::from_millis(200));
        let calls_while_paused = backend.calls.load(Ordering::Relaxed);
        stop.store(true, Ordering::Relaxed);
        assert!(handle.join().expect("no panic").is_ok());
        assert_eq!(
            calls_while_paused, 0,
            "a thermally-paused loop must NOT dispatch the GPU backend, got {calls_while_paused} calls"
        );
        assert!(work.submits.lock().unwrap().is_empty(), "paused ⇒ no submits");

        // --- not paused (same setup): backend IS dispatched (control) ---
        let work2 = Arc::new(MockWorkSource::new(mock_job(), 1024.0));
        let backend2 = Arc::new(MockBackend::new(vec![]));
        let stop2 = Arc::new(AtomicBool::new(false));
        let gate2 = Arc::new(ThermalGate::new()); // never paused
        let work2_l = Arc::clone(&work2);
        let backend2_l = Arc::clone(&backend2);
        let stop2_l = Arc::clone(&stop2);
        let gate2_l = Arc::clone(&gate2);
        let handle2 = std::thread::spawn(move || {
            let cfg = MiningConfig { cpu_threads: 0, cpu_share: 0.0 };
            run_stratum_full(
                backend2_l.as_ref(),
                work2_l.as_ref(),
                stop2_l,
                cfg,
                GpuWatchdogCfg { enabled: false, ..GpuWatchdogCfg::default() },
                gate2_l,
            )
        });
        for _ in 0..300 {
            if backend2.calls.load(Ordering::Relaxed) > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop2.store(true, Ordering::Relaxed);
        assert!(handle2.join().expect("no panic").is_ok());
        assert!(
            backend2.calls.load(Ordering::Relaxed) > 0,
            "the un-paused control run MUST dispatch the backend (proves the skip is the pause)"
        );
    }

    /// A thermal pause that LIFTS mid-run: start paused (no dispatch), then
    /// un-pause the gate — the loop must resume and dispatch the backend. Proves
    /// resume actually re-enables launches (not a one-way latch).
    #[test]
    fn thermal_resume_re_enables_gpu_launches() {
        let work = Arc::new(MockWorkSource::new(mock_job(), 1024.0));
        let backend = Arc::new(MockBackend::new(vec![]));
        let stop = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(ThermalGate::new());
        gate.apply(crate::thermal::ThermalState::Paused);

        let work_l = Arc::clone(&work);
        let backend_l = Arc::clone(&backend);
        let stop_l = Arc::clone(&stop);
        let gate_l = Arc::clone(&gate);
        let handle = std::thread::spawn(move || {
            let cfg = MiningConfig { cpu_threads: 0, cpu_share: 0.0 };
            run_stratum_full(
                backend_l.as_ref(),
                work_l.as_ref(),
                stop_l,
                cfg,
                GpuWatchdogCfg { enabled: false, ..GpuWatchdogCfg::default() },
                gate_l,
            )
        });
        // Paused first: no dispatch.
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(backend.calls.load(Ordering::Relaxed), 0, "paused ⇒ no dispatch yet");
        // Lift the pause: the loop must start dispatching.
        gate.apply(crate::thermal::ThermalState::Running);
        let mut dispatched = false;
        for _ in 0..300 {
            if backend.calls.load(Ordering::Relaxed) > 0 {
                dispatched = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        assert!(handle.join().expect("no panic").is_ok());
        assert!(dispatched, "resuming the gate must re-enable GPU launches");
    }

    /// With the watchdog DISABLED (the `run_stratum` shim's path), behaviour is
    /// byte-identical to the pre-watchdog loop: dispatch + clean shutdown, the
    /// watchdog thread exits immediately.
    #[test]
    fn run_stratum_shim_disables_gpu_watchdog() {
        let work = Arc::new(MockWorkSource::new(mock_job(), 1024.0));
        let backend = Arc::new(MockBackend::new(vec![]));
        let stop = Arc::new(AtomicBool::new(false));

        let work_for_loop = Arc::clone(&work);
        let backend_for_loop = Arc::clone(&backend);
        let stop_for_loop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let cfg = MiningConfig { cpu_threads: 0, cpu_share: 0.0 };
            run_stratum(backend_for_loop.as_ref(), work_for_loop.as_ref(), stop_for_loop, cfg)
        });
        for _ in 0..300 {
            if backend.calls.load(Ordering::Relaxed) > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        assert!(handle.join().expect("no panic").is_ok());
        assert!(backend.calls.load(Ordering::Relaxed) > 0);
    }
}
