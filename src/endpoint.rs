//! Compiled-in pool endpoint, lightly obfuscated.
//!
//! The public build connects to ONE pool by default and exposes no
//! `--pool`/`--url` override flag. To keep the literal host:port from showing
//! up in a plain `strings <binary>` dump (and to make a casual hex-edit of the
//! binary slightly more annoying), the endpoint is stored **XOR-scrambled** as a
//! byte array and reconstructed at runtime by [`pool_endpoint`].
//!
//! This is **obfuscation, not security** — anyone determined can recover it by
//! running the binary or replicating the XOR. The point is only that the raw
//! `host:3333` string is not sitting in the read-only data segment in cleartext.
//!
//! ## Why a scrambled *byte array* and not `xor("plaintext")`
//! An earlier version stored the cleartext as a `const &str` and XOR-folded it
//! at runtime. With `opt-level=3` + fat LTO the optimizer evaluated the entire
//! XOR round-trip at compile time and embedded the **plaintext** literal in the
//! binary — defeating the whole point (verified: `strings` showed the host).
//! So the cleartext now never appears in source at all: only the scrambled
//! bytes are a compile-time constant, and [`std::hint::black_box`] stops the
//! optimizer from folding the decode back to a plaintext literal.
//!
//! ## Cutting a release
//! Replace the placeholder array below with the scrambled form of the real
//! pool endpoint:
//!   1. Pick your live endpoint string, e.g. `"pool.example.com:3333"`.
//!   2. Scramble each byte with [`XOR_KEY`] (`byte ^ 0x5a`) and paste the
//!      resulting bytes into `SCRAMBLED_ENDPOINT`. (A one-liner in any language,
//!      or `python3 -c "print([b ^ 0x5a for b in b'host:3333'])"`.)
//!   3. Rebuild. The `pool_endpoint` round-trip is covered by a unit test that
//!      reconstructs the expected plaintext at test time from the same scramble,
//!      so a mistyped byte is caught before it ships.
//!
//! Prefer a stable hostname (which survives VPS IP changes) over a raw IP. The
//! obfuscated hostname is fine to commit; avoid committing a raw production IP.

use std::hint::black_box;

/// Single-byte XOR key used to scramble/descramble the endpoint. A fixed key is
/// sufficient for the stated goal (defeat `strings`, not a reverse engineer).
const XOR_KEY: u8 = 0x5a;

/// The live pool endpoint (`host:port`), with every byte XOR'd by [`XOR_KEY`].
/// The cleartext host never appears in source — only this scrambled form does.
/// To change the endpoint, re-scramble the new `host:port` (see "Cutting a
/// release" above); the test below reconstructs the expected value from this
/// same table, so no plaintext host literal lives anywhere in the crate.
const SCRAMBLED_ENDPOINT: &[u8] = &[
    0x2a, 0x35, 0x35, 0x36, 0x74, 0x23, 0x3b, 0x37, 0x3b, 0x3e, 0x2f, 0x35,
    0x74, 0x34, 0x35, 0x60, 0x69, 0x69, 0x69, 0x69,
];

/// Decode and return the pool endpoint as a `host:port` string.
///
/// Reconstructs the cleartext at runtime by XOR-ing the scrambled table.
/// [`black_box`] is applied to each byte and the key so the optimizer can't
/// const-fold the loop and re-materialize the plaintext as a literal in the
/// binary's data segment. Always valid UTF-8 (the source endpoint is ASCII and
/// XOR is a pure byte permutation that round-trips exactly).
pub fn pool_endpoint() -> String {
    let key = black_box(XOR_KEY);
    let decoded: Vec<u8> = SCRAMBLED_ENDPOINT
        .iter()
        .map(|&b| black_box(b) ^ key)
        .collect();
    String::from_utf8(decoded).expect("pool endpoint decodes to valid UTF-8")
}

