//! Stratum v1 JSON-RPC wire types and (de)serialization helpers.
//!
//! This module is **protocol-only**: it knows how to parse the line-delimited
//! JSON-RPC frames the pool's bridge speaks, and how to build the requests we
//! send back. It does NOT open sockets (that's [`super::client`]) and it does
//! NOT translate a `mining.notify` into a [`crate::csd_consensus::WorkTemplate`]
//! (that mapping is Task 3, deliberately kept out of here).
//!
//! Wire format (must match the bridge exactly):
//!   - One JSON object per line, `\n`-terminated.
//!   - `mining.subscribe` result: `[ <ignored>, extranonce1_hex, extranonce2_size ]`.
//!   - `mining.authorize` result: `true` (or `false` + error).
//!   - `mining.set_difficulty` params: `[ <difficulty: f64> ]`.
//!   - `mining.notify` params (9-tuple): `[ job_id, prev_hash_be_hex,
//!     coinb1_hex, coinb2_hex, merkle_branches_hex[], version_hex, nbits_hex,
//!     ntime_hex, clean_jobs ]`.
//!   - `mining.submit` params (5-tuple): `[ worker_name, job_id,
//!     extranonce2_hex(4 bytes), ntime_hex, nonce_hex ]`.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A JSON-RPC request we send to (or, for `mining.notify`/`set_difficulty`,
/// receive from) the bridge. `id` is `null` for server-pushed notifications,
/// hence `Option<u64>`. `params` is left as a raw `Value` because its shape is
/// method-dependent (a positional array whose element types vary per method).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: Option<u64>,
    pub method: String,
    pub params: Value,
}

/// A JSON-RPC response to one of our requests. `result` may be any JSON value
/// (a bool for authorize/submit, an array for subscribe). `error` is the
/// Stratum `[code, message, data]` triple when the call failed, else absent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: Option<u64>,
    /// Defaults to `Value::Null` if the server omits `result` on an error frame.
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub error: Option<Value>,
}

/// A server-pushed notification (`id` is `null`/absent). We only care about the
/// `method` and `params`; the `id` field on the wire is ignored on purpose so
/// deserialization tolerates `"id":null` framing without carrying it around.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

/// Parsed `mining.notify` 9-tuple. All hex fields are kept as the raw hex
/// strings exactly as received — decoding/assembly into a header is Task 3.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotifyParams {
    pub job_id: String,
    pub prev_hash_be_hex: String,
    pub coinb1_hex: String,
    pub coinb2_hex: String,
    pub merkle_branches_hex: Vec<String>,
    pub version_hex: String,
    pub nbits_hex: String,
    pub ntime_hex: String,
    pub clean_jobs: bool,
}

impl NotifyParams {
    /// Parse the positional `params` array of a `mining.notify` notification.
    /// Length-checked: the bridge always sends exactly 9 elements.
    pub fn parse(params: &Value) -> Result<NotifyParams> {
        let arr = params
            .as_array()
            .ok_or_else(|| anyhow!("mining.notify params is not a JSON array"))?;
        if arr.len() != 9 {
            return Err(anyhow!(
                "mining.notify expects 9 params, got {}",
                arr.len()
            ));
        }

        let job_id = str_at(arr, 0, "job_id")?;
        let prev_hash_be_hex = str_at(arr, 1, "prev_hash_be_hex")?;
        let coinb1_hex = str_at(arr, 2, "coinb1_hex")?;
        let coinb2_hex = str_at(arr, 3, "coinb2_hex")?;

        let merkle_branches_hex = arr[4]
            .as_array()
            .ok_or_else(|| anyhow!("mining.notify merkle_branches (index 4) is not an array"))?
            .iter()
            .enumerate()
            .map(|(i, v)| {
                v.as_str()
                    .map(|s| s.to_string())
                    .ok_or_else(|| anyhow!("merkle branch[{i}] is not a string"))
            })
            .collect::<Result<Vec<String>>>()?;

        let version_hex = str_at(arr, 5, "version_hex")?;
        let nbits_hex = str_at(arr, 6, "nbits_hex")?;
        let ntime_hex = str_at(arr, 7, "ntime_hex")?;
        let clean_jobs = arr[8]
            .as_bool()
            .ok_or_else(|| anyhow!("mining.notify clean_jobs (index 8) is not a bool"))?;

        Ok(NotifyParams {
            job_id,
            prev_hash_be_hex,
            coinb1_hex,
            coinb2_hex,
            merkle_branches_hex,
            version_hex,
            nbits_hex,
            ntime_hex,
            clean_jobs,
        })
    }
}

/// Parsed `mining.subscribe` result: `[ <ignored>, extranonce1_hex,
/// extranonce2_size ]`. We read index 1 (the session extranonce1) and index 2
/// (the extranonce2 byte width, which the bridge sets to 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscribeResult {
    pub extranonce1_hex: String,
    pub extranonce2_size: usize,
}

