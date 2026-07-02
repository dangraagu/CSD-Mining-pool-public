//! Optional TOML config file for csd-pool-miner.
//!
//! Lets a miner set defaults (payout address, the CPU dual-mining thread count,
//! backend, GPU geometry, ...) once instead of passing CLI flags every run.
//!
//! Precedence is enforced by the caller (`main::merge_config`):
//!   explicit CLI flag  >  this config file  >  built-in default.
//!
//! Every field is optional. A missing file, a missing field, or even a parse
//! error all degrade gracefully to "use the CLI defaults" — a bad config file
//! can never stop the miner from running. Unknown keys are ignored (so a future
//! key in an old binary, or vice-versa, is harmless); the README lists the exact
//! key names.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Deserialized config file. Field names match the CLI flags (snake_case).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    /// addr20 payout address (40 lowercase hex, optional `0x` prefix).
    pub address: Option<String>,
    /// Optional rig/worker name (display-only; appended to the address as the
    /// pool worker name "<address>.<worker>"). Sanitized by the binary to
    /// [A-Za-z0-9_-], max 24 chars.
    pub worker: Option<String>,
    /// "auto" | "cpu" | "opencl" | "cuda".
    pub backend: Option<String>,
    /// CPU-backend hashing threads (unset = all cores minus `reserve`).
    pub threads: Option<usize>,
    /// Cores to leave free when `threads` is unset.
    pub reserve: Option<usize>,
    /// GPU launch geometry: blocks per launch.
    pub blocks: Option<u32>,
    /// GPU launch geometry: threads per block.
    pub threads_per_block: Option<u32>,
    /// GPU kernel inner loop: nonces per thread per launch.
    pub nonces_per_thread: Option<u32>,
    /// Dual mining: CPU worker threads alongside a GPU backend (0 = GPU-only).
    pub cpu_threads: Option<usize>,
    /// Dual mining: fraction of the nonce range the CPU sweeps (0.0..=1.0).
    pub cpu_share: Option<f32>,
    /// GPU device index to mine on.
    pub device: Option<usize>,
}

impl FileConfig {
    /// Load the config file.
    ///
    /// * If `explicit` is `Some` (the user passed `--config PATH`), that exact
    ///   file is read; if it can't be read/parsed we warn and fall back to an
    ///   empty config (never fatal).
    /// * Otherwise the first existing of these is used:
    ///     `./config.toml`
    ///     `<platform config dir>/csd-pool-miner/config.toml`
    ///   and if none exist, an empty config is returned.
    ///
    /// Returns `(config, Some(path))` naming the file that took effect, or
    /// `(empty, None)` if none did, so the caller can log it.
    pub fn load(explicit: Option<&Path>) -> (FileConfig, Option<PathBuf>) {
        if let Some(p) = explicit {
            match std::fs::read_to_string(p) {
                Ok(s) => return (parse_or_warn(&s, p), Some(p.to_path_buf())),
                Err(e) => {
                    tracing::warn!(path = %p.display(), error = %e,
                        "--config file could not be read; using defaults");
                    return (FileConfig::default(), None);
                }
            }
        }
        for cand in default_paths() {
            if let Ok(s) = std::fs::read_to_string(&cand) {
                return (parse_or_warn(&s, &cand), Some(cand));
            }
        }
        (FileConfig::default(), None)
    }
}

/// Parse TOML, or warn and return an empty config on a syntax/type error.
fn parse_or_warn(s: &str, path: &Path) -> FileConfig {
    match toml::from_str::<FileConfig>(s) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e,
                "config file has a parse error; ignoring it (using defaults)");
            FileConfig::default()
        }
    }
}

/// Candidate config paths, in priority order.
fn default_paths() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("config.toml")];
    if let Some(dir) = platform_config_dir() {
        v.push(dir.join("csd-pool-miner").join("config.toml"));
    }
    v
}

/// Platform config dir, without depending on the `dirs` crate:
///   Windows -> `%APPDATA%`
///   else    -> `$XDG_CONFIG_HOME`, falling back to `$HOME/.config`.
fn platform_config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
            if !x.is_empty() {
                return Some(PathBuf::from(x));
            }
        }
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_config() {
        let s = r#"
            address = "192cf2af290bbcb05d7f389b4f83b56347de2cfa"
            worker = "rig1"
            backend = "cuda"
            cpu_threads = 0
            cpu_share = 0.25
            device = 1
        "#;
        let c: FileConfig = toml::from_str(s).unwrap();
        assert_eq!(c.address.as_deref(), Some("192cf2af290bbcb05d7f389b4f83b56347de2cfa"));
        assert_eq!(c.worker.as_deref(), Some("rig1"));
        assert_eq!(c.backend.as_deref(), Some("cuda"));
        assert_eq!(c.cpu_threads, Some(0));
        assert_eq!(c.cpu_share, Some(0.25));
        assert_eq!(c.device, Some(1));
        // Unset fields stay None.
        assert_eq!(c.blocks, None);
        assert_eq!(c.threads, None);
    }

    #[test]
    fn empty_config_is_all_none() {
        let c: FileConfig = toml::from_str("").unwrap();
        assert!(c.address.is_none() && c.cpu_threads.is_none() && c.backend.is_none());
        assert!(c.worker.is_none());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        // A typo'd / unknown key must not fail the parse.
        let c: FileConfig = toml::from_str("not_a_real_key = 5\ncpu_threads = 3").unwrap();
        assert_eq!(c.cpu_threads, Some(3));
    }
}
