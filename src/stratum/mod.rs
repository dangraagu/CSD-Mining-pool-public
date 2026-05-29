//! Stratum v1 pool-mining client.
//!
//! Layered so each piece is unit-testable in isolation:
//!   - [`protocol`]     — JSON-RPC wire types + (de)serialization (pure, TDD'd).
//!   - [`client`]       — the live [`client::StratumClient`]: TCP connect,
//!     subscribe/authorize handshake, and a background reader thread that
//!     tracks the latest pushed job + share difficulty.
//!   - [`mapping`]      — `mining.notify` → [`crate::csd_consensus::WorkTemplate`]
//!     (byte-locked against the bridge's `verify_submit`).
//!   - [`loop_stratum`] — the pooled mining loop [`run_stratum`] that drives a
//!     connected client: poll job → map → hash → submit shares.

pub mod client;
pub mod loop_stratum;
pub mod mapping;
pub mod protocol;

pub use client::{StratumClient, StratumJob};
pub use loop_stratum::run_stratum;
pub use protocol::{
    authorize_request, serialize_line, submit_request, subscribe_request, NotifyParams,
    Notification, Request, Response, SubscribeResult,
};
