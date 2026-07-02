//! Sweep-failure fallback guard.
//!
//! When the startup geometry sweep fails, the resolver must fall back to the
//! last-known-good CACHED geometry for this card, else the shipped DEFAULT —
//! and NEVER crash. The GPU-touching sweep itself can't run in CI, so the
//! decision layer is a pure helper (`geometry_after_failed_sweep`) tested here
//! with plain values, same discipline as `autotune::pick_best`.

#[allow(dead_code)]
#[path = "../src/main.rs"]
mod miner_bin;

use miner_bin::geometry_after_failed_sweep;

#[test]
fn falls_back_to_cache_when_present() {
    // A last-known-good cached geometry wins over the shipped default.
    assert_eq!(
        geometry_after_failed_sweep(Some((2048, 256, 1024)), (560, 256, 4096)),
        (2048, 256, 1024)
    );
}

#[test]
fn falls_back_to_default_when_no_cache() {
    // No cache ⇒ shipped default (never a crash, never a zero geometry).
    assert_eq!(
        geometry_after_failed_sweep(None, (560, 256, 4096)),
        (560, 256, 4096)
    );
}
