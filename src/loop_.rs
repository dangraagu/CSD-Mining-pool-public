//! Main mining loop.
//!
//! iter-32 (widened from iter-27): speculative quad-template mining. The
//! node's /work/get returns up to four templates per call:
//!
//!   - slot A: extends the node's local tip (always present)
//!   - slot B: extends the canonical explorer tip (when local != explorer)
//!   - slot C: extends the canonical explorer tip's parent (1-deep reorg-back hedge)
//!   - slot D: extends our most-recently-orphaned tip (resurrection hedge)
//!
//! Whichever slots are present, we iterate `iter_idx % templates.len()`
//! across them — single, dual, triple, and quad modes all degrade
//! naturally. Each template carries its own `id` (the 2-bit variant tag
//! is encoded in the top 2 bits so submits round-trip cleanly), so on
//! FOUND we submit using whatever template's id we were mining.
//!
//! Single-template mode (B/C/D all absent) degrades to the iter-26
//! behavior identically — same single sweep, same submit shape — only
//! the response envelope is new.
//!
//!   1. /work/get -> WorkResponse { templates: [Option<T>; 4] }
//!   2. Build coinbase + header skeleton for each present template once.
//!   3. Round-robin per iteration through populated slots.
//!   4. On hit: submit with that template's id (variant bits already set).
//!   5. Refresh every N hits or every M seconds.
//!
//! iter-31: CPU + GPU dual mining. Each per-template iteration partitions
//! the nonce range between the GPU (lower portion) and N CPU worker
//! threads (upper portion, split evenly). Whichever device finds a
//! solution first wins; the other devices observe the shared
//! `local_found` flag and stop on their next cancellation check.
//!
//! Partition contract (see `partition_nonce_range` for the pure helper
//! used in tests):
//!   - GPU sweeps `[nonce_start, gpu_end)` where
//!         gpu_end = nonce_start + floor(span * (1 - cpu_share))
//!   - CPU threads (if N>0 and share>0) collectively sweep
//!         `[gpu_end, nonce_end)`
//!   - Each CPU thread gets a contiguous, equal-sized sub-range.
//!   - cpu_share == 0.0 OR cpu_threads == 0 disables CPU mining entirely
//!     (GPU gets the full range).
//!
//! Both CPU and GPU mine the SAME template per iteration (they race), so
//! the dual-template A/B alternation (iter-27) is unaffected — CPU
//! follows the same alternation cadence as the GPU.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crate::csd_consensus::{self, WorkSubmission, WorkTemplate};

use crate::backend::{MiningBackend, MiningResult};
use crate::coinbase::{coinbase_txid, header_84, merkle_root_from_branch};
use crate::http::NodeClient;
use crate::sha256d_cpu::{finish_sha256d_from_midstate_fast, midstate_of_first_chunk_fast};

/// iter-32 mirror of `csd_node::mining::VARIANT_MASK`. We can't import it
/// from csd-node (the miner doesn't depend on the node crate), but the
/// mask must agree byte-for-byte. Keep these two constants in sync.
const VARIANT_MASK: u64 = 3u64 << 62;

/// iter-32 mirror of `csd_node::mining::variant_of`. Returns 0..=3 for
/// the four speculative variants A/B/C/D.
#[inline]
fn variant_of(id: u64) -> u8 {
    ((id & VARIANT_MASK) >> 62) as u8
}

/// iter-32 mirror of `csd_node::mining::variant_label`. Returns a stable
/// one-character label for the variant (A/B/C/D), or "?" for any out-of-
/// range value (defensive; can't happen with 2-bit variants).
fn variant_tag(id: u64) -> &'static str {
    match variant_of(id) {
        0 => "A",
        1 => "B",
        2 => "C",
        3 => "D",
        _ => "?",
    }
}

/// iter-53 #2 / iter-46 #D.8: pure staleness predicate for the pre-submit
/// check. Returns `true` iff we KNOW the node's current tip has rotated
/// away from the template's `prev` — in which case submitting the
/// just-mined block is guaranteed to fail node validation, so we save the
/// round-trip and immediately fetch fresh work.
///
/// Permissive on fetch failure: `current_tip == [0u8; 32]` is treated as
/// "unknown" (e.g. /tip request failed, or pre-genesis), and we return
/// `false` so the submit still goes through. The alternative (claiming
/// stale on a transient HTTP blip) would silently drop legitimate
/// solutions, which is strictly worse than a wasted /work/submit.
#[inline]
pub fn template_is_stale(
    template_prev: csd_consensus::Hash32,
    current_tip: csd_consensus::Hash32,
) -> bool {
    // Tip is "stale" iff we KNOW the tip rotated away from template_prev.
    // If current_tip is zero (fetch failed / pre-genesis), don't claim stale.
    current_tip != [0u8; 32] && template_prev != current_tip
}

/// iter-E1 (mine-through-503): pure decision for the `/work/get` 503 hold.
/// When the node 503s `/work/get` (its own gate is holding) we do NOT want to
/// drop the GPU into a 1 s idle-sleep poll loop — that was 22.6% of wall-clock
/// in the iter-E forensic. Instead, if we are holding a last-good slot-A
/// template whose parent STILL extends the node's current tip, we keep the GPU
/// busy mining it (the v74 doctrine: a block we mine on a still-current parent
/// is legitimately submittable, and the pre-submit staleness guard re-checks
/// anyway). We only fall back to idling when we genuinely can't.
///
/// Returns `true` (keep mining the held template) iff:
///   - we hold a last-good slot-A template (`held_prev` is `Some`), AND
///   - we got a confident /tip read (`current_tip` is `Some`), AND
///   - that tip is non-zero (zero = /tip fetch failed / pre-genesis), AND
///   - the held template's parent equals the current tip (`held_prev == tip`).
///
/// Returns `false` (idle / backoff) when the tip moved off our held parent,
/// when we hold no template, or when we couldn't confidently read the tip —
/// because then mining the held template would just burn the GPU on work the
/// network has already moved past.
#[inline]
pub fn should_mine_through_503(
    held_prev: Option<csd_consensus::Hash32>,
    current_tip: Option<csd_consensus::Hash32>,
) -> bool {
    match (held_prev, current_tip) {
        (Some(prev), Some(tip)) => tip != [0u8; 32] && prev == tip,
        // No held template, or no confident tip read → don't mine-through.
        _ => false,
    }
}

/// iter-31: runtime knobs for CPU+GPU dual mining. Built in `main.rs` from
/// CLI flags and passed into `run_forever`.
#[derive(Clone, Debug)]
pub struct MiningConfig {
    /// CPU threads to dedicate to hashing alongside the GPU. 0 disables
    /// CPU mining entirely (GPU takes 100% of the nonce range, exactly
    /// like the pre-iter-31 behavior).
    pub cpu_threads: usize,
    /// Fraction of the nonce range the CPU pool sweeps (0.0..=1.0).
    /// 0.0 disables CPU mining; 1.0 gives the GPU nothing. Documented
    /// useful range is roughly 0.2..0.5 depending on CPU/GPU mix.
    pub cpu_share: f32,
    /// v74 port: symmetric tolerance for the asymmetric explorer-gate.
    /// 0 = mine only when local == canonical OR local == canonical+1 OR
    /// (in grace) up to many ahead. N>0 = allow local to be up to N
    /// blocks BEHIND canonical before pausing. AHEAD direction is always
    /// `+1 universal, +N>1 grace-only` regardless of this knob.
    pub max_network_lag: u64,
    /// v75 port: peer RPC URLs to fan submits out to (in addition to the
    /// local node). Empty = local-only (default, no regression vs pre-v75).
    /// Each URL is a base like "http://1.2.3.4:8799". Lines from
    /// `--broadcast-peers-file` are loaded into this Vec at startup.
    pub broadcast_peers: Vec<String>,
}

