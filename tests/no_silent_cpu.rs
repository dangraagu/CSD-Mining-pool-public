//! No-silent-CPU hard-error guard.
//!
//! A GPU-intended rig whose CUDA/OpenCL fails to init must HARD-ERROR and
//! refuse to mine, never quietly degrade to a CPU backend (a silent CPU
//! fallback bleeds fleet hashrate invisibly). Two layers enforce this:
//!   1. the fleet default backend is PER BUILD VARIANT (`default_backend()`:
//!      cuda build → cuda, opencl build → opencl, cpu build → cpu) — each GPU
//!      variant lands on its own FORCED backend that `bail!`s on init failure.
//!      The pure fn is asserted for all four feature combinations below (every
//!      build asserts every combination); the clap surface is pinned in
//!      `autotune_default::default_backend_matches_compiled_features`. The cpu
//!      variant's default IS cpu — CPU mining is that build's purpose, not a
//!      silent fallback (an unconditional cuda default would crash-loop the
//!      amd/cpu variants: "cuda backend not compiled in" at every start);
//!   2. the `--backend auto` CPU-descent is now opt-in only, gated by
//!      `--allow-cpu-fallback` (default OFF). The gate decision is a pure
//!      helper so it's testable without a GPU.

#[allow(dead_code)]
#[path = "../src/main.rs"]
mod miner_bin;

use clap::Parser;
use miner_bin::{auto_may_fall_back_to_cpu, default_backend, BackendChoice, Cli, DEFAULT_BACKEND};

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
fn default_backend_all_four_feature_combinations() {
    // Pure-fn truth table, asserted on EVERY build (no cfg gates): a build
    // that cannot compile the cuda feature still proves what a cuda build's
    // default would be. cuda wins over opencl when both are compiled in.
    assert_eq!(default_backend(true, true), "cuda", "cuda+opencl build defaults to cuda");
    assert_eq!(default_backend(true, false), "cuda", "cuda-only build defaults to cuda");
    assert_eq!(default_backend(false, true), "opencl", "opencl-only build defaults to opencl");
    assert_eq!(default_backend(false, false), "cpu", "no-GPU-feature build defaults to cpu");
}

#[test]
fn compiled_default_matches_this_builds_features() {
    // The thin cfg! wiring: THIS build's clap default must be exactly what the
    // pure fn selects for this build's compiled feature set.
    assert_eq!(
        DEFAULT_BACKEND,
        default_backend(cfg!(feature = "cuda"), cfg!(feature = "opencl")),
        "DEFAULT_BACKEND const must be the pure default_backend() of the compiled features"
    );
}

#[test]
fn forced_cpu_still_allowed() {
    // Explicit `--backend cpu` is a deliberate operator choice, never "silent".
    let cli = parse(&["csd-pool-miner", "--address", &dummy_addr(), "--backend", "cpu"]);
    assert!(matches!(cli.backend, BackendChoice::Cpu));
}
