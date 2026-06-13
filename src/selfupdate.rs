//! Self-update decision + integrity logic (P4).
//!
//! The updater shell scripts (`mine-auto.{sh,bat}`) must NOT make the
//! update/no-update call with a raw string compare (`"0.1.10" != "0.1.9"` reads
//! the wrong way), nor `chmod+exec` a download without verifying it. So those
//! two dangerous decisions live here as **pure, unit-tested Rust** that the
//! scripts call via subcommands (`check-update`, `verify-file`); the shell stays
//! thin.

/// Parse a `vMAJOR.MINOR.PATCH` (or unprefixed) version into `(major, minor,
/// patch)`. Returns `None` for anything that isn't exactly three numeric
/// dot-separated components (so a garbage tag can't be mis-ordered).
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim();
    let v = v.strip_prefix('v').unwrap_or(v);
    let mut it = v.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None; // more than three components → not a plain semver
    }
    Some((major, minor, patch))
}

/// Should the miner update from `current` to `latest`?
///
/// True iff `latest` is a valid semver **strictly greater** than `current`
/// (numeric per-component compare — so `0.1.10 > 0.1.9`, which a string compare
/// gets wrong). Guards:
///   - an **unparseable `latest`** ⇒ `false` (never chase a garbage release),
///   - an unparseable `current` but good `latest` ⇒ `true` (a corrupt local
///     version string shouldn't pin us to a stale binary forever).
pub fn should_update(current: &str, latest: &str) -> bool {
    match (parse_version(current), parse_version(latest)) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(c), Some(l)) => l > c,
    }
}

/// Verify that `bytes` hash to `expected_hex` (a SHA-256 hex digest,
/// case-insensitive, surrounding whitespace ignored). Called before a freshly
/// downloaded binary is swapped in + executed, so a corrupted or tampered
/// download is never run.
pub fn verify_sha256(bytes: &[u8], expected_hex: &str) -> bool {
    use sha2::{Digest, Sha256};
    let got = hex::encode(Sha256::digest(bytes));
    got.eq_ignore_ascii_case(expected_hex.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_accepts_semver_and_v_prefix() {
        assert_eq!(parse_version("v0.1.8"), Some((0, 1, 8)));
        assert_eq!(parse_version("0.1.10"), Some((0, 1, 10)));
        assert_eq!(parse_version("  1.2.3 "), Some((1, 2, 3)));
        assert_eq!(parse_version("0.1"), None); // too few
        assert_eq!(parse_version("0.1.8.1"), None); // too many
        assert_eq!(parse_version("x.y.z"), None);
        assert_eq!(parse_version("v"), None);
    }

    #[test]
    fn should_update_uses_numeric_semver_not_string_compare() {
        // The bug this fixes: a string compare reads "0.1.10" as NOT newer than
        // "0.1.9" (because "1" < "9" lexically). Numeric compare gets it right.
        assert!(should_update("0.1.9", "0.1.10"));
        assert!(!should_update("0.1.10", "0.1.9"));
        // Equal / older / newer-minor / newer-major.
        assert!(!should_update("0.1.8", "0.1.8"));
        assert!(!should_update("0.1.8", "0.1.7"));
        assert!(should_update("0.1.8", "0.2.0"));
        assert!(should_update("0.9.9", "1.0.0"));
        // Guards.
        assert!(!should_update("0.1.8", "not-a-version")); // garbage remote → never
        assert!(should_update("garbage", "0.1.8")); // broken local → take the good remote
    }

    #[test]
    fn verify_sha256_matches_only_the_correct_digest() {
        // sha256("hello world") = b94d27b9934d3e08a52e52d7da7dabfac484efe3...
        let bytes = b"hello world";
        let correct = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert!(verify_sha256(bytes, correct));
        assert!(verify_sha256(bytes, &correct.to_uppercase())); // case-insensitive
        assert!(verify_sha256(bytes, &format!("  {correct}  "))); // trims
        assert!(!verify_sha256(bytes, "deadbeef"));
        assert!(!verify_sha256(b"hello world!", correct)); // one byte off
    }
}