/// An ordered list of pool endpoints with a current selection, for failover.
///
/// Index 0 is the **primary**. [`advance`](Self::advance) rotates to the next
/// endpoint after a failed connect (so a dead pool is left behind);
/// [`maybe_failback`](Self::maybe_failback) returns to the primary after a quiet
/// interval on a backup. All decisions are pure (a `now_ms` is injected), so the
/// rotation/failback policy is unit-tested without sockets or real time.
pub struct EndpointList {
    all: Vec<String>,
    current: usize,
    last_failback_ms: u64,
}

impl EndpointList {
    /// Build from a non-empty, ordered endpoint list (index 0 = primary).
    pub fn new(all: Vec<String>) -> Self {
        debug_assert!(!all.is_empty(), "EndpointList needs >= 1 endpoint");
        EndpointList {
            all,
            current: 0,
            last_failback_ms: 0,
        }
    }

    /// The currently-selected endpoint.
    pub fn current(&self) -> &str {
        &self.all[self.current]
    }

    /// True if the current selection is the primary (index 0).
    pub fn is_primary(&self) -> bool {
        self.current == 0
    }

    /// Rotate to the next endpoint (wrapping), returning it. No-op for a single
    /// endpoint. Called after a failed handshake to try the next pool.
    pub fn advance(&mut self) -> &str {
        if self.all.len() > 1 {
            self.current = (self.current + 1) % self.all.len();
        }
        &self.all[self.current]
    }

