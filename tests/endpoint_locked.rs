//! The shipped binary connects to exactly one compiled-in pool and exposes NO
//! flag that can repoint it. These tests are the regression guard for that
//! property: if anyone re-adds an endpoint-naming long flag (`--pool`, `--url`,
//! `--host`, …), the build goes red here.
//!
//! The binary's `Cli` (a private `clap::Parser` in `src/main.rs`) is reached by
//! `#[path]`-including the binary source as a module. That pulls in the whole
//! `main.rs`, including its own `#[cfg(test)] mod tests`, which is harmless: we
//! only touch the `Cli` type.

// Including the whole binary means most of its items (`fn main`, the mining
// drivers, etc.) are never called from this test crate. They are exercised by
// the binary and by `main.rs`'s own unit tests — not dead — so silence the
// test-crate-only dead-code noise here to keep `cargo test` output pristine.
#[allow(dead_code)]
#[path = "../src/main.rs"]
mod miner_bin;

use clap::{CommandFactory, Parser};
use miner_bin::Cli;

/// A syntactically valid addr20 (40 hex chars) so parsing fails *only* on the
/// endpoint flag, never on a bad address.
fn dummy_addr() -> String {
    "a".repeat(40)
}

#[test]
fn pool_flag_is_rejected() {
    // `--pool host:port` must no longer be a known argument.
    let parsed = Cli::try_parse_from([
        "csd-pool-miner",
        "--address",
        &dummy_addr(),
        "--pool",
        "evil.example:3333",
    ]);
    assert!(
        parsed.is_err(),
        "--pool must be rejected (the binary may not be repointed), but it parsed OK"
    );
}

#[test]
fn url_flag_is_rejected() {
    // The old `--url` alias must be gone too.
    let parsed = Cli::try_parse_from([
        "csd-pool-miner",
        "--address",
        &dummy_addr(),
        "--url",
        "evil.example:3333",
    ]);
    assert!(
        parsed.is_err(),
        "--url must be rejected (the binary may not be repointed), but it parsed OK"
    );
}

#[test]
fn no_endpoint_naming_long_flag_exists() {
    // Defense in depth: assert NONE of the common endpoint-naming long flags is
    // wired up, so a future rename can't silently reintroduce a repoint vector.
    const FORBIDDEN: &[&str] = &[
        "pool", "url", "host", "server", "node", "endpoint", "connect",
    ];
    let cmd = Cli::command();
    for arg in cmd.get_arguments() {
        if let Some(long) = arg.get_long() {
            assert!(
                !FORBIDDEN.contains(&long),
                "endpoint-naming long flag --{long} is present; the binary must not be repointable"
            );
        }
        // Also catch it hiding behind a visible alias (e.g. `--url` for `--pool`).
        for a in arg.get_visible_aliases().into_iter().flatten() {
            assert!(
                !FORBIDDEN.contains(&a),
                "endpoint-naming alias --{a} is present; the binary must not be repointable"
            );
        }
    }
}

/// A plain `--address` invocation must still parse (sanity: we removed only the
/// endpoint override, not the whole CLI).
#[test]
fn address_only_still_parses() {
    let parsed = Cli::try_parse_from(["csd-pool-miner", "--address", &dummy_addr()]);
    assert!(parsed.is_ok(), "address-only invocation must still parse");
}
