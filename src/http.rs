//! Thin HTTP client for the node's `/work/get` + `/work/submit` + `/stats`
//! endpoints.
//!
//! We deliberately use the blocking `ureq` client here — the miner is a
//! tight CPU/GPU loop, not an async server, and the request rate is gentle.

use anyhow::Result;
use crate::csd_consensus::{Hash32, WorkSubmission, WorkTemplate};
use serde::Deserialize;

pub struct NodeClient {
    pub base: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StatsView {
    pub sync_status: String,
    pub height: u64,
    pub canonical_height: u64,
    pub canonical_tip: String,
    pub tip: String,
}

/// iter-53 #2 / iter-46 #D.8: response shape for the node's `/tip` endpoint.
/// Only `tip` (the 32-byte hash as lowercase hex) is consumed by the miner's
/// pre-submit staleness check; other fields (height, bits, time) are present
/// on the wire but ignored here so we stay forward-compatible.
#[derive(Debug, Clone, Deserialize)]
pub struct TipView {
    pub tip: String,
}

/// iter-32 (widened from iter-27): response shape for `/work/get`. The
/// server returns a fixed four-slot array; the miner iterates through
/// whichever slots are present via `iter_idx % templates.len()`.
///
/// Contract:
///   - templates[0] = variant A (extends local_tip) — always Some on 200.
///   - templates[1] = variant B (extends explorer.tip) — Some only when
///     local_tip != explorer.tip and that parent is locally indexed.
///   - templates[2] = variant C (extends explorer.tip's parent) — Some
///     only when that grandparent is locally indexed and != local_tip.
///   - templates[3] = variant D (extends our most-recently-orphaned tip)
///     — Some only within ORPHAN_FRESH (5 min) and indexed.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkResponse {
    pub templates: [Option<WorkTemplate>; 4],
}

/// iter-E1 (mine-through-503): typed outcome of `get_work_classified`. Lets
/// the work loop tell apart "the node served work" from "the node is gating
/// us with a 503 right now" without conflating the latter with a transport
/// error (which still surfaces as `Err` from `get_work_classified`).
#[derive(Debug)]
pub enum GetWork {
    /// HTTP 200: a fresh quad-template envelope.
    Work(WorkResponse),
    /// HTTP 503: the node's `/work/get` gate is holding (fork micro-
    /// divergence, fastsync, or a Layer-5 settlement hold). NOT an error —
    /// the loop should keep mining its last-good slot-A template if that
    /// template still extends the current /tip.
    ServiceUnavailable,
}

/// iter-E1: pure classifier — `true` iff this `ureq::Error` is an HTTP 503
/// (Service Unavailable) status response. Everything else (other HTTP
/// statuses, and all `Transport` variants — connection refused, DNS
/// failure, timeout, TLS) returns `false` so the caller treats it as a real
/// failure to back off on. Kept as a free function so it is unit-testable
/// without a live server.
pub fn is_service_unavailable(e: &ureq::Error) -> bool {
    matches!(e, ureq::Error::Status(503, _))
}

