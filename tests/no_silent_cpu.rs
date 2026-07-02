//! No-silent-CPU hard-error guard.
//!
//! A GPU-intended rig whose CUDA/OpenCL fails to init must HARD-ERROR and
//! refuse to mine, never quietly degrade to a CPU backend (a silent CPU
//! fallback bleeds fleet hashrate invisibly). Two layers enforce this:
//!   1. the fleet default `--backend cuda` (forced) already `bail!`s on init
//!      failure — pinned in `autotune_default::default_backend_is_cuda`;
//!   2. the `--backend auto` CPU-descent is now opt-in only, gated by
//!      `--allow-cpu-fallback` (default OFF). The gate decision is a pure
//!      helper so it's testable without a GPU.

#[allow(dead_code)]
#[path = "../src/main.rs"]
mod miner_bin;

use clap::Parser;
use miner_bin::{auto_may_fall_back_to_cpu, BackendChoice, Cli};

fn dummy_addr() -> String {
    "a".repeat(40)
}

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).expect("CLI surface must parse")
}

#[test]
fn auto_without_flag_refuses_cpu() {
    assert!(
        !auto_may_fall_back_to_cpu(false),
        "auto must NOT silently fall back to CPU without --allow-cpu-fallback"
    );
}

#[test]
fn auto_with_flag_permits_cpu() {
    assert!(
        auto_may_fall_back_to_cpu(true),
        "auto may fall back to CPU only when --allow-cpu-fallback is set"
    );
}

#[test]
fn flag_defaults_off() {
    let cli = parse(&["csd-pool-miner", "--address", &dummy_addr()]);
    assert!(
        !cli.allow_cpu_fallback,
        "--allow-cpu-fallback must default to OFF"
    );
}

#[test]
fn forced_cpu_still_allowed() {
    // Explicit `--backend cpu` is a deliberate operator choice, never "silent".
    let cli = parse(&["csd-pool-miner", "--address", &dummy_addr(), "--backend", "cpu"]);
    assert!(matches!(cli.backend, BackendChoice::Cpu));
}