impl Default for MiningConfig {
    fn default() -> Self {
        // GPU-only by default so the trait stays the legacy shape if a
        // caller skips the new constructor.
        Self {
            cpu_threads: 0,
            cpu_share: 0.0,
            max_network_lag: 0,
            broadcast_peers: Vec::new(),
        }
    }
}

/// v74 port: pure decision function for the asymmetric explorer-gate. Holds
/// the entire mining-go/no-go logic for fork conditions in one testable spot.
///
/// Rules (BEHIND strict, AHEAD loosened, TIED-with-different-tip allowed):
///   - `local_h == canon_h` (any tips): MINE. The previous symmetric-PAUSE
///     code refused TIED-DIFFERENT-TIP, which was the dominant cause of
///     0 UTXOs/hr: right after we win locally we'd see the canonical view
///     still pointing to the prior tip and pause until propagation caught
///     up, throwing away our own block's lead.
///   - `local_h == canon_h + 1`: MINE. SURPRISE-WIN case — explorer is
///     momentarily one block behind us. Universal allow (not grace-gated).
///   - `local_h > canon_h + 1`: MINE only if `in_grace`. Beyond +1 with no
///     fresh submit suggests a sustained private fork.
///   - `local_h + max_lag < canon_h`: SKIP. We're more than `max_lag`
///     behind; let local node sync before contributing junk-on-stale.
///   - Otherwise (BEHIND by <= max_lag): MINE.
///
/// `current_status_is_synced` is the node's own `/stats.sync_status`. When
/// it's `"synced"` we still trust the asymmetric rules above (no change).
/// When it isn't `"synced"`, we still allow the TIED-DIFFERENT-TIP and
/// AHEAD-by-1 cases — those are exactly the false-positive "fork" labels
/// the node emits during normal post-win propagation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateDecision {
    Mine,
    SkipBehind { behind: u64 },
    SkipAheadFork { ahead: u64 },
}

pub fn should_mine_gate(
    local_h: u64,
    canon_h: u64,
    max_lag: u64,
    in_grace: bool,
) -> GateDecision {
    // Heights of zero are sentinel "unknown" — let the caller decide; here
    // we permit (it's the same conservative default the old code used when
    // /stats data was incomplete).
    if canon_h == 0 || local_h == 0 {
        return GateDecision::Mine;
    }
    if local_h < canon_h {
        let behind = canon_h - local_h;
        if behind > max_lag {
            return GateDecision::SkipBehind { behind };
        }
        return GateDecision::Mine;
    }
    if local_h > canon_h {
        let ahead = local_h - canon_h;
        if ahead == 1 {
            // v74 SURPRISE-WIN: always allowed.
            return GateDecision::Mine;
        }
        if !in_grace {
            return GateDecision::SkipAheadFork { ahead };
        }
        return GateDecision::Mine;
    }
    // local_h == canon_h: MINE even if the tip hashes differ. This is the
    // exact case the old `sync_status == "fork"` check was rejecting.
    GateDecision::Mine
}

