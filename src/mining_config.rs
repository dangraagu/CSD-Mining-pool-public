//! Runtime knobs for CPU+GPU dual mining, plus the pure nonce-range
//! partitioning helper used by the mining loop.

/// Runtime knobs for CPU+GPU dual mining. Built in `main.rs` from CLI flags
/// and passed into the mining loop.
#[derive(Clone, Debug)]
pub struct MiningConfig {
    /// CPU threads to dedicate to hashing alongside the GPU. 0 disables
    /// CPU mining entirely (the GPU takes 100% of the nonce range).
    pub cpu_threads: usize,
    /// Fraction of the nonce range the CPU pool sweeps (0.0..=1.0).
    /// 0.0 disables CPU mining; 1.0 gives the GPU nothing. A useful range
    /// is roughly 0.2..0.5 depending on the CPU/GPU mix.
    pub cpu_share: f32,
}

impl Default for MiningConfig {
    fn default() -> Self {
        // GPU-only by default.
        Self {
            cpu_threads: 0,
            cpu_share: 0.0,
        }
    }
}

/// Partition `[nonce_start, nonce_end)` into one GPU range plus `cpu_threads`
/// contiguous CPU ranges, giving the CPU pool a `cpu_share` fraction of the
/// span. Guarantees (when the CPU pool is active):
///   - GPU.start == nonce_start
///   - GPU.end == first CPU range start
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

/// Cheap invariant check used by the partition tests.
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- partition truth-table tests ---

    #[test]
    fn partition_gpu_only_when_threads_zero() {
        // cpu_threads == 0 → GPU takes the full range, CPU empty.
        let (gpu, cpu) = partition_nonce_range(0, 1_000_000, 0.4, 0);
        assert_eq!(gpu, (0, 1_000_000));
        assert!(cpu.is_empty());
        assert!(partition_invariants_hold(0, 1_000_000, gpu, &cpu));
    }

    #[test]
    fn partition_gpu_only_when_share_zero() {
        // cpu_share == 0.0 → GPU takes the full range even with threads>0.
        let (gpu, cpu) = partition_nonce_range(0, 1_000_000, 0.0, 16);
        assert_eq!(gpu, (0, 1_000_000));
        assert!(cpu.is_empty());
        assert!(partition_invariants_hold(0, 1_000_000, gpu, &cpu));
    }

    #[test]
    fn partition_default_split_4_threads() {
        // 1M nonces, 40% CPU, 4 threads: GPU=600_000, CPU=400_000 split
        // into 4 equal chunks of 100_000.
        let (gpu, cpu) = partition_nonce_range(0, 1_000_000, 0.4, 4);
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
        let (gpu, cpu) = partition_nonce_range(0, u32::MAX, 0.4, 16);
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
        let (gpu, cpu) = partition_nonce_range(0, 1_000_000, -0.5, 4);
        assert_eq!(gpu, (0, 1_000_000));
        assert!(cpu.is_empty());
        // > 1.0 clamps to 1.0 (CPU takes everything).
        let (gpu, cpu) = partition_nonce_range(0, 1_000_000, 2.0, 4);
        assert_eq!(gpu.1 - gpu.0, 0);
        assert_eq!(cpu.last().unwrap().1, 1_000_000);
        assert!(partition_invariants_hold(0, 1_000_000, gpu, &cpu));
    }

    #[test]
    fn partition_full_cpu_share_leaves_gpu_empty() {
        // cpu_share == 1.0 → GPU range is empty, CPU gets everything.
        let (gpu, cpu) = partition_nonce_range(0, 1_000_000, 1.0, 4);
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
            let (_, cpu) = partition_nonce_range(0, 1_000_000, 0.4, n);
            assert_eq!(cpu.len(), n, "expected {} cpu ranges", n);
        }
    }

    #[test]
    fn partition_single_thread_gets_full_cpu_pool() {
        let (gpu, cpu) = partition_nonce_range(0, 1_000_000, 0.4, 1);
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
}
