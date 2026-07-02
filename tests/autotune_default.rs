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
fn default_backend_is_cuda() {
    // Fleet default must land on the hard-erroring forced-CUDA path (not the
    // silent-CPU-capable Auto path).
    let cli = parse(&["csd-pool-miner", "--address", &dummy_addr()]);
    assert!(
        matches!(cli.backend, BackendChoice::Cuda),
        "default --backend must be cuda (no silent CPU fallthrough)"
    );
}