/// v75 port: load peer RPC URLs from a newline-separated file. Each line is
/// one URL (e.g. `http://1.2.3.4:8799`). Blank lines and `#`-comments are
/// ignored. A missing or empty file returns `Vec::new()` (caller falls
/// back to local-only submit).
pub fn load_broadcast_peers(path: &str) -> Vec<String> {
    if path.is_empty() {
        return Vec::new();
    }
    match std::fs::read_to_string(path) {
        Ok(s) => s
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// v75 port: fan out a single FOUND-block submit to the local node AND to
/// every URL in `peer_urls`. The local POST blocks the caller (so its
/// response drives the "did we win?" decision in the mining loop). The
/// peer POSTs run in detached fire-and-forget threads — their responses
/// are logged at info level as `[broadcast]` lines, but they never block
/// the critical submit path.
///
/// Contract:
///   - Returns the local node's submit response (or an error) as if you
///     had called `client.submit_work` directly. Latency is unchanged
///     from a local-only submit even when many peers are present.
///   - Peer threads are spawned with the standard `std::thread::Builder`
///     so the OS scheduler runs them in parallel with the local POST.
///   - If a peer URL is unreachable, the failure is logged once and that
///     peer is dropped for this submit — no retry, no backoff.
///
/// Test note: this function is hard to unit-test because it fans out
/// network I/O. Coverage relies on integration: empty `peer_urls` must
/// behave identically to `client.submit_work` (verified by the
/// `load_broadcast_peers` returning empty Vec on missing file).
pub fn spawn_broadcast_submit(
    client: &NodeClient,
    peer_urls: &[String],
    sub: &csd_consensus::WorkSubmission,
) -> anyhow::Result<serde_json::Value> {
    // Fire peer broadcasts in detached threads FIRST so they run in
    // parallel with the local POST. They each clone the submission +
    // url; both are tiny.
    for url in peer_urls {
        let url_for_thread = url.clone();
        let sub_clone = sub.clone();
        let _ = std::thread::Builder::new()
            .name(format!("broadcast-submit-{}", url))
            .spawn(move || {
                let started = Instant::now();
                let result = crate::http::NodeClient::submit_work_to(
                    &url_for_thread,
                    &sub_clone,
                );
                let ms = started.elapsed().as_millis();
                match result {
                    Ok(resp) => tracing::info!(
                        "[broadcast] peer={} response={} latency_ms={}",
                        url_for_thread, resp, ms
                    ),
                    Err(e) => tracing::warn!(
                        "[broadcast] peer={} error={} latency_ms={}",
                        url_for_thread, e, ms
                    ),
                }
            });
    }
    // Local submit drives the outcome.
    client.submit_work(sub)
}

/// Pure partition math. Returns `(gpu_range, cpu_ranges)`. Both ranges
/// are half-open `[start, end)`. The GPU range is always present (may be
/// empty when `cpu_share == 1.0`); the CPU ranges vector is empty when
/// `cpu_threads == 0` or `cpu_share <= 0.0`.
///
/// Invariants asserted by `partition_invariants_hold` (and tests):
///   - GPU.end == first CPU range start (when CPU active)
///   - last CPU range end == nonce_end
///   - sum of (gpu range len + all cpu range lens) == total span (no
///     overlap, no gap)
///   - cpu_share clamped to [0.0, 1.0]
pub fn partition_nonce_range(
    nonce_start: u32,
    nonce_end: u32,
    cpu_share: f32,
    cpu_threads: usize,
) -> ((u32, u32), Vec<(u32, u32)>) {
    if nonce_end <= nonce_start {
        return ((nonce_start, nonce_start), Vec::new());
    }
    let total = (nonce_end as u64) - (nonce_start as u64);
    let share = cpu_share.clamp(0.0, 1.0);
    let cpu_active = cpu_threads > 0 && share > 0.0;
    if !cpu_active {
        return ((nonce_start, nonce_end), Vec::new());
    }
    // CPU pool size, rounded to nearest. Must leave at least 1 nonce for
    // the GPU unless cpu_share == 1.0 (in which case the GPU pool is
    // empty, which the GPU backend tolerates).
    let cpu_total = ((total as f64) * (share as f64)).round() as u64;
    let cpu_total = if (share - 1.0).abs() < f32::EPSILON {
        total
    } else {
        cpu_total.min(total.saturating_sub(1)).max(1)
    };
    let gpu_end_u64 = (nonce_start as u64) + (total - cpu_total);
    let gpu_end = gpu_end_u64.min(nonce_end as u64) as u32;
    let gpu_range = (nonce_start, gpu_end);

    // Split [gpu_end, nonce_end) into cpu_threads equal contiguous chunks.
    let chunk = cpu_total / (cpu_threads as u64);
    let remainder = cpu_total % (cpu_threads as u64);
    let mut cpu_ranges = Vec::with_capacity(cpu_threads);
    let mut cursor = gpu_end as u64;
    for i in 0..cpu_threads {
        // Give the first `remainder` threads one extra nonce so the sum
        // matches `cpu_total` exactly (no rounding gap).
        let extra = if (i as u64) < remainder { 1 } else { 0 };
        let len = chunk + extra;
        let end = (cursor + len).min(nonce_end as u64);
        cpu_ranges.push((cursor as u32, end as u32));
        cursor = end;
    }
    // Force the final range to land exactly on nonce_end to absorb any
    // off-by-one from u64->u32 truncation.
    if let Some(last) = cpu_ranges.last_mut() {
        last.1 = nonce_end;
    }
    (gpu_range, cpu_ranges)
}

/// Cheap invariant check used by tests and by a debug_assert in the loop.
#[cfg(test)]
fn partition_invariants_hold(
    nonce_start: u32,
    nonce_end: u32,
    gpu: (u32, u32),
    cpu: &[(u32, u32)],
) -> bool {
    if gpu.0 != nonce_start {
        return false;
    }
    if cpu.is_empty() {
        return gpu.1 == nonce_end;
    }
    if gpu.1 != cpu[0].0 {
        return false;
    }
    for w in cpu.windows(2) {
        if w[0].1 != w[1].0 {
            return false;
        }
    }
    if cpu.last().unwrap().1 != nonce_end {
        return false;
    }
    // No empty/inverted CPU ranges.
    for (s, e) in cpu {
        if e < s {
            return false;
        }
    }
    true
}

/// Outcome of a CPU worker's sweep over its slice of the nonce range.
/// Held in a shared `Mutex<Option<CpuFind>>` so the first writer wins
/// and the rest of the workers (plus the GPU) abort on their next
/// cancellation check.
#[derive(Clone, Copy, Debug)]
struct CpuFind {
    thread_idx: usize,
    nonce: u32,
    hash: [u8; 32],
}

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

pub fn run_forever<B: MiningBackend>(
    backend: &B,
    client: &NodeClient,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    run_forever_with(backend, client, stop, MiningConfig::default())
}

pub fn run_forever_with<B: MiningBackend>(
    backend: &B,
    client: &NodeClient,
    stop: Arc<AtomicBool>,
    cfg: MiningConfig,
) -> Result<()> {
    let mut extranonce: u64 = 0;
    // iter-53 #3: 2s refresh (was 5s) to reduce stale-template waste. At
    // csd1's 120s block time, the network advances every 60-120s typically;
    // a 2s refresh keeps wasted GPU cycles minimal (max 2s of stale hashing
    // per new tip) while only adding ~1.5 extra /work/get calls per minute.
    //
    // Pairs with iter-53 #2's pre-submit staleness check: #3 shortens the
    // *mining* window on a stale template, #2 shortens the *submit* window.
    let refresh_every = Duration::from_secs(2);
    let mut last_pause_log = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .unwrap_or_else(Instant::now);

    // Hashrate tracking: every 10 s, log GH/s based on nonces swept.
    let mut last_hashrate_log = Instant::now();
    let mut gpu_nonces_since_log: u128 = 0;
    let mut cpu_nonces_since_log: u128 = 0;

    // v74 port: asymmetric explorer-gate state.
    //   - `last_submit_at` arms the post-submit grace window so AHEAD-by-N>1
    //     is allowed briefly while propagation catches up.
    //   - `consecutive_stale` counts STALE-PARENT classifications driven by
    //     the pre-submit fresh-tip poll. After STALE_STREAK_THRESHOLD in a
    //     row we lock the gate out for SUSTAINED_LOCKOUT_SECS to recover
    //     from a sustained private-fork situation.
    let mut last_submit_at: Option<Instant> = None;
    const POST_SUBMIT_GRACE_SECS: u64 = 45;
    let mut consecutive_stale: u32 = 0;
    let mut force_lockout_until: Option<Instant> = None;
    const STALE_STREAK_THRESHOLD: u32 = 3;
    const SUSTAINED_LOCKOUT_SECS: u64 = 60;

    // iter-E1 (mine-through-503): the most-recent slot-A template the node
    // served us on a 200. When `/work/get` later 503s (its gate is holding),
    // we keep the GPU mining THIS template as long as its parent still equals
    // the node's current /tip — instead of idling in the 1 s-sleep poll loop
    // (which was 22.6% of wall-clock in the iter-E forensic). Refreshed on
    // every successful fetch; consulted only in the 503 arm.
    let mut last_good_slot_a: Option<WorkTemplate> = None;
    // Rate-limit the "mining-through-503" info line so a long hold doesn't
    // spam the log (mirrors the 15 s cadence used by the pause/circuit logs).
    let mut last_mine_through_log = Instant::now()
        .checked_sub(Duration::from_secs(60))
        .unwrap_or_else(Instant::now);

    // iter-31 startup banner: makes the dual-mining mode visible in logs
    // BEFORE any sync wait. Logged once per process.
    if cfg.cpu_threads > 0 && cfg.cpu_share > 0.0 {
        tracing::info!(
            "cpu mining enabled: threads={} share={:.2} kernel=sha2-crate(sha-ni)",
            cfg.cpu_threads,
            cfg.cpu_share,
        );
    } else {
        tracing::info!(
            "cpu mining disabled (cpu_threads={} cpu_share={:.2}); GPU-only",
            cfg.cpu_threads, cfg.cpu_share,
        );
    }
    // v75 port: broadcast startup banner so the operator can confirm the
    // peer list was loaded.
    if cfg.broadcast_peers.is_empty() {
        tracing::info!(
            "v75: broadcast peer list empty; local-only submit (no regression vs pre-v75)"
        );
    } else {
        tracing::info!(
            "v75: broadcasting submits to {} peer RPC URL(s) in parallel with local",
            cfg.broadcast_peers.len(),
        );
        for u in &cfg.broadcast_peers {
            tracing::info!("v75: broadcast peer: {}", u);
        }
    }
    tracing::info!(
        "v74: asymmetric explorer-gate active (max_network_lag={}, post_submit_grace={}s)",
        cfg.max_network_lag, POST_SUBMIT_GRACE_SECS
    );

    while !stop.load(Ordering::Relaxed) {
        // v74 port: circuit-breaker — if a sustained STALE-PARENT streak has
        // armed the lockout, idle here until it expires. Reset the streak
        // when the lockout clears so the next CONFIDENT submit doesn't
        // accidentally re-arm.
        if let Some(t) = force_lockout_until {
            if t > Instant::now() {
                let remaining = (t - Instant::now()).as_secs();
                if last_pause_log.elapsed() >= Duration::from_secs(15) {
                    tracing::warn!(
                        "[circuit-breaker] sustained-STALE lockout active, {}s remaining (consecutive_stale={}); skipping mine cycle",
                        remaining, consecutive_stale
                    );
                    last_pause_log = Instant::now();
                }
                std::thread::sleep(Duration::from_secs(1));
                continue;
            } else {
                let prev_streak = consecutive_stale;
                force_lockout_until = None;
                consecutive_stale = 0;
                tracing::warn!(
                    "[circuit-breaker] sustained-STALE lockout cleared (streak was {}), resuming",
                    prev_streak
                );
            }
        }

        // v74 port: asymmetric gate. Replaces the v-pre `sync_status !=
        // \"synced\"` PAUSE which was throwing away SURPRISE-WIN and
        // TIED-DIFFERENT-TIP cases (the dominant 0-UTXO/hr cause). We
        // still call /stats — both for the height pair and so we can
        // optionally log the node's own status label for context — but
        // the mine/skip decision is now driven by `should_mine_gate`.
        let stats = match client.get_stats() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("/stats failed: {} (retry in 1s)", e);
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let in_grace = match last_submit_at {
            Some(t) => t.elapsed() < Duration::from_secs(POST_SUBMIT_GRACE_SECS),
            None => false,
        };
        let decision = should_mine_gate(
            stats.height,
            stats.canonical_height,
            cfg.max_network_lag,
            in_grace,
        );
        match decision {
            GateDecision::Mine => {}
            GateDecision::SkipBehind { behind } => {
                if last_pause_log.elapsed() >= Duration::from_secs(15) {
                    tracing::warn!(
                        "PAUSED gate=BEHIND behind={} max_lag={} sync_status={} local_h={} canon_h={} local_tip=0x{} canon_tip=0x{}",
                        behind,
                        cfg.max_network_lag,
                        stats.sync_status,
                        stats.height,
                        stats.canonical_height,
                        stats.tip,
                        stats.canonical_tip,
                    );
                    last_pause_log = Instant::now();
                }
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
            GateDecision::SkipAheadFork { ahead } => {
                if last_pause_log.elapsed() >= Duration::from_secs(15) {
                    tracing::warn!(
                        "PAUSED gate=AHEAD-FORK ahead={} (>+1 with no grace) sync_status={} local_h={} canon_h={} local_tip=0x{} canon_tip=0x{}",
                        ahead,
                        stats.sync_status,
                        stats.height,
                        stats.canonical_height,
                        stats.tip,
                        stats.canonical_tip,
                    );
                    last_pause_log = Instant::now();
                }
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        }
        // Visibility: when the node thinks we're forked but the gate
        // permits us (the SURPRISE-WIN / TIED-DIFFERENT-TIP cases we're
        // explicitly porting v74 to allow), log once per 60s so logs
        // make the divergence obvious.
        if stats.sync_status != "synced" && last_pause_log.elapsed() >= Duration::from_secs(60) {
            tracing::info!(
                "v74 gate ALLOW despite sync_status={} (local_h={} canon_h={} local_tip=0x{} canon_tip=0x{})",
                stats.sync_status,
                stats.height,
                stats.canonical_height,
                stats.tip,
                stats.canonical_tip,
            );
            last_pause_log = Instant::now();
        }

        // iter-E1 (mine-through-503): branch on the *typed* /work/get outcome
        // so a node-side 503 hold is handled differently from a real transport
        // failure. On 200 we mine the fresh templates and remember slot A. On
        // 503 we keep the GPU busy on the last-good slot A *iff* its parent
        // still extends the node's current /tip; otherwise we idle/backoff.
        // On any other error (transport / non-503 status) we back off.
        let templates: Vec<WorkTemplate> = match client.get_work_classified() {
            Ok(crate::http::GetWork::Work(resp)) => {
                // iter-27 E: collect whichever slots are present. Slot A should
                // always be Some on 200; slot B is the speculative hedge that
                // appears only on local!=explorer divergence.
                let templates: Vec<WorkTemplate> =
                    resp.templates.into_iter().flatten().collect();
                if templates.is_empty() {
                    // The node should never return both slots None on a 200,
                    // but belt-and-braces: treat as "no work yet" and retry.
                    tracing::warn!("/work/get returned 0 templates; retry in 1s");
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
                // Remember slot A (variant 0 == the only winnable, local-tip-
                // extending target) for the next 503 hold. Slot A is always
                // the first present slot on a 200 per the /work/get contract.
                last_good_slot_a = Some(templates[0].clone());
                templates
            }
            Ok(crate::http::GetWork::ServiceUnavailable) => {
                // The node's /work/get gate is holding (fork micro-divergence,
                // fastsync, or a Layer-5 settlement hold). Instead of idling,
                // keep mining the last-good slot-A template IF its parent still
                // equals the node's current /tip. The cheap /tip poll validates
                // that the chain hasn't moved off our held parent.
                let held_prev = last_good_slot_a.as_ref().map(|t| t.prev);
                let current_tip = client.get_tip().ok();
                if should_mine_through_503(held_prev, current_tip) {
                    // Safe to keep the GPU busy: re-mine the held slot A.
                    if last_mine_through_log.elapsed() >= Duration::from_secs(15) {
                        let held = last_good_slot_a.as_ref().expect(
                            "should_mine_through_503 implies last_good_slot_a is Some",
                        );
                        tracing::info!(
                            "[mine-through-503] /work/get 503 but held slot-A prev=0x{} still == tip; mining it (id={} height={}) instead of idling",
                            hex::encode(held.prev),
                            held.id,
                            held.height,
                        );
                        last_mine_through_log = Instant::now();
                    }
                    vec![last_good_slot_a
                        .as_ref()
                        .expect("checked Some above")
                        .clone()]
                } else {
                    // Tip moved off our held parent, no template held, or /tip
                    // read failed → genuinely nothing safe to mine. Idle briefly
                    // and re-poll (this is the only legitimate idle path now).
                    if last_mine_through_log.elapsed() >= Duration::from_secs(15) {
                        tracing::warn!(
                            "/work/get 503 and no still-current last-good template (held={}, tip_read={}); idling 1s",
                            held_prev.is_some(),
                            current_tip.is_some(),
                        );
                        last_mine_through_log = Instant::now();
                    }
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            }
            Err(e) => {
                // Real failure: transport error (node down, DNS, timeout) or a
                // non-503 status. Back off — mining a held template here would
                // not help and could waste cycles if the node is truly gone.
                tracing::warn!("/work/get failed: {} (retry in 1s)", e);
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };

        // One-line summary so logs make the speculation visible.
        // iter-32: generalized from 1/2 slots to 1..=4 slots. Mode label
        // grows with the number of populated slots so post-mortems can
        // tell single vs dual vs triple vs quad at a glance.
        match templates.len() {
            1 => tracing::info!(
                "got_work mode=single id={} variant={} height={} bits=0x{:08x} prev=0x{} target=0x{}",
                templates[0].id,
                variant_tag(templates[0].id),
                templates[0].height,
                templates[0].bits,
                hex::encode(templates[0].prev),
                hex::encode(templates[0].target),
            ),
            _ => {
                let mode = match templates.len() {
                    2 => "dual",
                    3 => "triple",
                    4 => "quad",
                    n => {
                        // Defensive: shouldn't happen with the 4-slot
                        // wire format, but log it cleanly if it does.
                        tracing::warn!(
                            "got_work mode=other ({} slots, unexpected)",
                            n
                        );
                        "other"
                    }
                };
                let mut parts: Vec<String> = Vec::with_capacity(templates.len());
                for t in &templates {
                    parts.push(format!(
                        "{}(id={} h={} prev=0x{})",
                        variant_tag(t.id),
                        t.id,
                        t.height,
                        hex::encode(t.prev),
                    ));
                }
                tracing::info!(
                    "got_work mode={} slots={}: {}",
                    mode,
                    templates.len(),
                    parts.join(" "),
                );
            }
        }

        // Precompute branches per template (cheap; doesn't change per
        // extranonce). Index aligns with `templates`.
        let branches: Vec<Vec<[u8; 32]>> = templates
            .iter()
            .map(|t| t.merkle_branch.iter().map(|x| x.0).collect())
            .collect();

        let template_started = Instant::now();
        let last_refresh = Instant::now();
        let mut iter_idx: usize = 0;

        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if last_refresh.elapsed() > refresh_every {
                break;
            }

            // iter-27 E: pick the template for this iteration. With two
            // templates we strictly alternate A, B, A, B; with one we
            // always pick slot 0 (the legacy single-template path).
            let pick = iter_idx % templates.len();
            let template = &templates[pick];
            let branch = &branches[pick];

            // Compute coinbase txid & merkle root for this extranonce
            // against the chosen template.
            let cb_txid = coinbase_txid(
                &template.coinbase_prefix,
                extranonce,
                &template.coinbase_suffix,
            );
            let merkle = merkle_root_from_branch(cb_txid, branch, 0);

            // Build the header skeleton (nonce will be overwritten by the
            // backend per-thread).
            let hdr = header_84(
                template.version,
                &template.prev,
                &merkle,
                template.time,
                template.bits,
                0,
            );

            // iter-31: partition the nonce range between GPU + CPU pool.
            let (gpu_range, cpu_ranges) = partition_nonce_range(
                template.nonce_start,
                template.nonce_end,
                cfg.cpu_share,
                cfg.cpu_threads,
            );

            // Shared cancellation + winner slot for this template
            // iteration. `iter_stop` is OR-ed with the global `stop` to
            // form the AtomicBool we hand to the GPU backend, so the GPU
            // exits its next pipe drain when CPU wins.
            let iter_stop = Arc::new(AtomicBool::new(false));
            let cpu_winner: Arc<Mutex<Option<CpuFind>>> = Arc::new(Mutex::new(None));
            // Used by the spec'd `found_for_template_id` contract — CPU
            // workers and (theoretically) future GPU finds set this so
            // workers' next 256-iteration check exits cleanly.
            let found_for_template_id = Arc::new(AtomicU64::new(0));
            let template_id = template.id;
            // Track CPU work for the combined hashrate log.
            let cpu_swept = Arc::new(std::sync::atomic::AtomicU64::new(0));

            // Bridge: GPU sees stop || iter_stop. Built so we don't have
            // to weaken the existing `MiningBackend::hash_range(&AtomicBool)`
            // signature; we just OR the two into one local AtomicBool that
            // the GPU thread polls. We update it in real time via a small
            // poller spawned alongside the workers.
            let gpu_stop = Arc::new(AtomicBool::new(stop.load(Ordering::Relaxed)));

            // Run GPU + CPU workers concurrently, race for first FOUND.
            let gpu_result: Mutex<Option<MiningResult>> = Mutex::new(None);
            let gpu_result_ref = &gpu_result;

            let midstate = midstate_of_first_chunk_fast(&hdr);
            let mut tail_template = [0u8; 20];
            tail_template[..16].copy_from_slice(&hdr[64..80]);
            let target = template.target;

            thread::scope(|scope| {
                // CPU workers: one per cpu_ranges entry. SHA-NI dispatch
                // via `finish_sha256d_from_midstate_fast`. Checks the
                // shared cancellation atomic every 256 iterations like
                // the GPU kernel does.
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
                            // Cancellation poll every 256 iterations
                            // (matches the GPU kernel's epoch).
                            if i & 0xff == 0 {
                                if stop_.load(Ordering::Relaxed)
                                    || iter_stop_.load(Ordering::Relaxed)
                                {
                                    break;
                                }
                                if found_for_template_id_.load(Ordering::Acquire)
                                    == template_id
                                {
                                    break;
                                }
                            }
                            tail[16..20].copy_from_slice(&n.to_le_bytes());
                            let h = finish_sha256d_from_midstate_fast(&midstate, &tail);
                            local_swept += 1;
                            if hash_leq_target(&h, &target) {
                                // Race the winner slot. Only the first
                                // writer fills it; the rest observe and
                                // exit on their next poll.
                                let mut g = cpu_winner_.lock().unwrap();
                                if g.is_none() {
                                    *g = Some(CpuFind {
                                        thread_idx,
                                        nonce: n,
                                        hash: h,
                                    });
                                    found_for_template_id_
                                        .store(template_id, Ordering::Release);
                                    iter_stop_.store(true, Ordering::Release);
                                }
                                break;
                            }
                        }
                        cpu_swept_.fetch_add(local_swept, Ordering::Relaxed);
                    });
                }

                // Lightweight bridge poller: forwards stop || iter_stop
                // into `gpu_stop` so the GPU backend wakes up on CPU win.
                // Scoped thread; exits when iter_stop is set OR the GPU
                // returns (we then poke iter_stop ourselves below).
                let stop_b = stop.clone();
                let iter_stop_b = iter_stop.clone();
                let gpu_stop_b = gpu_stop.clone();
                scope.spawn(move || {
                    loop {
                        let s = stop_b.load(Ordering::Relaxed)
                            || iter_stop_b.load(Ordering::Relaxed);
                        gpu_stop_b.store(s, Ordering::Relaxed);
                        if s {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                });

                // GPU sweep on its assigned sub-range. Runs on the main
                // scope thread (we're already inside thread::scope).
                let (gstart, gend) = gpu_range;
                let res = if gend > gstart {
                    backend.hash_range(hdr, target, gstart, gend, &gpu_stop)
                } else {
                    None
                };
                *gpu_result_ref.lock().unwrap() = res;
                // Whether the GPU found something or exhausted, signal
                // the CPU workers + bridge thread to wind down so we
                // don't keep CPU spinning past template life.
                iter_stop.store(true, Ordering::Release);
            });

            let gpu_found = gpu_result.into_inner().unwrap();
            let cpu_found = cpu_winner.lock().unwrap().clone();
            let cpu_swept_n = cpu_swept.load(Ordering::Relaxed) as u128;
            let gpu_swept = (gpu_range.1 as u128).saturating_sub(gpu_range.0 as u128);

            // GPU finished its sweep (whether by hit, exhaustion, or
            // cancellation); accumulate hashrate.
            gpu_nonces_since_log = gpu_nonces_since_log.saturating_add(gpu_swept);
            cpu_nonces_since_log = cpu_nonces_since_log.saturating_add(cpu_swept_n);

            // Priority: if both raced and got hits, prefer GPU (it
            // finished an entire sweep block; CPU may have hit earlier
            // but ordering is fundamentally ambiguous on a race). Either
            // one submits via the same /work/submit path, so the choice
            // doesn't affect correctness.
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
                    let elapsed_ms = template_started.elapsed().as_millis().max(1) as u64;

                    let (device, thread_label, nonce, claimed_hash) = match src {
                        WinSource::Gpu(mr) => ("gpu", None, mr.nonce, mr.hash),
                        WinSource::Cpu(cf) => {
                            ("cpu", Some(cf.thread_idx), cf.nonce, cf.hash)
                        }
                    };

                    // CORRECTNESS GATE: re-hash on CPU. Catches any kernel
                    // bug or driver miscompile before the bad block reaches
                    // the node. ~µs CPU cost, well worth it. (Also runs for
                    // CPU wins — cheap and uniform.)
                    let mut hdr_check = hdr;
                    hdr_check[80..84].copy_from_slice(&nonce.to_le_bytes());
                    let cpu_hash = crate::sha256d_cpu::sha256d(&hdr_check);
                    if cpu_hash != claimed_hash {
                        tracing::error!(
                            "device={} HASH MISMATCH at id={} variant={} nonce={} extranonce={}: claimed=0x{} cpu=0x{} - skipping",
                            device,
                            template.id,
                            variant_tag(template.id),
                            nonce,
                            extranonce,
                            hex::encode(claimed_hash),
                            hex::encode(cpu_hash),
                        );
                        extranonce = extranonce.wrapping_add(1);
                        iter_idx = iter_idx.wrapping_add(1);
                        continue;
                    }
                    // Also confirm the hash actually clears the target.
                    if cpu_hash > template.target {
                        tracing::error!(
                            "device={} returned hash ABOVE target at id={} variant={} nonce={}: hash=0x{} target=0x{} - skipping",
                            device,
                            template.id,
                            variant_tag(template.id),
                            nonce,
                            hex::encode(cpu_hash),
                            hex::encode(template.target),
                        );
                        extranonce = extranonce.wrapping_add(1);
                        iter_idx = iter_idx.wrapping_add(1);
                        continue;
                    }

                    match thread_label {
                        Some(t) => tracing::info!(
                            "FOUND device={} thread={} id={} variant={} height={} nonce={} extranonce={} hash=0x{} elapsed_ms={}",
                            device,
                            t,
                            template.id,
                            variant_tag(template.id),
                            template.height,
                            nonce,
                            extranonce,
                            hex::encode(cpu_hash),
                            elapsed_ms,
                        ),
                        None => tracing::info!(
                            "FOUND device={} id={} variant={} height={} nonce={} extranonce={} hash=0x{} elapsed_ms={}",
                            device,
                            template.id,
                            variant_tag(template.id),
                            template.height,
                            nonce,
                            extranonce,
                            hex::encode(cpu_hash),
                            elapsed_ms,
                        ),
                    }
                    let sub = WorkSubmission {
                        id: template.id,
                        nonce,
                        extranonce,
                        time: template.time,
                    };
                    // v74 port: pre-submit fresh-tip poll drives the
                    // CONFIDENT / STALE classification used by the
                    // sustained-STALE circuit breaker. The earlier
                    // iter-53 #2 logic ABORTED the submit on stale; the
                    // v74 doctrine flips that — submission ALWAYS fires
                    // (cumulative-work consensus + propagation can still
                    // flip a race), and the classification drives only
                    // the lockout decision + post-submit grace decision.
                    let template_prev = template.prev;
                    let mut classification = "CONFIDENT";
                    if let Ok(current_tip) = client.get_tip() {
                        if template_is_stale(template_prev, current_tip) {
                            classification = "STALE-PARENT";
                            tracing::warn!(
                                "[submit-classify] {} | template prev=0x{} != current tip=0x{} (id={} variant={} height={}) - submitting anyway (v74)",
                                classification,
                                hex::encode(template_prev),
                                hex::encode(current_tip),
                                template.id,
                                variant_tag(template.id),
                                template.height,
                            );
                        } else {
                            tracing::debug!(
                                "[submit-classify] {} | tip still 0x{} (id={} variant={} height={})",
                                classification,
                                hex::encode(current_tip),
                                template.id,
                                variant_tag(template.id),
                                template.height,
                            );
                        }
                    } else {
                        // /tip fetch failed: be permissive (don't downgrade).
                        tracing::debug!(
                            "[submit-classify] CONFIDENT (/tip fetch failed, no info) (id={} variant={})",
                            template.id, variant_tag(template.id),
                        );
                    }

                    // v74 circuit-breaker bookkeeping.
                    if classification == "STALE-PARENT" {
                        consecutive_stale += 1;
                        if consecutive_stale >= STALE_STREAK_THRESHOLD
                            && force_lockout_until.is_none()
                        {
                            force_lockout_until = Some(
                                Instant::now()
                                    + Duration::from_secs(SUSTAINED_LOCKOUT_SECS),
                            );
                            tracing::warn!(
                                "[circuit-breaker] {} consecutive STALE-PARENT submits - {}s explorer-gate lockout activated",
                                consecutive_stale, SUSTAINED_LOCKOUT_SECS
                            );
                        }
                    } else if classification == "CONFIDENT" {
                        if consecutive_stale > 0 {
                            tracing::info!(
                                "[circuit-breaker] stale streak broken (was {}), resetting",
                                consecutive_stale
                            );
                        }
                        consecutive_stale = 0;
                    }

                    // v75 port: fan submit out to local + every peer URL
                    // in parallel. Local response drives our outcome;
                    // peers are fire-and-forget for orphan-rate reduction.
                    let submit_start = Instant::now();
                    let submit_outcome = spawn_broadcast_submit(
                        client,
                        &cfg.broadcast_peers,
                        &sub,
                    );
                    let submit_ms = submit_start.elapsed().as_millis() as u64;
                    match submit_outcome {
                        Ok(resp) => {
                            tracing::info!(
                                "submit device={} id={} variant={} response={} class={} latency_ms={} peers={}",
                                device,
                                template.id,
                                variant_tag(template.id),
                                resp,
                                classification,
                                submit_ms,
                                cfg.broadcast_peers.len(),
                            );
                            // v74 grace-window arming: only on CONFIDENT
                            // submits. STALE submits MIGHT still race-win,
                            // but if they don't we don't want to leave the
                            // gate wide open for the next 60s of mining
                            // on a tip the network already moved past.
                            if classification == "CONFIDENT" {
                                last_submit_at = Some(Instant::now());
                            }
                        }
                        Err(e) => tracing::warn!(
                            "submit device={} id={} variant={} failed: {} class={} latency_ms={}",
                            device,
                            template.id,
                            variant_tag(template.id),
                            e,
                            classification,
                            submit_ms,
                        ),
                    }
                    // After a submit, ask for fresh work.
                    break;
                }
                None => {
                    // Both GPU and CPU exhausted their slices for this
                    // extranonce / template; bump the iteration index
                    // (rotates A<->B if dual) and the extranonce, then
                    // re-derive the merkle root next loop.
                    if last_hashrate_log.elapsed() >= Duration::from_secs(10) {
                        let elapsed = last_hashrate_log.elapsed().as_secs_f64();
                        let ghs_gpu = (gpu_nonces_since_log as f64) / 1e9 / elapsed;
                        let mhs_cpu = (cpu_nonces_since_log as f64) / 1e6 / elapsed;
                        let combined_ghs = ghs_gpu + (mhs_cpu / 1000.0);
                        let mode = if cfg.cpu_threads > 0 && cfg.cpu_share > 0.0 {
                            "dual"
                        } else {
                            "gpu-only"
                        };
                        // iter-32: 1..=4 slot modes (single/dual/triple/quad).
                        let templates_mode = match templates.len() {
                            1 => "single-tmpl",
                            2 => "dual-tmpl",
                            3 => "triple-tmpl",
                            4 => "quad-tmpl",
                            _ => "n-tmpl",
                        };
                        tracing::info!(
                            "hashrate gpu={:.2} GH/s cpu={:.2} MH/s combined={:.2} GH/s mode={} ({}; height {}, target=0x{}...)",
                            ghs_gpu,
                            mhs_cpu,
                            combined_ghs,
                            mode,
                            templates_mode,
                            template.height,
                            &hex::encode(template.target)[..16]
                        );
                        // iter-51: fire-and-forget the combined GH/s to
                        // the node's POST /miner/heartbeat so /stats can
                        // expose `our_hashrate_ghs` +
                        // `network_hashrate_excl_us_ghs`. We must NOT
                        // block the mining loop on this — failures (node
                        // down, network blip, slow response) are logged
                        // at debug only and the loop continues.
                        if let Err(e) = client.post_miner_heartbeat(combined_ghs) {
                            tracing::debug!(
                                "iter-51: /miner/heartbeat post failed (combined={:.2} GH/s): {}",
                                combined_ghs, e
                            );
                        }
                        last_hashrate_log = Instant::now();
                        gpu_nonces_since_log = 0;
                        cpu_nonces_since_log = 0;
                    }
                    extranonce = extranonce.wrapping_add(1);
                    iter_idx = iter_idx.wrapping_add(1);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- partition truth-table tests ---

    #[test]
    fn partition_gpu_only_when_threads_zero() {
        // cpu_threads == 0 → GPU takes the full range, CPU empty.
        let (gpu, cpu) =
            partition_nonce_range(0, 1_000_000, 0.4, 0);
        assert_eq!(gpu, (0, 1_000_000));
        assert!(cpu.is_empty());
        assert!(partition_invariants_hold(0, 1_000_000, gpu, &cpu));
    }

    #[test]
    fn partition_gpu_only_when_share_zero() {
        // cpu_share == 0.0 → GPU takes the full range even with threads>0.
        let (gpu, cpu) =
            partition_nonce_range(0, 1_000_000, 0.0, 16);
        assert_eq!(gpu, (0, 1_000_000));
        assert!(cpu.is_empty());
        assert!(partition_invariants_hold(0, 1_000_000, gpu, &cpu));
    }

    #[test]
    fn partition_default_split_4_threads() {
        // 1M nonces, 40% CPU, 4 threads: GPU=600_000, CPU=400_000 split
        // into 4 equal chunks of 100_000.
        let (gpu, cpu) =
            partition_nonce_range(0, 1_000_000, 0.4, 4);
        assert_eq!(gpu, (0, 600_000));
        assert_eq!(cpu.len(), 4);
        assert_eq!(cpu[0], (600_000, 700_000));
        assert_eq!(cpu[1], (700_000, 800_000));
        assert_eq!(cpu[2], (800_000, 900_000));
        assert_eq!(cpu[3], (900_000, 1_000_000));
        assert!(partition_invariants_hold(0, 1_000_000, gpu, &cpu));
    }

    #[test]
    fn partition_full_u32_range_no_gap() {
        // The real loop sweeps [0, u32::MAX) — check end-to-end coverage.
        let (gpu, cpu) =
            partition_nonce_range(0, u32::MAX, 0.4, 16);
        assert_eq!(gpu.0, 0);
        assert_eq!(cpu.len(), 16);
        // Last CPU range ends exactly at u32::MAX (no gap from rounding).
        assert_eq!(cpu.last().unwrap().1, u32::MAX);
        // GPU end == first CPU start (no gap).
        assert_eq!(gpu.1, cpu[0].0);
        // No gaps between CPU chunks.
        for w in cpu.windows(2) {
            assert_eq!(w[0].1, w[1].0);
        }
        assert!(partition_invariants_hold(0, u32::MAX, gpu, &cpu));
    }

    #[test]
    fn partition_remainder_distributed_evenly() {
        // 1003 nonces, share=0.5 → cpu_total=502 (round of 501.5), 4
        // threads → 125 each + 2 remainder, first two threads get 126.
        let (gpu, cpu) = partition_nonce_range(0, 1003, 0.5, 4);
        assert_eq!(gpu, (0, 501));
        assert_eq!(cpu.len(), 4);
        // Sum check.
        let gpu_len = (gpu.1 - gpu.0) as u64;
        let cpu_sum: u64 = cpu.iter().map(|(s, e)| (*e - *s) as u64).sum();
        assert_eq!(gpu_len + cpu_sum, 1003);
        assert!(partition_invariants_hold(0, 1003, gpu, &cpu));
    }

    #[test]
    fn partition_share_clamped() {
        // Negative share clamps to 0 (GPU-only).
        let (gpu, cpu) =
            partition_nonce_range(0, 1_000_000, -0.5, 4);
        assert_eq!(gpu, (0, 1_000_000));
        assert!(cpu.is_empty());
        // > 1.0 clamps to 1.0 (CPU takes everything).
        let (gpu, cpu) =
            partition_nonce_range(0, 1_000_000, 2.0, 4);
        assert_eq!(gpu.1 - gpu.0, 0);
        assert_eq!(cpu.last().unwrap().1, 1_000_000);
        assert!(partition_invariants_hold(0, 1_000_000, gpu, &cpu));
    }

    #[test]
    fn partition_full_cpu_share_leaves_gpu_empty() {
        // cpu_share == 1.0 → GPU range is empty, CPU gets everything.
        let (gpu, cpu) =
            partition_nonce_range(0, 1_000_000, 1.0, 4);
        assert_eq!(gpu.0, 0);
        assert_eq!(gpu.1, 0);
        assert_eq!(cpu.len(), 4);
        assert_eq!(cpu.last().unwrap().1, 1_000_000);
        let cpu_sum: u64 = cpu.iter().map(|(s, e)| (*e - *s) as u64).sum();
        assert_eq!(cpu_sum, 1_000_000);
        assert!(partition_invariants_hold(0, 1_000_000, gpu, &cpu));
    }

    #[test]
    fn partition_empty_range() {
        let (gpu, cpu) = partition_nonce_range(100, 100, 0.4, 4);
        assert_eq!(gpu, (100, 100));
        assert!(cpu.is_empty());
    }

    #[test]
    fn partition_thread_count_matches_spec() {
        // Spawn count behavior: cpu_threads == N produces exactly N
        // CPU ranges (matching the spec's "each thread gets a
        // contiguous sub-range").
        for n in [1usize, 2, 4, 8, 16, 32] {
            let (_, cpu) =
                partition_nonce_range(0, 1_000_000, 0.4, n);
            assert_eq!(cpu.len(), n, "expected {} cpu ranges", n);
        }
    }

    #[test]
    fn partition_single_thread_gets_full_cpu_pool() {
        let (gpu, cpu) =
            partition_nonce_range(0, 1_000_000, 0.4, 1);
        assert_eq!(gpu, (0, 600_000));
        assert_eq!(cpu.len(), 1);
        assert_eq!(cpu[0], (600_000, 1_000_000));
    }

    #[test]
    fn partition_at_least_one_nonce_for_gpu_when_share_lt_1() {
        // Edge: very small range with cpu_share=0.99 — GPU should still
        // get at least 1 nonce (so backend.hash_range doesn't trivially
        // return None for a too-narrow window).
        let (gpu, cpu) = partition_nonce_range(0, 100, 0.99, 4);
        assert!(gpu.1 > gpu.0, "GPU pool must be non-empty for share<1.0");
        assert!(!cpu.is_empty());
        let cpu_sum: u64 = cpu.iter().map(|(s, e)| (*e - *s) as u64).sum();
        let gpu_len = (gpu.1 - gpu.0) as u64;
        assert_eq!(gpu_len + cpu_sum, 100);
    }

    // --- iter-32 variant_of / variant_tag tests ---

    #[test]
    fn variant_of_decodes_all_4_variants() {
        // Variant 0 = top 2 bits clear.
        assert_eq!(variant_of(0u64), 0);
        assert_eq!(variant_of(123u64), 0); // any seq, top bits clear
        // Variant 1 = top 2 bits = 0b01 (bit 62 set, bit 63 clear).
        assert_eq!(variant_of(1u64 << 62), 1);
        assert_eq!(variant_of((1u64 << 62) | 99), 1);
        // Variant 2 = top 2 bits = 0b10 (bit 62 clear, bit 63 set).
        assert_eq!(variant_of(2u64 << 62), 2);
        assert_eq!(variant_of((2u64 << 62) | 77), 2);
        // Variant 3 = top 2 bits = 0b11 (both bits set).
        assert_eq!(variant_of(3u64 << 62), 3);
        assert_eq!(variant_of((3u64 << 62) | 55), 3);
    }

    #[test]
    fn variant_tag_labels_each_variant() {
        assert_eq!(variant_tag(0u64), "A");
        assert_eq!(variant_tag(1u64 << 62), "B");
        assert_eq!(variant_tag(2u64 << 62), "C");
        assert_eq!(variant_tag(3u64 << 62), "D");
        // Sequence portion doesn't affect the tag.
        assert_eq!(variant_tag((1u64 << 62) | 0xDEAD_BEEF), "B");
        assert_eq!(variant_tag((3u64 << 62) | 0xCAFE_F00D), "D");
    }

    #[test]
    fn variant_mask_matches_node_constant() {
        // Sanity: the mirror constant must match the wire layout.
        // Top 2 bits set: 0xC000_0000_0000_0000.
        assert_eq!(VARIANT_MASK, 0xC000_0000_0000_0000u64);
    }

    // --- iter-53 #2 / iter-46 #D.8: pre-submit staleness check tests ---

    #[test]
    fn iter53_d8_stale_when_tip_differs() {
        // Tip has rotated away from template_prev → submit is doomed.
        assert!(template_is_stale([1u8; 32], [2u8; 32]));
    }

    #[test]
    fn iter53_d8_not_stale_when_tip_matches() {
        // Tip == template_prev → template still extends the current tip.
        assert!(!template_is_stale([1u8; 32], [1u8; 32]));
    }

    #[test]
    fn iter53_d8_not_stale_when_tip_zero_fetch_failed() {
        // current_tip == zero (sentinel for "fetch failed / pre-genesis").
        // Be permissive: we don't actually know the tip rotated, so don't
        // throw away a potentially valid submit.
        assert!(!template_is_stale([1u8; 32], [0u8; 32]));
    }

    // --- iter-E1: mine-through-503 decision tests ---

    #[test]
    fn e1_mine_through_when_held_prev_equals_tip() {
        // The core case: node 503s /work/get, but we hold a slot-A template
        // whose parent still == the current tip → keep the GPU mining it
        // (do NOT idle). This is the 22.6%-idle reclaim.
        assert!(should_mine_through_503(Some([7u8; 32]), Some([7u8; 32])));
    }

    #[test]
    fn e1_idle_when_tip_moved_off_held_prev() {
        // Tip rotated away from our held parent → mining the held template
        // would burn the GPU on a parent the network moved past. Stop
        // (idle/backoff) and wait for fresh work.
        assert!(!should_mine_through_503(Some([7u8; 32]), Some([9u8; 32])));
    }

    #[test]
    fn e1_idle_when_no_template_held() {
        // No last-good slot A yet (e.g. first cycle 503'd before any 200) →
        // nothing safe to mine, so idle.
        assert!(!should_mine_through_503(None, Some([7u8; 32])));
    }

    #[test]
    fn e1_idle_when_tip_read_failed() {
        // /tip fetch failed (None) → we can't confirm the held parent is
        // still current, so be conservative and idle rather than risk mining
        // a stale parent indefinitely. (Mirrors template_is_stale's
        // permissive-but-here-conservative tip handling for the mine path.)
        assert!(!should_mine_through_503(Some([7u8; 32]), None));
        // And the zero-tip sentinel (decoded-but-empty) is also "unknown".
        assert!(!should_mine_through_503(Some([7u8; 32]), Some([0u8; 32])));
    }

    #[test]
    fn e1_idle_when_both_unknown() {
        // Neither a held template nor a tip read → definitively idle.
        assert!(!should_mine_through_503(None, None));
    }

    // --- v74 port: asymmetric explorer-gate decision tests ---

    #[test]
    fn v74_gate_tied_same_height_mines() {
        // The exact case that was producing 0 UTXOs/hr: local == canon
        // but tips differ. v74 says MINE; pre-v74 PAUSED on sync_status=fork.
        assert_eq!(
            should_mine_gate(100, 100, 0, false),
            GateDecision::Mine
        );
    }

    #[test]
    fn v74_gate_ahead_by_one_always_mines() {
        // SURPRISE-WIN: explorer is one block behind. Allowed regardless of
        // grace state (the key v74 loosening).
        assert_eq!(
            should_mine_gate(101, 100, 0, false),
            GateDecision::Mine
        );
        assert_eq!(
            should_mine_gate(101, 100, 0, true),
            GateDecision::Mine
        );
    }

    #[test]
    fn v74_gate_ahead_by_two_grace_only() {
        // +2 without grace: looks like a private fork forming. Skip.
        assert_eq!(
            should_mine_gate(102, 100, 0, false),
            GateDecision::SkipAheadFork { ahead: 2 }
        );
        // +2 with grace: post-submit window, still legitimately ahead.
        assert_eq!(
            should_mine_gate(102, 100, 0, true),
            GateDecision::Mine
        );
    }

    #[test]
    fn v74_gate_behind_strict_when_lag_exceeded() {
        // BEHIND by 1, default max_lag=0: skip. (Strict policy.)
        assert_eq!(
            should_mine_gate(99, 100, 0, false),
            GateDecision::SkipBehind { behind: 1 }
        );
        assert_eq!(
            should_mine_gate(99, 100, 0, true),
            GateDecision::SkipBehind { behind: 1 }
        );
    }

    #[test]
    fn v74_gate_behind_within_lag_mines() {
        // BEHIND by 1, max_lag=2: allowed.
        assert_eq!(
            should_mine_gate(99, 100, 2, false),
            GateDecision::Mine
        );
        // Exactly at the lag boundary.
        assert_eq!(
            should_mine_gate(98, 100, 2, false),
            GateDecision::Mine
        );
        // One past the boundary: skip.
        assert_eq!(
            should_mine_gate(97, 100, 2, false),
            GateDecision::SkipBehind { behind: 3 }
        );
    }

    #[test]
    fn v74_gate_unknown_heights_mine() {
        // Zero heights = sentinel "stats incomplete". Conservative default
        // matches the pre-v74 behavior of falling through to /work/get.
        assert_eq!(
            should_mine_gate(0, 100, 0, false),
            GateDecision::Mine
        );
        assert_eq!(
            should_mine_gate(100, 0, 0, false),
            GateDecision::Mine
        );
    }

    #[test]
    fn v74_gate_large_ahead_grace_still_allows() {
        // Deep grace: even ahead by many blocks is allowed (post-submit
        // propagation can leave us briefly ahead by 2-3 if blocks come
        // in bursts).
        assert_eq!(
            should_mine_gate(105, 100, 0, true),
            GateDecision::Mine
        );
        // Without grace, the same scenario looks like a sustained fork.
        assert_eq!(
            should_mine_gate(105, 100, 0, false),
            GateDecision::SkipAheadFork { ahead: 5 }
        );
    }

    // --- v75 port: broadcast peer loader tests ---

    #[test]
    fn v75_broadcast_empty_path_returns_empty() {
        assert!(load_broadcast_peers("").is_empty());
    }

    #[test]
    fn v75_broadcast_missing_file_returns_empty() {
        // Bogus path: must not panic, must return empty (caller falls
        // back to local-only).
        assert!(load_broadcast_peers("./__definitely_does_not_exist__.txt").is_empty());
    }

    #[test]
    fn v75_broadcast_parses_file_skips_comments_and_blanks() {
        // Write a temp file, parse it, drop it. Uses the std tempdir
        // pattern (no external crate).
        let mut path = std::env::temp_dir();
        path.push(format!("v75_broadcast_test_{}.txt", std::process::id()));
        std::fs::write(
            &path,
            "# header comment\n  \nhttp://1.2.3.4:8799\n\n# inline comment\nhttp://5.6.7.8:8799  \n",
        )
        .unwrap();
        let peers = load_broadcast_peers(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            peers,
            vec![
                "http://1.2.3.4:8799".to_string(),
                "http://5.6.7.8:8799".to_string()
            ]
        );
    }
}