impl SubscribeResult {
    pub fn parse(result: &Value) -> Result<SubscribeResult> {
        let arr = result
            .as_array()
            .ok_or_else(|| anyhow!("mining.subscribe result is not a JSON array"))?;
        if arr.len() < 3 {
            return Err(anyhow!(
                "mining.subscribe result expects >=3 elements, got {}",
                arr.len()
            ));
        }
        let extranonce1_hex = arr[1]
            .as_str()
            .ok_or_else(|| anyhow!("subscribe result extranonce1 (index 1) is not a string"))?
            .to_string();
        let extranonce2_size = arr[2]
            .as_u64()
            .ok_or_else(|| anyhow!("subscribe result extranonce2_size (index 2) is not a u64"))?
            as usize;
        Ok(SubscribeResult {
            extranonce1_hex,
            extranonce2_size,
        })
    }
}

/// Build a `mining.subscribe` request. We send a single user-agent string as
/// the lone positional param; the bridge ignores it but expects an array.
pub fn subscribe_request(id: u64) -> Request {
    Request {
        id: Some(id),
        method: "mining.subscribe".to_string(),
        params: serde_json::json!([format!("csd-pool-miner/{}", env!("CARGO_PKG_VERSION"))]),
    }
}

/// Build a `mining.authorize` request: `["<csd1 address>", "x"]`. The password
/// field is the conventional placeholder `"x"` (the bridge only checks the
/// worker address).
pub fn authorize_request(id: u64, worker: &str) -> Request {
    Request {
        id: Some(id),
        method: "mining.authorize".to_string(),
        params: serde_json::json!([worker, "x"]),
    }
}

/// Build a `mining.submit` request carrying the 5-tuple
/// `[worker, job_id, extranonce2_hex, ntime_hex, nonce_hex]`.
pub fn submit_request(
    id: u64,
    worker: &str,
    job_id: &str,
    xn2_hex: &str,
    ntime_hex: &str,
    nonce_hex: &str,
) -> Request {
    Request {
        id: Some(id),
        method: "mining.submit".to_string(),
        params: serde_json::json!([worker, job_id, xn2_hex, ntime_hex, nonce_hex]),
    }
}

/// Serialize any JSON-RPC value to a single newline-terminated line, ready to
/// write straight to the socket. Every Stratum frame is one line.
pub fn serialize_line<T: Serialize>(value: &T) -> Result<String> {
    let mut s = serde_json::to_string(value).context("serializing stratum frame")?;
    s.push('\n');
    Ok(s)
}

