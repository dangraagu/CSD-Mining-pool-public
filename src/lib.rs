//! csd-gpu-miner — standalone GPU miner for Compute Substrate v2.
//!
//! Architecture:
//!   work loop: poll `/work/get`, build a header skeleton, dispatch to a
//!              `MiningBackend`, submit winning solutions to `/work/submit`.
//!
//!   backends:
//!     - cpu     (default; reference + smoke-test correctness)
//!     - opencl  (feature = "opencl"; broad GPU coverage)
//!     - cuda    (feature = "cuda"; NVIDIA fast path)
//!
//! The hot work item per nonce is exactly:
//!     sha256d(80_byte_header) <= target ?
//!
//! Since the csd2 header is 80 bytes, the SHA-256 chunk boundary lines up
//! with the midstate, so a backend can precompute the first chunk once and
//! the GPU kernel only does one inner + one outer SHA-256 compress per
//! attempt.

#![allow(clippy::needless_range_loop)]

pub mod backend;
pub mod backends;
pub mod coinbase;
pub mod consensus_types;
pub mod http;
pub mod logging;
pub mod loop_;
pub mod selftest;
pub mod sha256d_cpu;
pub mod work_source;

/// Compatibility shim: the work-template/submission types used to live in a
/// sibling `csd-consensus` crate. They are now vendored into
/// [`consensus_types`]; this alias keeps the original `csd_consensus::Type`
/// import paths working unchanged across the codebase.
pub mod csd_consensus {
    pub use crate::consensus_types::*;
}

pub use backend::{MiningBackend, MiningResult};
