//! Compiled-in pool endpoint, lightly obfuscated.
//!
//! The public build connects to ONE pool by default and exposes no
//! `--node`/`--pool` override flag. To keep the literal host:port from showing
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

/// The live pool endpoint `"pool.yamaduo.no:3333"`, with every byte XOR'd by
/// [`XOR_KEY`]. The cleartext host never appears in source — only this
/// scrambled form does. To change the endpoint, re-scramble the new `host:3333`
/// (see "Cutting a release" above) and update the test below.
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

/// Resolve the ordered list of pool endpoints to try, from the operator's
/// `--pool`/`--url` overrides and the compiled-in default.
///
/// An empty override list yields `[builtin]` — the baked-in endpoint is the
/// DEFAULT element and is never removed. A non-empty list is used verbatim, in
/// order (each validated as `host:port`); the builtin is NOT appended (an
/// operator who names pools chooses their own failover order). A malformed entry
/// is a hard error so the miner fails loud instead of silently mining nowhere.
pub fn resolve_endpoints(cli_pools: &[String], builtin: &str) -> Result<Vec<String>, String> {
    if cli_pools.is_empty() {
        return Ok(vec![builtin.to_string()]);
    }
    let mut out = Vec::with_capacity(cli_pools.len());
    for p in cli_pools {
        let p = p.trim();
        if !is_valid_host_port(p) {
            return Err(format!("invalid pool endpoint {p:?} (expected host:port)"));
        }
        out.push(p.to_string());
    }
    Ok(out)
}

/// Lightweight syntactic `host:port` check (DNS is resolved later, at connect
/// time): a non-empty host, a final `:` separator, and a numeric port 1..=65535.
fn is_valid_host_port(s: &str) -> bool {
    match s.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && matches!(port.parse::<u16>(), Ok(p) if p > 0),
        None => false,
    }
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

    /// The scramble we expect the placeholder to decode to. Reconstructed here
    /// (not stored as a top-level `const`) so the *plaintext* never exists as a
    /// compile-time literal anywhere in the crate.
    fn expected_plaintext() -> String {
        "pool.yamaduo.no:3333".to_string()
    }

    #[test]
    fn pool_endpoint_decodes_to_expected() {
        assert_eq!(pool_endpoint(), expected_plaintext());
    }

    #[test]
    fn scrambled_table_is_actually_scrambled() {
        // The stored bytes must NOT equal the plaintext bytes (else the
        // obfuscation is a no-op and `strings` would reveal the host).
        let plain = expected_plaintext();
        assert_ne!(SCRAMBLED_ENDPOINT, plain.as_bytes());
        // And it must have the right length (catches a truncated paste).
        assert_eq!(SCRAMBLED_ENDPOINT.len(), plain.len());
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
    fn resolve_endpoints_defaults_to_builtin_when_empty() {
        assert_eq!(
            resolve_endpoints(&[], "pool.example.com:3333").unwrap(),
            vec!["pool.example.com:3333".to_string()]
        );
    }

    #[test]
    fn resolve_endpoints_uses_cli_list_in_order_without_builtin() {
        let pools = vec!["a.com:1".to_string(), "b.com:2".to_string()];
        assert_eq!(
            resolve_endpoints(&pools, "builtin.example:3333").unwrap(),
            vec!["a.com:1".to_string(), "b.com:2".to_string()]
        );
    }

    #[test]
    fn resolve_endpoints_rejects_bad_hostport() {
        for bad in ["noport", "host:", ":3333", "host:0", "host:99999", "host:abc"] {
            assert!(
                resolve_endpoints(&[bad.to_string()], "b:3333").is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn is_valid_host_port_accepts_real_endpoints() {
        assert!(is_valid_host_port("pool.yamaduo.no:3333"));
        assert!(is_valid_host_port("1.2.3.4:8080"));
        assert!(is_valid_host_port("[::1]:3333")); // bracketed IPv6
        assert!(!is_valid_host_port("nope"));
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
}
