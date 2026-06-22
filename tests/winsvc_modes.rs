//! The native Windows-service CLI surface must stay wired to the pure
//! mode-parser. These tests reach the binary's private `Cli` (a `clap::Parser`
//! in `src/main.rs`) by `#[path]`-including the binary source as a module — the
//! same technique `endpoint_locked.rs` uses — and assert the three service flags
//! (`--install-service` / `--uninstall-service` / `--run-as-service`) parse and
//! map to the right [`ServiceMode`], that they are mutually exclusive, and that
//! their absence is "no service mode" (normal mining).
//!
//! These cover the always-compiled, platform-independent half (flag → mode). The
//! SCM interaction itself needs a live Windows Service Control Manager + admin
//! rights, so it is deliberately NOT exercised here (it is guarded behind
//! `cfg(all(windows, feature = "winsvc"))` in `winsvc::imp`). The unit tests in
//! `src/winsvc.rs` cover the pure parser directly; this is the end-to-end clap
//! wiring guard.

#[allow(dead_code)]
#[path = "../src/main.rs"]
mod miner_bin;

use clap::Parser;
use csd_gpu_miner::winsvc::ServiceMode;
use miner_bin::Cli;

/// A syntactically valid addr20 (40 hex chars) so parsing never fails on the
/// address — only the behavior under test matters.
fn dummy_addr() -> String {
    "a".repeat(40)
}

/// Parse argv into a `Cli`, panicking with the clap error if the surface is
/// missing the flag (which is itself a failure signal).
fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).expect("these service flags must be a known CLI surface")
}

#[test]
fn no_service_flag_is_no_service_mode() {
    let cli = parse(&["csd-pool-miner", "--address", &dummy_addr()]);
    assert_eq!(
        cli.service_mode().unwrap(),
        None,
        "without a service flag the miner runs in the foreground (no service mode)"
    );
}

#[test]
fn install_flag_maps_to_install_mode() {
    let cli = parse(&["csd-pool-miner", "--address", &dummy_addr(), "--install-service"]);
    assert_eq!(cli.service_mode().unwrap(), Some(ServiceMode::Install));
}

#[test]
fn uninstall_flag_maps_to_uninstall_mode() {
    // Uninstall needs no address.
    let cli = parse(&["csd-pool-miner", "--uninstall-service"]);
    assert_eq!(cli.service_mode().unwrap(), Some(ServiceMode::Uninstall));
}

#[test]
fn run_flag_maps_to_run_mode() {
    let cli = parse(&["csd-pool-miner", "--address", &dummy_addr(), "--run-as-service"]);
    assert_eq!(cli.service_mode().unwrap(), Some(ServiceMode::Run));
}

#[test]
fn conflicting_service_flags_are_rejected() {
    // The flags parse individually, but requesting two actions at once is an
    // error (resolved by `service_mode`, not by clap, so the message is ours).
    let cli = parse(&[
        "csd-pool-miner",
        "--address",
        &dummy_addr(),
        "--install-service",
        "--uninstall-service",
    ]);
    let err = cli.service_mode().unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("mutually exclusive"),
        "conflicting service flags must error as mutually exclusive (got {msg:?})"
    );
}

#[test]
fn install_and_run_together_are_rejected() {
    let cli = parse(&[
        "csd-pool-miner",
        "--address",
        &dummy_addr(),
        "--install-service",
        "--run-as-service",
    ]);
    assert!(
        cli.service_mode().is_err(),
        "--install-service + --run-as-service must be rejected"
    );
}

/// On the default build (no `winsvc` feature), actually *executing* a service
/// action must be a clean, non-panicking error that explains how to enable the
/// feature — never a silent success and never a crash. This proves the no-op
/// stub is the fallback when the feature is off.
#[cfg(not(all(windows, feature = "winsvc")))]
#[test]
fn executing_a_service_action_without_winsvc_errors_cleanly() {
    let err = csd_gpu_miner::winsvc::run_service_action(
        ServiceMode::Install,
        Box::new(|_stop| Ok(())),
    )
    .expect_err("install must fail on a build without the winsvc feature");
    let msg = format!("{err}");
    assert!(
        msg.contains("winsvc") && msg.contains("--install-service"),
        "the error must name the flag and the winsvc feature (got {msg:?})"
    );
}