    /// If currently on a backup and `interval_ms` has elapsed since the last
    /// failback (or since we were last on the primary), jump back to the primary
    /// so the next connect prefers it. Returns whether it failed back. While on
    /// the primary it just keeps the failback clock fresh and returns `false`.
    pub fn maybe_failback(&mut self, now_ms: u64, interval_ms: u64) -> bool {
        if self.current == 0 {
            self.last_failback_ms = now_ms;
            return false;
        }
        // On a backup with the failback clock never started (e.g. a deliberate
        // failover advanced off the primary before any failback check ran): start
        // it NOW and hold the backup for a full interval. Treating 0 as
        // "infinitely long ago" would instantly snap back to a possibly-bad
        // primary, defeating the failover (v0.1.9 #2 regression).
        if self.last_failback_ms == 0 {
            self.last_failback_ms = now_ms;
            return false;
        }
        if now_ms.saturating_sub(self.last_failback_ms) >= interval_ms {
            self.current = 0;
            self.last_failback_ms = now_ms;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reconstruct the expected endpoint by descrambling [`SCRAMBLED_ENDPOINT`]
    /// independently of [`pool_endpoint`] (no `black_box`, plain XOR here). The
    /// cleartext host therefore never exists as a literal anywhere in the crate —
    /// not even in the test — yet we still verify the decode is correct.
    fn expected_from_scramble() -> String {
        let decoded: Vec<u8> = SCRAMBLED_ENDPOINT.iter().map(|&b| b ^ XOR_KEY).collect();
        String::from_utf8(decoded).expect("scrambled endpoint decodes to valid UTF-8")
    }

    #[test]
    fn pool_endpoint_decodes_to_expected() {
        // `pool_endpoint()` (with black_box) must agree with the independent,
        // literal-free reconstruction from the same scramble table.
        assert_eq!(pool_endpoint(), expected_from_scramble());
    }

    #[test]
    fn scrambled_table_is_actually_scrambled() {
        // The stored bytes must NOT equal the plaintext bytes (else the
        // obfuscation is a no-op and `strings` would reveal the host).
        let plain = expected_from_scramble();
        assert_ne!(SCRAMBLED_ENDPOINT, plain.as_bytes());
        // And it must have the right length (catches a truncated paste).
        assert_eq!(SCRAMBLED_ENDPOINT.len(), plain.len());
    }

    /// Pin the decoded endpoint to a known SHA-256 digest. The digest is
    /// preimage-resistant, so no plaintext host literal lives in source — yet
    /// this catches ANY corruption of `SCRAMBLED_ENDPOINT`, including a
    /// valid-but-wrong byte that the descramble round-trip cannot see, before a
    /// wrong-host binary could ship with a green suite.
    #[test]
    fn decoded_endpoint_matches_pinned_digest() {
        use sha2::{Digest, Sha256};
        let got = hex::encode(Sha256::digest(pool_endpoint().as_bytes()));
        assert_eq!(
            got, "3e8a0f477e5dfbb50e5c8092dc01f55ccd62a2d372b65ac7e715937fbe86dd6d",
            "decoded endpoint changed: if you intentionally repointed the pool, \
             recompute this digest; otherwise SCRAMBLED_ENDPOINT is corrupted"
        );
    }

    #[test]
    fn round_trips_for_an_arbitrary_endpoint() {
        // Document the scramble recipe a release-cutter uses: scramble then
        // decode must be the identity.
        let sample = b"pool.example.com:3333";
        let scrambled: Vec<u8> = sample.iter().map(|&b| b ^ XOR_KEY).collect();
        assert_ne!(&scrambled[..], &sample[..]);
        let back: Vec<u8> = scrambled.iter().map(|&b| b ^ XOR_KEY).collect();
        assert_eq!(&back[..], &sample[..]);
    }

    #[test]
    fn endpoint_list_starts_on_primary() {
        let el = EndpointList::new(vec!["a:1".into(), "b:2".into()]);
        assert_eq!(el.current(), "a:1");
        assert!(el.is_primary());
    }

    #[test]
    fn endpoint_list_advance_rotates_and_wraps() {
        let mut el = EndpointList::new(vec!["a:1".into(), "b:2".into(), "c:3".into()]);
        assert_eq!(el.advance(), "b:2");
        assert!(!el.is_primary());
        assert_eq!(el.advance(), "c:3");
        assert_eq!(el.advance(), "a:1"); // wraps back to primary
        assert!(el.is_primary());
    }

    #[test]
    fn endpoint_list_single_endpoint_advance_is_noop() {
        let mut el = EndpointList::new(vec!["only:1".into()]);
        assert_eq!(el.advance(), "only:1");
        assert_eq!(el.advance(), "only:1");
        assert!(el.is_primary());
    }

    #[test]
    fn endpoint_list_fails_back_to_primary_after_interval() {
        let mut el = EndpointList::new(vec!["a:1".into(), "b:2".into()]);
        // Establish the failback clock while on the primary at t=1000.
        assert!(!el.maybe_failback(1000, 5000));
        // Move to a backup.
        el.advance();
        assert!(!el.is_primary());
        // Before the interval elapses (t=3000, 2000ms < 5000): no failback.
        assert!(!el.maybe_failback(3000, 5000));
        assert!(!el.is_primary());
        // After the interval (t=6001, 5001ms >= 5000): fail back to primary.
        assert!(el.maybe_failback(6001, 5000));
        assert!(el.is_primary());
        assert_eq!(el.current(), "a:1");
    }

    #[test]
    fn endpoint_list_no_failback_while_on_primary() {
        let mut el = EndpointList::new(vec!["a:1".into(), "b:2".into()]);
        assert!(!el.maybe_failback(999_999, 5000)); // already primary → never fails back
        assert!(el.is_primary());
    }

    #[test]
    fn maybe_failback_holds_backup_when_clock_unstarted() {
        // Regression (v0.1.9 #2): a deliberate failover advances to a backup
        // BEFORE any maybe_failback has run on the primary, so last_failback_ms is
        // still 0. That must NOT be read as "infinitely long ago" → an instant
        // snap-back to a (possibly bad) primary. Instead it starts the clock and
        // holds the backup for a full interval.
        let mut el = EndpointList::new(vec!["a:1".into(), "b:2".into()]);
        el.advance(); // jump straight to the backup; the failback clock never started
        assert!(!el.is_primary());
        // First check on the backup with an unstarted clock: start it, stay put.
        assert!(!el.maybe_failback(1_000_000, 5000));
        assert!(
            !el.is_primary(),
            "must hold the backup, not instantly snap back to the primary"
        );
        // Before the interval elapses: still on the backup.
        assert!(!el.maybe_failback(1_004_000, 5000)); // 4000ms < 5000
        assert!(!el.is_primary());
        // After the interval: now it fails back to the primary, as designed.
        assert!(el.maybe_failback(1_006_000, 5000)); // 6000ms >= 5000
        assert!(el.is_primary());
    }
}
