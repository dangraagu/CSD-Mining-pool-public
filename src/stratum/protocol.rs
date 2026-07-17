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

/// Normalize a raw GPU device name (as reported by NVML/CUDA/OpenCL, e.g.
/// `"NVIDIA GeForce RTX 5070 Ti"`) into the compact model string embedded in the
/// `mining.subscribe` user-agent (e.g. `"RTX 5070 Ti"`), so the pool can harvest
/// per-card hashrate.
///
/// PURE (no I/O, no GPU) → unit-tested with plain strings. Steps, in order:
///   1. Strip ONE leading vendor prefix, case-insensitively, most-specific
///      first (`"NVIDIA GeForce "` before `"NVIDIA "`, etc.).
///   2. Keep only printable ASCII (`0x20..=0x7E`); drop everything else.
///   3. Collapse internal whitespace runs to a single space and trim the ends.
///   4. Cap the RESULT at 28 chars (so the whole UA stays well under the pool's
///      64-char truncation).
///
/// An empty input — or one that reduces to nothing — yields `""`, which the UA
/// builder treats as "no model" (no dangling `"()"`).
pub fn normalize_gpu_model(raw: &str) -> String {
    // Most-specific prefix first so "NVIDIA GeForce X" strips to "X", not
    // "GeForce X". Each is pure ASCII, so byte-length == char-length.
    const PREFIXES: [&str; 6] = [
        "NVIDIA GeForce ",
        "NVIDIA ",
        "AMD Radeon ",
        "AMD ",
        "Intel Arc ",
        "Intel ",
    ];

    let mut s = raw.trim();
    for p in PREFIXES {
        // `get(..len)` is None if len is out of bounds OR not a char boundary,
        // so this never panics on a multi-byte name.
        if let Some(head) = s.get(..p.len()) {
            if head.eq_ignore_ascii_case(p) {
                s = &s[p.len()..];
                break;
            }
        }
    }

    // Printable-ASCII filter + whitespace collapse in one pass. Any whitespace
    // char becomes at most one ' ' separator; non-printable / non-ASCII bytes
    // are dropped entirely.
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !out.is_empty() && !prev_space {
                out.push(' ');
                prev_space = true;
            }
            continue;
        }
        if ch.is_ascii_graphic() {
            out.push(ch);
            prev_space = false;
        }
        // else: non-printable / non-ASCII → dropped.
    }
    while out.ends_with(' ') {
        out.pop();
    }

    // Cap the result at 28 chars. Post-filter every char is single-byte ASCII,
    // so 28 is always a char boundary; trim any space the cut exposed.
    if out.len() > 28 {
        out.truncate(28);
        while out.ends_with(' ') {
            out.pop();
        }
    }
    out
}

/// Build a `mining.subscribe` request. We send a single user-agent string as
/// the lone positional param. The v0.2.0 bridge records it as the session's
/// miner version ("csd-gpu-miner/<version>", shown on the pool dashboard);
/// older bridges ignore the param entirely but still expect an array, so this
/// is wire-compatible in both directions.
///
/// `gpu_model` is the already-normalized active GPU model (see
/// [`normalize_gpu_model`]). When it is `Some` and non-empty the UA becomes
/// `"csd-gpu-miner/<version> (<model>)"` so the pool can attribute hashrate
/// per card; `None` (or an empty string, defensively) keeps the plain
/// `"csd-gpu-miner/<version>"` — never a dangling `"()"`. The pool truncates the
/// UA at 64 chars and a normalized model is capped at 28, so the whole string
/// stays well under that.
pub fn subscribe_request(id: u64, gpu_model: Option<&str>) -> Request {
    let ver = env!("CARGO_PKG_VERSION");
    let ua = match gpu_model {
        Some(m) if !m.is_empty() => format!("csd-gpu-miner/{ver} ({m})"),
        _ => format!("csd-gpu-miner/{ver}"),
    };
    Request {
        id: Some(id),
        method: "mining.subscribe".to_string(),
        params: serde_json::json!([ua]),
    }
}

/// Build a `mining.authorize` request: `["<addr20>", "x"]`. The password
/// field is the conventional placeholder `"x"` (the bridge only checks the
/// worker address).
pub fn authorize_request(id: u64, worker: &str) -> Request {
    Request {
        id: Some(id),
        method: "mining.authorize".to_string(),
        params: serde_json::json!([worker, "x"]),
    }
}

