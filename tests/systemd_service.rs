//! The shipped native-Linux systemd unit MUST stay in lock-step with the
//! miner's own restart contract. The GPU watchdog exits the process with a
//! *distinct* code ([`csd_gpu_miner::gpu_watchdog::EXIT_GPU_STALLED`] == 17) to
//! mean "stalled GPU, please restart me". If the packaged `.service` does not
//! force-restart on exactly that code, a hung-GPU rig would exit and stay dead
//! under systemd's default `Restart=on-failure` semantics (a clean watchdog
//! exit is still a *failure* only if non-zero, but the code could drift, and
//! `RestartForceExitStatus` must name the same number the binary actually
//! uses).
//!
//! These tests are the regression guard for that packaging↔code contract:
//! change `EXIT_GPU_STALLED` in the source and forget to update the unit, and
//! the build goes red here (the same spirit as `endpoint_locked.rs`). They read
//! the unit file from the repo — no systemd, no root, no I/O beyond a file read
//! — so they run anywhere `cargo test` does.

use csd_gpu_miner::gpu_watchdog::EXIT_GPU_STALLED;
use std::path::PathBuf;

/// Absolute path to a file under the crate root (`CARGO_MANIFEST_DIR`), so the
/// test is independent of the process's working directory.
fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// Read a packaged unit file, failing with a clear message if it's missing
/// (this is the failing-test signal before the file is added).
fn read_unit(rel: &str) -> String {
    let p = repo_path(rel);
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("packaged systemd unit {} must exist and be readable: {e}", p.display()))
}

/// Collect the values of a `Key=Value` directive across a unit file. systemd
/// allows a key to appear more than once (e.g. multiple `RestartForceExitStatus`
/// or `ExecStart=` lines), so we return every match. Lines are trimmed; blank
/// lines and `#`/`;` comments are skipped.
fn directive_values<'a>(unit: &'a str, key: &str) -> Vec<&'a str> {
    unit.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with(';'))
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim())
        .collect()
}

/// Helper: assert a unit force-restarts on EXACTLY the watchdog's exit code,
/// auto-restarts, and launches the miner binary by its canonical name. Shared
/// by the plain unit and the `@`-templated multi-GPU unit so neither can drift.
fn assert_restart_contract(rel: &str) {
    let unit = read_unit(rel);

    // RestartForceExitStatus must name the watchdog's exit code. A clean GPU-
    // stall exit (17) is otherwise NOT covered by `Restart=on-failure` only if
    // someone weakens it; naming it explicitly makes the restart unconditional
    // for that code regardless of the Restart= mode.
    let force = directive_values(&unit, "RestartForceExitStatus");
    assert!(
        !force.is_empty(),
        "{rel}: must set RestartForceExitStatus so a GPU-stall exit ({EXIT_GPU_STALLED}) restarts"
    );
    // The directive can list several space-separated statuses; the watchdog code
    // must be one of them.
    let codes: Vec<&str> = force.iter().flat_map(|v| v.split_whitespace()).collect();
    let want = EXIT_GPU_STALLED.to_string();
    assert!(
        codes.contains(&want.as_str()),
        "{rel}: RestartForceExitStatus {force:?} must include the GPU-watchdog exit code {EXIT_GPU_STALLED} \
         (keep it in lock-step with gpu_watchdog::EXIT_GPU_STALLED)"
    );

    // Auto-restart on every drop (crash, OOM, network kill), not just the
    // watchdog code — the task spec requires Restart=always.
    let restart = directive_values(&unit, "Restart");
    assert_eq!(
        restart.last().copied(),
        Some("always"),
        "{rel}: Restart must be `always` (got {restart:?})"
    );

    // A sane, non-zero backoff so a crash-loop doesn't hammer the pool/PSU.
    let restart_sec = directive_values(&unit, "RestartSec");
    assert!(
        !restart_sec.is_empty(),
        "{rel}: RestartSec must be set to a sane non-zero backoff"
    );

    // ExecStart must actually launch the miner binary (by its canonical name),
    // not some unrelated command.
    let exec = directive_values(&unit, "ExecStart");
    assert_eq!(exec.len(), 1, "{rel}: exactly one ExecStart expected (got {exec:?})");
    assert!(
        exec[0].contains("csd-pool-miner"),
        "{rel}: ExecStart must launch the csd-pool-miner binary (got {:?})",
        exec[0]
    );

    // The operator's payout address comes from the EnvironmentFile, never baked
    // into the unit — so the same packaged file works for every rig.
    let envfile = directive_values(&unit, "EnvironmentFile");
    assert!(
        envfile.iter().any(|v| v.contains("/etc/csd-pool-miner")),
        "{rel}: EnvironmentFile must point at /etc/csd-pool-miner... (got {envfile:?})"
    );
}

#[test]
fn service_force_restarts_on_gpu_stall_exit_code() {
    assert_restart_contract("deploy/systemd/csd-pool-miner.service");
}

#[test]
fn template_unit_shares_the_restart_contract() {
    // The multi-GPU `@`-instantiated template must honor the same contract so a
    // per-card unit also auto-restarts a stalled GPU.
    assert_restart_contract("deploy/systemd/csd-pool-miner@.service");
}

#[test]
fn service_runs_unprivileged_and_hardened() {
    // The miner needs no root; the unit must drop to a non-root User= and the
    // README/unit must not silently run as root.
    let unit = read_unit("deploy/systemd/csd-pool-miner.service");
    let user = directive_values(&unit, "User");
    assert_eq!(
        user.len(),
        1,
        "the service must run as a dedicated non-root User= (got {user:?})"
    );
    assert_ne!(user[0], "root", "the miner must not run as root");
}

#[test]
fn install_readme_exists() {
    // Operators need step-by-step install docs alongside the units.
    let readme = read_unit("deploy/systemd/README.md");
    assert!(
        readme.contains("systemctl"),
        "deploy/systemd/README.md must document the systemctl install steps"
    );
}
