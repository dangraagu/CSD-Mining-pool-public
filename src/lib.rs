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
pub mod coinbase;
pub mod consensus_types;
pub mod endpoint;
pub mod logging;
pub mod mining_config;
pub mod selftest;
pub mod sha256d_cpu;
pub mod stratum;

/// Compatibility shim: the work-template/submission types used to live in a
/// sibling `csd-consensus` crate. They are now vendored into
/// [`consensus_types`]; this alias keeps the original `csd_consensus::Type`
/// import paths working unchanged across the codebase.
pub mod csd_consensus {
    pub use crate::consensus_types::*;
}

pub use backend::{MiningBackend, MiningResult};
