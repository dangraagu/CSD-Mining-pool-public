//! GPU auto-tune: pick the fastest launch geometry, and persist it.
//!
//! Split deliberately so the *decision* is a pure function — no GPU, no I/O —
//! and is unit-tested with plain values (the same testing discipline as
//! `stratum::watchdog` / `gpu_watchdog`). The actual benchmark (timing the
//! SHA-256d kernel on a real device) lives in the `cuda` backend behind the
//! `cuda` feature; this module only:
//!   - enumerates the small fixed set of candidate geometries to try
//!     ([`candidate_geometries`]),
//!   - picks the best from a set of measured MH/s ([`pick_best`]), and
//!   - (de)serializes + locates the on-disk geometry cache so the chosen
//!     geometry is committed once and reused on every later start (no
//!     re-benchmark each launch).
//!
//! NONE of this touches PoW/hash logic: a geometry is purely *how many*
//! blocks/threads/nonces a launch sweeps. The hash each thread computes is the
//! identical SHA-256d regardless of geometry.

use std::path::PathBuf;

/// One benchmarked launch geometry and the throughput it achieved.
///
/// `mh_s` is mega-hashes per second (millions of `sha256d` attempts per second)
/// measured over a short fixed-duration run. A geometry is `(blocks,
/// threads_per_block, nonces_per_thread)`; the kernel sweeps
/// `blocks * threads_per_block * nonces_per_thread` nonces per launch.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeometryMeasurement {
    pub blocks: u32,
    pub threads_per_block: u32,
    pub nonces_per_thread: u32,
    /// Measured throughput in MH/s. A non-finite or non-positive value means
    /// "this geometry failed / produced no usable measurement" and is ignored
    /// by [`pick_best`].
    pub mh_s: f64,
}

impl GeometryMeasurement {
    pub fn new(blocks: u32, threads_per_block: u32, nonces_per_thread: u32, mh_s: f64) -> Self {
        Self {
            blocks,
            threads_per_block,
            nonces_per_thread,
            mh_s,
        }
    }

    /// Nonces swept per launch for this geometry (`u64` so a big product can't
    /// overflow `u32`). Used only as a deterministic tie-breaker.
    pub fn nonces_per_launch(&self) -> u64 {
        self.blocks as u64 * self.threads_per_block as u64 * self.nonces_per_thread as u64
    }
}

/// The fixed set of candidate geometries the auto-tuner benchmarks. Kept small
/// (a handful) so the whole sweep is quick, and centred on the shipped default
/// (560 x 256 x 4096) with a spread of block counts and inner-loop depths that
/// covers small laptop GPUs through big desktop cards. Pure + deterministic so
/// a `--bench` run is reproducible and the list is unit-tested.
///
/// Each entry is `(blocks, threads_per_block, nonces_per_thread)`.
pub fn candidate_geometries() -> Vec<(u32, u32, u32)> {
    vec![
        // (blocks, threads_per_block, nonces_per_thread)
        (256, 256, 4096),
        (512, 256, 2048),
        (560, 256, 4096), // shipped default
        (1024, 256, 2048),
        (2048, 256, 1024), // A/B winner on the 5070 Ti (native sm_120)
        (4096, 256, 512),  // deep-block variant (v0.2.0)
        (1024, 512, 1024),
    ]
}

/// Pick the geometry with the highest measured MH/s.
///
/// Pure → unit-tested with plain values. Rules:
///   - Measurements whose `mh_s` is non-finite (NaN/inf) or `<= 0.0` are
///     IGNORED (a failed / zero benchmark must never win).
///   - The winner is the maximum `mh_s`.
///   - Ties (equal `mh_s`) are broken DETERMINISTICALLY so the result never
///     depends on input order or float jitter: prefer the larger
///     nonces-per-launch, then larger `blocks`, then larger
///     `threads_per_block`, then larger `nonces_per_thread`.
///   - Returns `None` only when there is no usable measurement at all.
pub fn pick_best(measurements: &[GeometryMeasurement]) -> Option<GeometryMeasurement> {
    let mut best: Option<GeometryMeasurement> = None;
    for &m in measurements {
        if !m.mh_s.is_finite() || m.mh_s <= 0.0 {
            continue;
        }
        match best {
            None => best = Some(m),
            Some(b) => {
                if measurement_is_better(&m, &b) {
                    best = Some(m);
                }
            }
        }
    }
    best
}

