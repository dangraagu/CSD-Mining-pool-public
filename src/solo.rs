//! Solo-mining cores (P3) — **pure**.
//!
//! Two transport-agnostic pieces a `NodeWorkSource` needs to mine directly to a
//! csd-node (no pool/bridge):
//!   - [`parse_node_template`] — turn a `GET /work/get` body into the vendored
//!     [`WorkTemplate`] (tolerating both the flat compute-substrate node shape
//!     and the multi-slot envelope build).
//!   - [`solution_to_submission`] — turn a found [`Solution`] into the node's
//!     [`WorkSubmission`] (`{id,nonce,extranonce,time}`) for `POST /work/submit`.
//!
//! The HTTP poller + the `WorkSource` impl that drive these are wired separately
//! (they need a socket); these functions are pure and unit-tested.

use serde::Deserialize;

use crate::consensus_types::{WorkSubmission, WorkTemplate};

/// The multi-slot node build wraps templates in `{ "templates": [T?, …] }`; the
/// flat compute-substrate build returns a bare `WorkTemplate`. We only need to
/// *deserialize* this fallback shape.
#[derive(Deserialize)]
struct WorkEnvelope {
    templates: Vec<Option<WorkTemplate>>,
}

/// Parse a csd-node `GET /work/get` response body into a [`WorkTemplate`].
///
/// Tries the **flat** shape first (the compute-substrate node returns a bare
/// `WorkTemplate` whose fields are identical to the vendored type), then falls
/// back to the **envelope** shape (`{"templates":[T?,…]}`, the multi-slot build)
/// and takes the first populated slot. A non-JSON / 503 / empty-slot body is an
/// `Err` (the caller idles rather than mining stale/garbage work).
pub fn parse_node_template(body: &str) -> Result<WorkTemplate, String> {
    // Flat shape (the common case). serde ignores unknown fields, so a richer
    // node response still parses as long as the required fields are present.
    if let Ok(t) = serde_json::from_str::<WorkTemplate>(body) {
        return Ok(t);
    }
    // Envelope fallback (multi-slot node build).
    match serde_json::from_str::<WorkEnvelope>(body) {
        Ok(env) => env
            .templates
            .into_iter()
            .flatten()
            .next()
            .ok_or_else(|| "node /work/get envelope had no populated slot".to_string()),
        Err(e) => Err(format!("unrecognized /work/get response: {e}")),
    }
}

/// A solved work unit, ready to submit to the node. `job_id` is for local
/// logging/staleness only and is NOT sent on the wire.
#[derive(Clone, Debug)]
pub struct Solution {
    pub template_id: u64,
    pub nonce: u32,
    pub extranonce: u64,
    pub time: u64,
    pub job_id: String,
}

/// Map a [`Solution`] to the node's [`WorkSubmission`] — the exact
/// `{id,nonce,extranonce,time}` payload `POST /work/submit` expects.
pub fn solution_to_submission(sol: &Solution) -> WorkSubmission {
    WorkSubmission {
        id: sol.template_id,
        nonce: sol.nonce,
        extranonce: sol.extranonce,
        time: sol.time,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative template: low (network-style) target, raw prev, 8-byte
    /// extranonce, full nonce range — the shape `/work/get` returns.
    fn sample_template() -> WorkTemplate {
        let mut target = [0u8; 32];
        target[3] = 0x0f;
        target[4] = 0xff; // leading zeros then a small number = a real target
        WorkTemplate {
            id: 7,
            version: 1,
            prev: [0x11u8; 32],
            time: 1_700_000_123,
            bits: 0x1e00ffff,
            target,
            coinbase_prefix: vec![0xde, 0xad],
            extranonce_size: 8,
            coinbase_suffix: vec![0xbe, 0xef],
            merkle_branch: vec![],
            nonce_start: 0,
            nonce_end: u32::MAX,
            height: 12345,
        }
    }

    #[test]
    fn parse_node_template_flat_round_trips() {
        let t = sample_template();
        let body = serde_json::to_string(&t).unwrap();
        let parsed = parse_node_template(&body).expect("flat parse");
        // WorkTemplate has no PartialEq (it's a vendored wire type) — compare via
        // its JSON value, which IS comparable and covers every field.
        assert_eq!(
            serde_json::to_value(&t).unwrap(),
            serde_json::to_value(&parsed).unwrap()
        );
    }

    #[test]
    fn parse_node_template_accepts_envelope_slot_a() {
        let t = sample_template();
        let env = serde_json::json!({
            "templates": [serde_json::to_value(&t).unwrap(), null, null, null]
        });
        let parsed = parse_node_template(&env.to_string()).expect("envelope parse");
        assert_eq!(
            serde_json::to_value(&t).unwrap(),
            serde_json::to_value(&parsed).unwrap()
        );
    }

    #[test]
    fn parse_node_template_rejects_empty_envelope_and_junk() {
        // All slots null → no work.
        let empty = serde_json::json!({ "templates": [null, null] });
        assert!(parse_node_template(&empty.to_string()).is_err());
        // Non-JSON and a 503-style text body → error (caller idles).
        assert!(parse_node_template("not json at all").is_err());
        assert!(parse_node_template("503 Service Unavailable").is_err());
        assert!(parse_node_template("").is_err());
    }

    #[test]
    fn solution_to_submission_has_node_worksubmission_shape() {
        let sol = Solution {
            template_id: 42,
            nonce: 0xDEAD_BEEF,
            extranonce: 0x0102_0304_0506_0708,
            time: 1_700_000_000,
            job_id: "job-1".to_string(),
        };
        let sub = solution_to_submission(&sol);
        let v = serde_json::to_value(&sub).unwrap();
        let obj = v.as_object().expect("submission is a JSON object");
        // EXACTLY {id,nonce,extranonce,time} — no block_hex, no job_id leak.
        assert_eq!(obj.len(), 4, "WorkSubmission must have 4 fields, got {obj:?}");
        assert_eq!(v["id"], 42);
        assert_eq!(v["nonce"], 0xDEAD_BEEFu32);
        assert_eq!(v["extranonce"], 0x0102_0304_0506_0708u64);
        assert_eq!(v["time"], 1_700_000_000u64);
        assert!(v.get("job_id").is_none());
        assert!(v.get("block_hex").is_none());
    }
}
