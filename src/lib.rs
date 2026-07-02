//! csd-pool-miner — standalone GPU/CPU miner for the Compute Substrate (CSD)
//! pool.
//!
//! It connects to the pool's Stratum v1 endpoint, maps `mining.notify` jobs
//! into csd1 84-byte block headers, dispatches hashing to a `MiningBackend`,
//! and submits shares. The pool endpoint is compiled in (see [`endpoint`]).
//!
//!   backends:
//!     - cpu     (default; reference + smoke-test correctness)
//!     - opencl  (feature = "opencl"; broad GPU coverage)
//!     - cuda    (feature = "cuda"; NVIDIA fast path)
//!
//! The hot work item per nonce is `sha256d(84_byte_header) <= target`. The
//! nonce sits in the second 64-byte SHA-256 chunk, so a backend can precompute
//! the first chunk's midstate once and only recompress the tail per attempt.

#![allow(clippy::needless_range_loop)]

pub mod backend;
pub mod backends;
pub mod bench;
pub mod coinbase;
pub mod consensus_types;
pub mod endpoint;
pub mod gpu_watchdog;
pub mod hiveos;
pub mod http;
pub mod logging;
pub mod mining_config;
pub mod notify;
pub mod nvml;
pub mod selftest;
pub mod selfupdate;
pub mod sha256d_cpu;
pub mod stats;
pub mod stats_server;
pub mod stratum;
pub mod thermal;
pub mod winsvc;

/// Compatibility shim: re-exports the vendored [`consensus_types`] under the
/// `csd_consensus` path, so `csd_consensus::Type` import paths keep working
/// unchanged across the codebase.
pub mod csd_consensus {
    pub use crate::consensus_types::*;
}

pub use backend::{DeviceError, MiningBackend, MiningResult};