/// Build a `mining.suggest_difficulty` request: `[<difficulty: f64>]`.
///
/// Sent once post-handshake (and re-sent after each reconnect) to hint the pool
/// at a starting share difficulty derived from a local hashrate benchmark, so
/// vardiff starts near-correct instead of ramping from the diff-8 floor. `id` is
/// `null`: like `set_difficulty`/`notify`, this is a fire-and-forget hint the
/// bridge does not reply to, so there is no response id to match. The pool is
/// free to clamp, honour, or ignore it — vardiff still owns the final difficulty.
pub fn suggest_difficulty_request(d: f64) -> Request {
    Request {
        id: None,
        method: "mining.suggest_difficulty".to_string(),
        params: serde_json::json!([d]),
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
    fn suggest_difficulty_request_shape() {
        // id is null (a notification-style request the bridge does not ack),
        // method is mining.suggest_difficulty, params is a single-element f64
        // array carrying the suggested difficulty.
        let req = suggest_difficulty_request(1024.0);
        assert_eq!(req.id, None);
        assert_eq!(req.method, "mining.suggest_difficulty");
        let p = req.params.as_array().unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].as_f64().unwrap(), 1024.0);
    }

    #[test]
    fn suggest_difficulty_request_exact_wire_line() {
        // Pin the EXACT bytes that hit the socket: id null, method, single-f64
        // params, one trailing newline, no pretty-printing. serde_json renders an
        // integral f64 (16384.0) as "16384.0".
        let req = suggest_difficulty_request(16384.0);
        let line = serialize_line(&req).unwrap();
        assert_eq!(
            line,
            "{\"id\":null,\"method\":\"mining.suggest_difficulty\",\"params\":[16384.0]}\n"
        );
        // And it round-trips back to the same request.
        let back: Request = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(back.id, None);
        assert_eq!(back.method, "mining.suggest_difficulty");
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
    fn authorize_request_dotted_worker_exact_wire_line() {
        // A rig-suffixed username ("<addr40>.<rig>") must pass through to the
        // wire VERBATIM — no re-splitting, no mangling, password stays "x".
        // The payout identity is the part before the first '.' (the bridge
        // strips the suffix for the money path); this test pins that the miner
        // never alters the string it was handed.
        let addr = "0123456789abcdef0123456789abcdef01234567";
        let user = format!("{addr}.rig1");
        let req = authorize_request(2, &user);
        let line = serialize_line(&req).unwrap();
        assert_eq!(
            line,
            format!("{{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"{user}\",\"x\"]}}\n")
        );
        // The bare payout address is recoverable byte-identically by splitting
        // on the first '.' — the invariant the pool's PPLNS credit relies on.
        assert_eq!(user.split('.').next().unwrap(), addr);
    }

    #[test]
    fn subscribe_request_shape() {
        let req = subscribe_request(1, None);
        assert_eq!(req.method, "mining.subscribe");
        assert_eq!(req.id, Some(1));
        // A single string user-agent param in an array. The UA is
        // "csd-gpu-miner/<version>" — the bridge records it as the session's
        // miner version (older bridges ignore it entirely).
        assert_eq!(req.params.as_array().unwrap().len(), 1);
        assert!(req.params[0].as_str().unwrap().starts_with("csd-gpu-miner/"));
    }

    #[test]
    fn subscribe_request_exact_wire_line() {
        // Pin the EXACT bytes that hit the socket: with no GPU model the UA is
        // "csd-gpu-miner/<CARGO_PKG_VERSION>" as the lone positional param,
        // one trailing newline, no pretty-printing.
        let req = subscribe_request(1, None);
        let line = serialize_line(&req).unwrap();
        assert_eq!(
            line,
            format!(
                "{{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"csd-gpu-miner/{}\"]}}\n",
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[test]
    fn subscribe_request_with_gpu_model_appends_suffix() {
        // A non-empty normalized model becomes a " (<model>)" suffix on the UA so
        // the pool can harvest per-card hashrate from the subscribe user-agent.
        let req = subscribe_request(1, Some("RTX 5070 Ti"));
        let ua = req.params[0].as_str().unwrap();
        assert_eq!(
            ua,
            format!("csd-gpu-miner/{} (RTX 5070 Ti)", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn subscribe_request_empty_model_is_plain() {
        // Defense-in-depth: an empty model string must NOT produce a dangling
        // "()" — it is treated exactly like `None` (plain UA, no suffix).
        let plain = format!("csd-gpu-miner/{}", env!("CARGO_PKG_VERSION"));
        assert_eq!(
            subscribe_request(1, Some("")).params[0].as_str().unwrap(),
            plain
        );
        assert_eq!(
            subscribe_request(1, None).params[0].as_str().unwrap(),
            plain
        );
    }

    #[test]
    fn subscribe_ua_stays_well_under_64_chars() {
        // The pool truncates the subscribe UA at 64 chars; a 28-char model plus
        // the fixed "csd-gpu-miner/<ver> (...)" scaffold must stay comfortably
        // under that so the model is never clipped.
        let model = "X".repeat(28); // the max a normalized model can be
        let ua = subscribe_request(1, Some(&model)).params[0]
            .as_str()
            .unwrap()
            .to_string();
        assert!(ua.len() < 64, "UA too long ({}): {ua}", ua.len());
    }

    #[test]
    fn normalize_gpu_model_strips_vendor_prefix_and_normalizes() {
        // Vendor prefixes stripped (case-insensitively, most-specific first).
        assert_eq!(
            normalize_gpu_model("NVIDIA GeForce RTX 5070 Ti"),
            "RTX 5070 Ti"
        );
        assert_eq!(normalize_gpu_model("NVIDIA GeForce RTX 4090"), "RTX 4090");
        assert_eq!(normalize_gpu_model("AMD Radeon RX 7900 XTX"), "RX 7900 XTX");
        assert_eq!(normalize_gpu_model("Intel Arc A770"), "A770");
        // Bare "NVIDIA " (no "GeForce") and case-insensitivity.
        assert_eq!(
            normalize_gpu_model("nvidia A100-SXM4-80GB"),
            "A100-SXM4-80GB"
        );
        // Empty in → empty out.
        assert_eq!(normalize_gpu_model(""), "");
        // Internal whitespace runs collapse to a single space; ends trimmed.
        assert_eq!(
            normalize_gpu_model("  NVIDIA GeForce   RTX  4090  "),
            "RTX 4090"
        );
        // Non-printable / non-ASCII bytes are dropped (keep only 0x20..=0x7E).
        assert_eq!(
            normalize_gpu_model("NVIDIA GeForce RTX\t4090\u{0}"),
            "RTX 4090"
        );
        // A 60-char junk name is capped to <= 28 chars.
        let junk = "Q".repeat(60);
        let out = normalize_gpu_model(&junk);
        assert!(out.len() <= 28, "not capped: len={} {out}", out.len());
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