/// Strict "is `a` a better pick than `b`?" ordering used by [`pick_best`].
/// Higher MH/s wins; deterministic tie-break on geometry shape otherwise.
fn measurement_is_better(a: &GeometryMeasurement, b: &GeometryMeasurement) -> bool {
    // Primary: higher throughput.
    if a.mh_s > b.mh_s {
        return true;
    }
    if a.mh_s < b.mh_s {
        return false;
    }
    // Tie-breakers (all "larger wins"), in order, so the choice is stable
    // regardless of the order measurements were supplied in.
    let (an, bn) = (a.nonces_per_launch(), b.nonces_per_launch());
    if an != bn {
        return an > bn;
    }
    if a.blocks != b.blocks {
        return a.blocks > b.blocks;
    }
    if a.threads_per_block != b.threads_per_block {
        return a.threads_per_block > b.threads_per_block;
    }
    a.nonces_per_thread > b.nonces_per_thread
}

/// A chosen geometry persisted to the on-disk cache so the auto-tuner runs ONCE
/// and every later start reuses the winner instead of re-benchmarking.
///
/// `device_name` and `device_index` scope the cache entry to the card it was
/// measured on: a rig that swaps GPUs (or runs `--device 1`) won't silently
/// reuse a geometry tuned for a different card. `mh_s` is recorded purely for
/// the operator's reference (and printed on load); it does not affect mining.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedGeometry {
    pub device_index: usize,
    pub device_name: String,
    pub blocks: u32,
    pub threads_per_block: u32,
    pub nonces_per_thread: u32,
    pub mh_s: f64,
}

impl CachedGeometry {
    /// Serialize to the small TOML the cache file holds. Hand-rolled (rather
    /// than `serde`) so the format is explicit, stable, and trivially
    /// round-trip-tested; the device name is quote-escaped so an odd GPU name
    /// can't corrupt the file.
    pub fn to_toml_string(&self) -> String {
        format!(
            "device_index = {}\ndevice_name = \"{}\"\nblocks = {}\nthreads_per_block = {}\nnonces_per_thread = {}\nmh_s = {}\n",
            self.device_index,
            self.device_name.replace('\\', "\\\\").replace('"', "\\\""),
            self.blocks,
            self.threads_per_block,
            self.nonces_per_thread,
            self.mh_s,
        )
    }

    /// Parse a cache file's TOML back into a [`CachedGeometry`]. Returns `None`
    /// on any malformed / incomplete input (a corrupt cache must degrade to
    /// "no cache", never panic or mis-mine), and validates the geometry is
    /// usable (all three dimensions `>= 1`).
    pub fn parse_toml(s: &str) -> Option<CachedGeometry> {
        let mut device_index: Option<usize> = None;
        let mut device_name: Option<String> = None;
        let mut blocks: Option<u32> = None;
        let mut threads_per_block: Option<u32> = None;
        let mut nonces_per_thread: Option<u32> = None;
        let mut mh_s: Option<f64> = None;

        for line in s.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, val) = line.split_once('=')?;
            let key = key.trim();
            let val = val.trim();
            match key {
                "device_index" => device_index = val.parse().ok(),
                "device_name" => device_name = Some(unquote(val)),
                "blocks" => blocks = val.parse().ok(),
                "threads_per_block" => threads_per_block = val.parse().ok(),
                "nonces_per_thread" => nonces_per_thread = val.parse().ok(),
                "mh_s" => mh_s = val.parse().ok(),
                _ => {} // ignore unknown keys (forward-compat)
            }
        }

        let g = CachedGeometry {
            device_index: device_index?,
            device_name: device_name?,
            blocks: blocks?,
            threads_per_block: threads_per_block?,
            nonces_per_thread: nonces_per_thread?,
            mh_s: mh_s?,
        };
        // A geometry with a zero dimension can't mine — reject so a corrupt
        // cache never yields `--blocks 0`.
        if g.blocks == 0 || g.threads_per_block == 0 || g.nonces_per_thread == 0 {
            return None;
        }
        Some(g)
    }
}

/// Strip surrounding double-quotes and unescape `\"` / `\\` from a TOML string
/// value. Lenient: an unquoted value is returned as-is.
fn unquote(v: &str) -> String {
    let inner = v
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(v);
    inner.replace("\\\"", "\"").replace("\\\\", "\\")
}

/// Filename of the per-machine geometry cache inside the app's config dir.
pub const CACHE_FILE_NAME: &str = "autotune.toml";

/// Absolute path to the geometry cache: `<platform config dir>/csd-pool-miner/
/// autotune.toml`, matching where the optional `config.toml` lives. `None` if no
/// config dir can be resolved (no `%APPDATA%` / `$HOME`), in which case the
/// caller skips persistence (auto-tune still works, it just re-benches).
pub fn cache_path() -> Option<PathBuf> {
    platform_config_dir().map(|d| d.join("csd-pool-miner").join(CACHE_FILE_NAME))
}

