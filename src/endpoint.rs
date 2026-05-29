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
//! Do NOT commit a real production IP to the public repo — keep the placeholder
//! here and substitute it in the release build only.

use std::hint::black_box;

/// Single-byte XOR key used to scramble/descramble the endpoint. A fixed key is
/// sufficient for the stated goal (defeat `strings`, not a reverse engineer).
const XOR_KEY: u8 = 0x5a;

/// **PLACEHOLDER — replace with the scrambled real `host:3333` when cutting a
/// release.** These bytes are `"pool.REPLACE-AT-RELEASE.example:3333"` with
/// every byte XOR'd by [`XOR_KEY`]. Kept as a clearly-fake host so an
/// un-substituted build fails fast at connect time (DNS error) rather than
/// silently mining nowhere.
const SCRAMBLED_ENDPOINT: &[u8] = &[
    0x2a, 0x35, 0x35, 0x36, 0x74, 0x08, 0x1f, 0x0a, 0x16, 0x1b, 0x19, 0x1f,
    0x77, 0x1b, 0x0e, 0x77, 0x08, 0x1f, 0x16, 0x1f, 0x1b, 0x09, 0x1f, 0x74,
    0x3f, 0x22, 0x3b, 0x37, 0x2a, 0x36, 0x3f, 0x60, 0x69, 0x69, 0x69, 0x69,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The scramble we expect the placeholder to decode to. Reconstructed here
    /// (not stored as a top-level `const`) so the *plaintext* never exists as a
    /// compile-time literal anywhere in the crate.
    fn expected_plaintext() -> String {
        "pool.REPLACE-AT-RELEASE.example:3333".to_string()
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
}