/// Internal helper: read positional `arr[idx]` as a `String`, with a field name
/// in the error for diagnosability.
fn str_at(arr: &[Value], idx: usize, field: &str) -> Result<String> {
    arr[idx]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("mining.notify {field} (index {idx}) is not a string"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_notify_9_tuple() {
        let line = r#"{"id":null,"method":"mining.notify","params":["job1","00ff","aa","bb",["cc"],"01000000","1d00ffff","60c0babe",true]}"#;
        let req: Notification = serde_json::from_str(line).unwrap();
        let n = NotifyParams::parse(&req.params).unwrap();
        assert_eq!(n.job_id, "job1");
        assert_eq!(n.coinb1_hex, "aa");
        assert_eq!(n.merkle_branches_hex, vec!["cc".to_string()]);
        assert_eq!(n.ntime_hex, "60c0babe");
        assert_eq!(n.clean_jobs, true);
    }

    #[test]
    fn parse_notify_full_field_check() {
        // Exercise every field, including a multi-element merkle branch and an
        // empty branch boundary, plus clean_jobs=false.
        let line = r#"{"id":null,"method":"mining.notify","params":["j2","prevhash","c1","c2",["br0","br1","br2"],"20000000","1a0abbcd","deadbeef",false]}"#;
        let req: Notification = serde_json::from_str(line).unwrap();
        let n = NotifyParams::parse(&req.params).unwrap();
        assert_eq!(n.job_id, "j2");
        assert_eq!(n.prev_hash_be_hex, "prevhash");
        assert_eq!(n.coinb1_hex, "c1");
        assert_eq!(n.coinb2_hex, "c2");
        assert_eq!(
            n.merkle_branches_hex,
            vec!["br0".to_string(), "br1".to_string(), "br2".to_string()]
        );
        assert_eq!(n.version_hex, "20000000");
        assert_eq!(n.nbits_hex, "1a0abbcd");
        assert_eq!(n.ntime_hex, "deadbeef");
        assert_eq!(n.clean_jobs, false);
    }

    #[test]
    fn parse_notify_empty_merkle_branch() {
        let line = r#"{"id":null,"method":"mining.notify","params":["j","p","a","b",[],"01000000","1d00ffff","60c0babe",true]}"#;
        let req: Notification = serde_json::from_str(line).unwrap();
        let n = NotifyParams::parse(&req.params).unwrap();
        assert!(n.merkle_branches_hex.is_empty());
    }

    #[test]
    fn parse_notify_rejects_wrong_arity() {
        // 8 elements (missing clean_jobs) must be a hard error, not a silent
        // truncation — getting this wrong would mis-map the whole job.
        let bad = serde_json::json!(["j", "p", "a", "b", ["cc"], "v", "nb", "nt"]);
        assert!(NotifyParams::parse(&bad).is_err());
    }

    #[test]
    fn parse_notify_rejects_non_array() {
        let bad = serde_json::json!({"not": "an array"});
        assert!(NotifyParams::parse(&bad).is_err());
    }

    #[test]
    fn parse_subscribe_result() {
        // Canonical bridge subscribe result: [ <ignored>, xn1, xn2_size ].
        // The bridge sets extranonce2_size = 4.
        let line = r#"{"id":1,"result":[[["mining.set_difficulty","1"],["mining.notify","1"]],"a1b2c3d4",4],"error":null}"#;
        let resp: Response = serde_json::from_str(line).unwrap();
        let s = SubscribeResult::parse(&resp.result).unwrap();
        assert_eq!(s.extranonce1_hex, "a1b2c3d4");
        assert_eq!(s.extranonce2_size, 4);
    }

    #[test]
    fn parse_subscribe_result_simple_shape() {
        // Some bridges send a null/string in slot 0 rather than a nested array;
        // we ignore slot 0 entirely, so this must still parse.
        let result = serde_json::json!([serde_json::Value::Null, "deadbeef", 4]);
        let s = SubscribeResult::parse(&result).unwrap();
        assert_eq!(s.extranonce1_hex, "deadbeef");
        assert_eq!(s.extranonce2_size, 4);
    }

    #[test]
    fn parse_subscribe_result_rejects_short() {
        let bad = serde_json::json!(["only", "two"]);
        assert!(SubscribeResult::parse(&bad).is_err());
    }

    #[test]
    fn submit_request_shape() {
        let req = submit_request(7, "csd1worker", "job1", "00000001", "60c0babe", "deadbeef");
        assert_eq!(req.id, Some(7));
        assert_eq!(req.method, "mining.submit");
        let p = req.params.as_array().unwrap();
        assert_eq!(p.len(), 5);
        assert_eq!(p[0], "csd1worker");
        assert_eq!(p[1], "job1");
        assert_eq!(p[2], "00000001");
        assert_eq!(p[3], "60c0babe");
        assert_eq!(p[4], "deadbeef");
    }

    #[test]
    fn submit_request_round_trips_through_json() {
        // The exact bytes that hit the wire must match the 5-tuple contract.
        let req = submit_request(42, "w", "j", "00000002", "11223344", "aabbccdd");
        let line = serialize_line(&req).unwrap();
        assert!(line.ends_with('\n'));
        let back: Request = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(back.method, "mining.submit");
        assert_eq!(back.id, Some(42));
        assert_eq!(back.params, req.params);
    }

    #[test]
    fn authorize_request_shape() {
        let req = authorize_request(2, "csd1addr");
        assert_eq!(req.method, "mining.authorize");
        let p = req.params.as_array().unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0], "csd1addr");
        assert_eq!(p[1], "x");
    }

    #[test]
    fn subscribe_request_shape() {
        let req = subscribe_request(1);
        assert_eq!(req.method, "mining.subscribe");
        assert_eq!(req.id, Some(1));
        // A single string user-agent param in an array.
        assert_eq!(req.params.as_array().unwrap().len(), 1);
        assert!(req.params[0].as_str().unwrap().starts_with("csd-pool-miner/"));
    }

    #[test]
    fn serialize_line_appends_newline_and_is_parseable() {
        let req = authorize_request(1, "addr");
        let line = serialize_line(&req).unwrap();
        assert!(line.ends_with('\n'));
        // Exactly one newline (no internal pretty-printing).
        assert_eq!(line.matches('\n').count(), 1);
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["method"], "mining.authorize");
    }

    #[test]
    fn response_tolerates_missing_result_on_error_frame() {
        // An authorize failure: result omitted, error = [24, msg, null].
        let line = r#"{"id":2,"error":[24,"Invalid worker address",null]}"#;
        let resp: Response = serde_json::from_str(line).unwrap();
        assert_eq!(resp.id, Some(2));
        assert!(resp.result.is_null());
        assert!(resp.error.is_some());
    }

    #[test]
    fn notification_tolerates_id_null() {
        // set_difficulty arrives with id:null and a single-element f64 array.
        let line = r#"{"id":null,"method":"mining.set_difficulty","params":[1024.0]}"#;
        let note: Notification = serde_json::from_str(line).unwrap();
        assert_eq!(note.method, "mining.set_difficulty");
        assert_eq!(note.params[0].as_f64().unwrap(), 1024.0);
    }
}
