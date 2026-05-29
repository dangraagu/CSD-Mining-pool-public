//! Stratum v1 pool-mining client.
//!
//! Layered so the protocol is unit-testable without a socket:
//!   - [`protocol`] — JSON-RPC wire types + (de)serialization (pure, TDD'd).
//!   - [`client`]   — the live [`client::StratumClient`]: TCP connect,
//!     subscribe/authorize handshake, and a background reader thread that
//!     tracks the latest pushed job + share difficulty.
//!
//! Scope boundary: this layer is **protocol-only**. It does not translate a
//! `mining.notify` into a [`crate::csd_consensus::WorkTemplate`] (Task 3) and
//! does not implement [`crate::work_source::WorkSource`] (Task 4).

pub mod client;
pub mod protocol;

pub use client::{StratumClient, StratumJob};
pub use protocol::{
    authorize_request, serialize_line, submit_request, subscribe_request, NotifyParams,
    Notification, Request, Response, SubscribeResult,
};