impl NodeClient {
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
        }
    }

    /// iter-32 (widened from iter-27): returns the quad-template envelope.
    /// Callers that don't care about speculation can pluck `templates[0]`.
    pub fn get_work(&self) -> Result<WorkResponse> {
        let url = format!("{}/work/get", self.base);
        let resp = ureq::get(&url).call()?;
        let r: WorkResponse = resp.into_json()?;
        Ok(r)
    }

    /// iter-E1 (mine-through-503): like `get_work` but distinguishes the
    /// node's "no work right now" 503 (HTTP Service Unavailable — the node's
    /// `/work/get` Layer-1/Layer-5 gate is holding) from a genuine transport
    /// failure or any other non-503 status.
    ///
    /// Mapping:
    ///   - HTTP 200 with a decodable body → `Ok(GetWork::Work(resp))`
    ///   - HTTP 503                        → `Ok(GetWork::ServiceUnavailable)`
    ///   - any other status / transport error / JSON decode error
    ///                                     → `Err(..)`
    ///
    /// The whole point: `ureq` collapses every HTTP 5xx into the same
    /// `Err(ureq::Error::Status(..))` variant, and once that flows through
    /// `?` into `anyhow::Error` the caller can no longer tell a 503 "keep
    /// mining the last-good template" hold from a real "the node is down,
    /// back off" transport error. We branch on the *typed* `ureq::Error`
    /// here, BEFORE it is erased into `anyhow`, so the work loop can react
    /// correctly. See `is_service_unavailable` for the pure classifier the
    /// unit tests exercise.
    pub fn get_work_classified(&self) -> Result<GetWork> {
        let url = format!("{}/work/get", self.base);
        match ureq::get(&url).call() {
            Ok(resp) => {
                let r: WorkResponse = resp.into_json()?;
                Ok(GetWork::Work(r))
            }
            Err(e) => {
                if is_service_unavailable(&e) {
                    Ok(GetWork::ServiceUnavailable)
                } else {
                    // Non-503 status (4xx, other 5xx) or a transport error
                    // (connection refused, DNS, timeout): a real failure
                    // the caller should back off on.
                    Err(e.into())
                }
            }
        }
    }

    pub fn submit_work(&self, sub: &WorkSubmission) -> Result<serde_json::Value> {
        Self::submit_work_to(&self.base, sub)
    }

    /// v75 port: stateless submit helper used by the multi-peer broadcast
    /// fanout. Accepts a raw base URL so a single call can target the local
    /// csd-node OR a peer's RPC endpoint without constructing a long-lived
    /// `NodeClient` per peer. Trailing slashes in `base` are tolerated.
    pub fn submit_work_to(base: &str, sub: &WorkSubmission) -> Result<serde_json::Value> {
        let trimmed = base.trim_end_matches('/');
        let url = format!("{}/work/submit", trimmed);
        let resp = ureq::post(&url).send_json(serde_json::to_value(sub)?)?;
        Ok(resp.into_json()?)
    }

    pub fn get_stats(&self) -> Result<StatsView> {
        let url = format!("{}/stats", self.base);
        let resp = ureq::get(&url).call()?;
        Ok(resp.into_json()?)
    }

    /// iter-53 #2 / iter-46 #D.8: fetch the node's current tip hash for the
    /// pre-submit staleness check. Returns the 32-byte tip on success; an
    /// error (network failure, malformed hex, missing tip) bubbles up so
    /// the caller can decide to be permissive (don't claim "stale" on a
    /// fetch failure — see `template_is_stale` in loop_.rs).
    pub fn get_tip(&self) -> Result<Hash32> {
        let url = format!("{}/tip", self.base);
        let resp = ureq::get(&url).call()?;
        let view: TipView = resp.into_json()?;
        let bytes = hex::decode(&view.tip)?;
        if bytes.len() != 32 {
            anyhow::bail!(
                "/tip returned hash of unexpected length: {} (expected 32)",
                bytes.len()
            );
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(out)
    }

    /// iter-51: post combined (GPU+CPU) hashrate to the node. Fire-and-
    /// forget from the miner loop; never block the loop on failure. The
    /// node validates `hashrate_ghs` is finite + non-negative and writes
    /// it into its `ApiState.our_hashrate_ghs` RwLock so /stats can
    /// surface it as `our_hashrate_ghs` + the derived
    /// `network_hashrate_excl_us_ghs`.
    ///
    /// Returns `()` on 200; bubbles up the error otherwise so callers
    /// can log at debug level and move on.
    pub fn post_miner_heartbeat(&self, hashrate_ghs: f64) -> Result<()> {
        let url = format!("{}/miner/heartbeat", self.base);
        ureq::post(&url).send_json(serde_json::json!({
            "hashrate_ghs": hashrate_ghs,
        }))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic `ureq::Error::Status` for the given HTTP status.
    /// ureq 2.x exposes `Response::new(status, status_text, body)`, so we can
    /// fabricate the error arm without a live server.
    fn status_err(code: u16, text: &str) -> ureq::Error {
        let resp = ureq::Response::new(code, text, "{}").expect("synthetic response");
        ureq::Error::Status(code, resp)
    }

    #[test]
    fn is_503_true_on_service_unavailable() {
        // The exact case mine-through-503 keys on: the node's /work/get gate
        // returns HTTP 503 → keep mining the last-good template, do NOT idle.
        assert!(is_service_unavailable(&status_err(503, "Service Unavailable")));
    }

    #[test]
    fn is_503_false_on_other_5xx() {
        // A 500/502/504 is a real server fault, not the cooperative "hold"
        // semantics of 503 → treat as a backoff-worthy error.
        assert!(!is_service_unavailable(&status_err(500, "Internal Server Error")));
        assert!(!is_service_unavailable(&status_err(502, "Bad Gateway")));
        assert!(!is_service_unavailable(&status_err(504, "Gateway Timeout")));
    }

    #[test]
    fn is_503_false_on_4xx() {
        // Client errors are never the "keep mining" hold.
        assert!(!is_service_unavailable(&status_err(404, "Not Found")));
        assert!(!is_service_unavailable(&status_err(400, "Bad Request")));
    }
}
