//! Abstraction over where the work loop gets jobs and submits solutions.
//!
//! Today the only implementor is [`crate::http::NodeClient`] (the HTTP
//! `/work/get` + `/work/submit` path). A Stratum-v1 pool client will be a
//! second implementor later, letting the same loop drive either intake.

use anyhow::Result;
use crate::csd_consensus::{Hash32, WorkSubmission, WorkTemplate};

/// Outcome of polling a work source for the current job(s).
pub enum WorkOutcome {
    /// Fresh template(s) to mine. Stratum yields a 1-element vec (latest
    /// pushed job); HTTP yields the quad-template envelope.
    Work(Vec<WorkTemplate>),
    /// Healthy but no new work right now — keep mining the last-good
    /// template (HTTP 503 hold; Stratum "no notify yet").
    Hold,
}

/// Abstracts where the loop gets work and where it submits solutions.
pub trait WorkSource: Send + Sync {
    fn poll_work(&self) -> Result<WorkOutcome>;
    fn submit(&self, sub: &WorkSubmission) -> Result<()>;
    /// Pre-submit staleness source. Stratum returns None (pool owns canonicity).
    fn tip(&self) -> Option<Hash32> { None }
    /// Optional hashrate heartbeat. Default no-op.
    fn report_hashrate(&self, _ghs: f64) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn node_client_is_a_work_source() {
        fn assert_ws<T: WorkSource>() {}
        assert_ws::<crate::http::NodeClient>();
    }
}
