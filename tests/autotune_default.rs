//! Auto-tune-every-start wiring guard.
//!
//! v0.2.0 flips the geometry sweep to run at EVERY mining-session start by
//! default (operator directive). These tests pin that default and its escape
//! hatches at the CLI surface — no GPU needed, they exercise pure clap parsing
//! plus the pure `Cli::should_auto_tune()` decision helper. Same
//! `#[path]`-include-the-binary discipline as `winsvc_modes.rs` /
//! `endpoint_locked.rs`.

#[allow(dead_code)]
#[path = "../src/main.rs"]
mod miner_bin;

use clap::Parser;
use miner_bin::{BackendChoice, Cli};

/// A syntactically valid addr20 (40 hex chars) so parsing never fails on the
/// address — only the behavior under test matters.
fn dummy_addr() -> String {
    "a".repeat(40)
}

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).expect("CLI surface must parse")
}

#[test]
fn autotune_on_by_default() {
    // Bare invocation (no flags): the sweep must be ON by default now.
    let cli = parse(&["csd-pool-miner", "--address", &dummy_addr()]);
    assert!(
        cli.auto_tune,
        "auto_tune must default to TRUE (sweep every start)"
    );
    assert!(
        cli.should_auto_tune(),
        "a bare invocation must resolve to should_auto_tune()==true"
    );
}

#[test]
fn no_auto_tune_flag_suppresses() {
    let cli = parse(&["csd-pool-miner", "--address", &dummy_addr(), "--no-auto-tune"]);
    assert!(
        !cli.should_auto_tune(),
        "--no-auto-tune must suppress the startup sweep"
    );
}

#[test]
fn explicit_pinned_geometry_suppresses() {
    // An explicit pinned geometry is a first-class escape hatch: main() records
    // it in `geometry_set_explicitly`. Simulate that (clap alone can't observe
    // ValueSource without main()'s post-parse step) and assert it suppresses.
    let mut cli = parse(&["csd-pool-miner", "--address", &dummy_addr(), "--blocks", "2048"]);
    cli.geometry_set_explicitly = true;
    assert!(
        !cli.should_auto_tune(),
        "an explicit pinned geometry must suppress the sweep"
    );
}

#[test]
fn default_backend_matches_compiled_features() {
    // Per-variant fleet default (SPEC CHANGE, v0.2.0 pre-release P0): each
    // build's default backend targets ITS OWN compiled GPU API — cuda build →
    // cuda, opencl build → opencl, neither → cpu. The old unconditional "cuda"
    // default bricked the amd + cpu release variants: BackendChoice::Cuda on a
    // no-cuda build hits `bail!("cuda backend not compiled in")` at EVERY
    // start (crash-loop, 0 H/s fleet-wide after self-update). No-silent-CPU is
    // preserved PER VARIANT: a GPU build still lands on its hard-erroring
    // forced path (never the silent-CPU-capable Auto path), and the dedicated
    // cpu variant's PURPOSE is CPU mining — not a "silent fallback".
    let cli = parse(&["csd-pool-miner", "--address", &dummy_addr()]);
    if cfg!(feature = "cuda") {
        assert!(
            matches!(cli.backend, BackendChoice::Cuda),
            "cuda build: default --backend must be cuda (forced, hard-erroring)"
        );
    } else if cfg!(feature = "opencl") {
        assert!(
            matches!(cli.backend, BackendChoice::Opencl),
            "opencl build: default --backend must be opencl (forced, hard-erroring)"
        );
    } else {
        assert!(
            matches!(cli.backend, BackendChoice::Cpu),
            "cpu-only build: default --backend must be cpu (CPU mining IS the product)"
        );
    }
}