/// Platform config dir (same rule as `config_file::platform_config_dir`, kept
/// local so this module has no cross-dependency):
///   Windows -> `%APPDATA%`
///   else    -> `$XDG_CONFIG_HOME`, falling back to `$HOME/.config`.
fn platform_config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
            if !x.is_empty() {
                return Some(PathBuf::from(x));
            }
        }
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
    }
}

/// Load a cached geometry for `device_index`, but ONLY if it was tuned for the
/// SAME `device_name` (so swapping cards doesn't reuse a stale geometry).
/// Returns `(blocks, threads_per_block, nonces_per_thread)` on a hit, `None` on
/// any miss: no cache file, unreadable/corrupt file, different device index, or
/// a different GPU at that index. Never panics.
pub fn load_cached_for_device(
    device_index: usize,
    device_name: &str,
) -> Option<(u32, u32, u32)> {
    let path = cache_path()?;
    let s = std::fs::read_to_string(&path).ok()?;
    let g = CachedGeometry::parse_toml(&s)?;
    if g.device_index == device_index && g.device_name == device_name {
        Some((g.blocks, g.threads_per_block, g.nonces_per_thread))
    } else {
        None
    }
}

/// Persist `geom` to [`cache_path`], creating the parent dir if needed. Returns
/// the path written on success. Best-effort: any I/O error is propagated as
/// `Err` for the caller to log, but a failure here is never fatal to mining
/// (the geometry is already chosen and in use; the cache is just an
/// optimisation for the next start).
pub fn save_cached(geom: &CachedGeometry) -> std::io::Result<PathBuf> {
    let path = cache_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no config dir for autotune cache (no %APPDATA% / $HOME)",
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, geom.to_toml_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(b: u32, t: u32, n: u32, mh: f64) -> GeometryMeasurement {
        GeometryMeasurement::new(b, t, n, mh)
    }

    // --- pick_best ---------------------------------------------------------

    #[test]
    fn pick_best_none_on_empty() {
        assert_eq!(pick_best(&[]), None);
    }

    #[test]
    fn pick_best_picks_highest_mh_s() {
        let ms = [
            m(256, 256, 4096, 1200.0),
            m(560, 256, 4096, 3100.5),
            m(1024, 256, 2048, 2900.0),
        ];
        let best = pick_best(&ms).unwrap();
        assert_eq!((best.blocks, best.threads_per_block, best.nonces_per_thread), (560, 256, 4096));
        assert_eq!(best.mh_s, 3100.5);
    }

    #[test]
    fn pick_best_ignores_zero_negative_and_nan() {
        // The only positive-finite measurement must win even though others have
        // numerically "larger-looking" junk values.
        let ms = [
            m(256, 256, 4096, 0.0),          // zero ⇒ ignored
            m(512, 256, 2048, -5.0),         // negative ⇒ ignored
            m(1024, 256, 2048, f64::NAN),    // NaN ⇒ ignored
            m(2048, 256, 1024, f64::INFINITY), // inf ⇒ ignored (not finite)
            m(560, 256, 4096, 42.0),         // the one real measurement
        ];
        let best = pick_best(&ms).unwrap();
        assert_eq!(best.mh_s, 42.0);
        assert_eq!((best.blocks, best.threads_per_block, best.nonces_per_thread), (560, 256, 4096));
    }

    #[test]
    fn pick_best_all_unusable_is_none() {
        let ms = [
            m(256, 256, 4096, 0.0),
            m(512, 256, 2048, f64::NAN),
            m(1024, 256, 2048, -1.0),
        ];
        assert_eq!(pick_best(&ms), None);
    }

    #[test]
    fn pick_best_tie_break_is_deterministic_and_order_independent() {
        // Two geometries with the EXACT same MH/s: the larger nonces-per-launch
        // must win, regardless of input order.
        let small = m(256, 256, 1024, 1000.0); // 67,108,864 nonces/launch
        let big = m(1024, 256, 1024, 1000.0); //  268,435,456 nonces/launch
        assert_eq!(pick_best(&[small, big]).unwrap(), big);
        assert_eq!(pick_best(&[big, small]).unwrap(), big, "order must not matter");

        // Same nonces-per-launch but different shape ⇒ larger `blocks` wins.
        let a = m(512, 256, 2048, 50.0); // 268,435,456
        let b = m(1024, 128, 2048, 50.0); // 268,435,456 (same product), more blocks
        assert_eq!(pick_best(&[a, b]).unwrap(), b);
        assert_eq!(pick_best(&[b, a]).unwrap(), b);
    }

    // --- candidate_geometries ---------------------------------------------

    #[test]
    fn candidates_are_nonempty_valid_and_include_the_shipped_default() {
        let c = candidate_geometries();
        assert!(!c.is_empty(), "must benchmark at least one geometry");
        // No zero dimension (every candidate is a runnable launch).
        for (b, t, n) in &c {
            assert!(*b >= 1 && *t >= 1 && *n >= 1, "candidate {:?} has a zero dim", (b, t, n));
        }
        // The shipped default geometry must be in the set so auto-tune can never
        // pick something strictly worse than the current default.
        assert!(
            c.contains(&(560, 256, 4096)),
            "candidate set must include the shipped default 560x256x4096"
        );
        // Candidates are distinct (no wasted duplicate benchmark).
        let mut sorted = c.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), c.len(), "candidate geometries must be distinct");
    }

    #[test]
    fn candidates_include_4096x256x512() {
        // v0.2.0 adds the deep-block geometry alongside the proven winner
        // 2048x256x1024; both must be in the swept set.
        let c = candidate_geometries();
        assert!(
            c.contains(&(4096, 256, 512)),
            "candidate set must include 4096x256x512"
        );
        assert!(
            c.contains(&(2048, 256, 1024)),
            "candidate set must include the A/B winner 2048x256x1024"
        );
    }

    // --- CachedGeometry round-trip ----------------------------------------

    #[test]
    fn cached_geometry_round_trips_through_toml() {
        let g = CachedGeometry {
            device_index: 1,
            device_name: "NVIDIA GeForce RTX 4090".to_string(),
            blocks: 1024,
            threads_per_block: 256,
            nonces_per_thread: 2048,
            mh_s: 9876.5,
        };
        let s = g.to_toml_string();
        let back = CachedGeometry::parse_toml(&s).expect("round-trip must parse");
        assert_eq!(back, g);
    }

    #[test]
    fn cached_geometry_round_trips_name_with_quotes_and_backslashes() {
        // A pathological device name must survive the quote/backslash escaping.
        let g = CachedGeometry {
            device_index: 0,
            device_name: r#"weird "GPU" \ name"#.to_string(),
            blocks: 560,
            threads_per_block: 256,
            nonces_per_thread: 4096,
            mh_s: 1.0,
        };
        let back = CachedGeometry::parse_toml(&g.to_toml_string()).unwrap();
        assert_eq!(back.device_name, g.device_name);
        assert_eq!(back, g);
    }

    #[test]
    fn parse_toml_rejects_incomplete_or_corrupt() {
        // Missing required keys ⇒ None.
        assert!(CachedGeometry::parse_toml("blocks = 560\nthreads_per_block = 256\n").is_none());
        // Garbage ⇒ None, never a panic.
        assert!(CachedGeometry::parse_toml("this is not toml at all").is_none());
        assert!(CachedGeometry::parse_toml("").is_none());
        // A zero dimension is rejected (would mean an unminable --blocks 0).
        let zero = "device_index = 0\ndevice_name = \"x\"\nblocks = 0\nthreads_per_block = 256\nnonces_per_thread = 4096\nmh_s = 1.0\n";
        assert!(CachedGeometry::parse_toml(zero).is_none());
    }

    #[test]
    fn parse_toml_ignores_unknown_keys() {
        let s = "future_key = 99\ndevice_index = 0\ndevice_name = \"x\"\nblocks = 560\nthreads_per_block = 256\nnonces_per_thread = 4096\nmh_s = 2.5\n";
        let g = CachedGeometry::parse_toml(s).unwrap();
        assert_eq!(g.blocks, 560);
        assert_eq!(g.mh_s, 2.5);
    }

    // --- cache_path -------------------------------------------------------

    #[test]
    fn cache_path_when_resolvable_ends_with_expected_suffix() {
        // Drive the dir from an env var we control so the test is hermetic.
        #[cfg(windows)]
        let key = "APPDATA";
        #[cfg(not(windows))]
        let key = "XDG_CONFIG_HOME";

        let prev = std::env::var_os(key);
        std::env::set_var(key, if cfg!(windows) { "C:\\cfg" } else { "/cfg" });
        let p = cache_path().expect("cache_path resolvable when the config dir env is set");
        // Restore env before asserting (so a failed assert can't leak it).
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        let p = p.to_string_lossy().replace('\\', "/");
        assert!(
            p.ends_with("csd-pool-miner/autotune.toml"),
            "unexpected cache path: {p}"
        );
    }
}
