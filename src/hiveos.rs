//! HiveOS custom-miner glue (P4 §1).
//!
//! HiveOS polls a custom miner's `h-stats.sh` every ~10s for an h-stats JSON
//! object. Rather than re-derive the hashrate/share mapping in fragile shell
//! `jq`/`awk` (where the §G7 kH/s-clamp bug would silently reappear), the whole
//! transform lives here as **pure, unit-tested Rust** and the shell calls the
//! `hiveos-stats` subcommand. Two pieces:
//!   - [`hiveos_stats_from_summary`] — map the xmrig `/1/summary` JSON this miner
//!     already serves into the HiveOS h-stats shape.
//!   - [`parse_gpu_ids`] — parse a `--gpu-id 0,2,3` include-list (a launcher
//!     device filter; validated here so junk is rejected early).

/// Map an xmrig `/1/summary` JSON value (the shape [`crate::stats::summary_json`]
/// produces) into a HiveOS h-stats JSON object.
///
/// Output shape:
/// ```json
/// { "hs": [<khs>], "hs_units": "khs", "temp": [], "fan": [],
///   "uptime": <u64>, "ar": [<good>, <rejected>, 0, <stale>], "algo": "sha256d" }
/// ```
///
/// **The key transform:** `summary.hashrate.total[0]` is the 10s window in
/// **H/s**. HiveOS clamps absurd `hs[]` elements (anything over ~1e13 shows a
/// bogus "1000 GH"), so we convert to **kH/s** = `total[0] / 1000.0`, rounded to
/// an integer, and set `hs_units = "khs"`. Dividing by 1000 (NOT 1e6 / 1e9) is
/// the exact fix for the §G7 headline bug — a 5 GH/s card becomes 5_000_000 kH/s,
/// orders of magnitude under the clamp ceiling.
///
/// `ar` is `[shares_good, shares_rejected, invalid=0, shares_stale]` and
/// `uptime` comes from `summary.uptime`.
///
/// **GPU temperature (`temp[]`):** when the summary carries a csd
/// `health.gpu_temp_c` (the OPTIONAL `nvml` build with NVML up), it is rounded
/// to a whole degree and emitted as the single-element `temp[]` HiveOS shows per
/// GPU. When it is absent (the default/fleet build, a non-NVIDIA host, or an
/// unreadable sensor) `temp[]` stays the empty array exactly as before — zero
/// behaviour change vs the lean build.
///
/// **Defensive by construction:** every field is plucked with a `0` default, so
/// a missing/null/malformed summary (even an empty `{}`) yields a zero-but-valid
/// HiveOS object — the rig still reports "alive, zero" rather than breaking the
/// custom miner. Never panics.
pub fn hiveos_stats_from_summary(summary: &serde_json::Value) -> serde_json::Value {
    // hashrate.total[0] (10s window) in H/s → kH/s, rounded to an integer.
    // `.get(...).and_then(...)` chains so any missing layer collapses to 0.0.
    let hs_hz = summary
        .get("hashrate")
        .and_then(|h| h.get("total"))
        .and_then(|t| t.get(0))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let khs = (hs_hz / 1000.0).round() as i64;

    let results = summary.get("results");
    let pick = |key: &str| -> i64 {
        results
            .and_then(|r| r.get(key))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    };
    let good = pick("shares_good");
    let rejected = pick("shares_rejected");
    let stale = pick("shares_stale");

    let uptime = summary
        .get("uptime")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    // GPU temperature (°C) from the optional csd `health.gpu_temp_c` extension.
    // Present only on the `nvml` build with NVML up; absent ⇒ `temp[]` is empty
    // (the pre-nvml behaviour). Rounded to a whole degree (HiveOS shows integers).
    let temp = match summary
        .get("health")
        .and_then(|h| h.get("gpu_temp_c"))
        .and_then(|v| v.as_f64())
    {
        Some(t) => serde_json::json!([t.round() as i64]),
        None => serde_json::json!([]),
    };

    serde_json::json!({
        "hs": [khs],
        "hs_units": "khs",
        "temp": temp,
        "fan": [],
        "uptime": uptime,
        // xmrig-style ar: [accepted, rejected, invalid, stale]. We have no
        // separate "invalid" counter, so it is a constant 0.
        "ar": [good, rejected, 0, stale],
        "algo": "sha256d",
    })
}

/// Parse a `--gpu-id` include-list like `"0,2,3"` into `[0, 2, 3]`.
///
/// Each entry is trimmed; a non-numeric entry is a clear `Err`. An empty string
/// (and a whitespace-only string) is `Ok(vec![])` — "no filter". Order is
/// preserved as written.
pub fn parse_gpu_ids(s: &str) -> Result<Vec<usize>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(Vec::new());
    }
    s.split(',')
        .map(|part| {
            let p = part.trim();
            p.parse::<usize>()
                .map_err(|_| format!("invalid --gpu-id entry {p:?} (expected a non-negative integer)"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hiveos_stats_maps_summary_to_hstats_shape() {
        // A 3 GH/s 10s window = 3_000_000_000 H/s → 3_000_000 kH/s (÷1000).
        let summary = serde_json::json!({
            "worker_id": "csd1abc",
            "version": "0.1.8",
            "uptime": 3600,
            "hashrate": { "total": [3_000_000_000.0_f64, 2.9e9, 3.1e9], "highest": 3.1e9 },
            "results": {
                "shares_good": 42,
                "shares_total": 50,
                "shares_rejected": 3,
                "shares_stale": 5,
            },
            "connection": { "pool": "pool.test:3333" },
        });
        let h = hiveos_stats_from_summary(&summary);

        // The headline kH/s transform: H/s ÷ 1000 (NOT ÷1e6, NOT ÷1e9).
        assert_eq!(h["hs"], serde_json::json!([3_000_000]));
        assert_eq!(h["hs_units"], "khs");
        // ar = [good, rejected, invalid=0, stale].
        assert_eq!(h["ar"], serde_json::json!([42, 3, 0, 5]));
        assert_eq!(h["uptime"], 3600);
        assert_eq!(h["algo"], "sha256d");
        // Always-present empty arrays HiveOS expects.
        assert_eq!(h["temp"], serde_json::json!([]));
        assert_eq!(h["fan"], serde_json::json!([]));
    }

    #[test]
    fn hiveos_khs_divisor_is_1000_not_1e6_or_1e9() {
        // Guard the exact §G7 bug class: a 5 GH/s card MUST read 5_000_000 kH/s.
        // ÷1e6 would give 5000 (wrong), ÷1e9 would give 5 (wrong).
        let summary = serde_json::json!({
            "hashrate": { "total": [5_000_000_000.0_f64] },
            "results": {},
            "uptime": 0,
        });
        let h = hiveos_stats_from_summary(&summary);
        assert_eq!(h["hs"], serde_json::json!([5_000_000]));
        assert_ne!(h["hs"], serde_json::json!([5000])); // ÷1e6 would be this
        assert_ne!(h["hs"], serde_json::json!([5])); // ÷1e9 would be this
    }

    #[test]
    fn hiveos_stats_empty_summary_is_zero_but_valid() {
        // Defensive: a malformed/empty summary must NOT panic and must yield a
        // structurally-valid zero h-stats object so the rig still reports.
        let h = hiveos_stats_from_summary(&serde_json::json!({}));
        assert_eq!(h["hs"], serde_json::json!([0]));
        assert_eq!(h["hs_units"], "khs");
        assert_eq!(h["ar"], serde_json::json!([0, 0, 0, 0]));
        assert_eq!(h["uptime"], 0);
        assert_eq!(h["algo"], "sha256d");
        assert_eq!(h["temp"], serde_json::json!([]));
        assert_eq!(h["fan"], serde_json::json!([]));
    }

    #[test]
    fn hiveos_temp_populated_from_gpu_temp_c_when_present() {
        // The OPTIONAL nvml build's summary carries health.gpu_temp_c → temp[<C>].
        let summary = serde_json::json!({
            "hashrate": { "total": [3_000_000_000.0_f64] },
            "results": { "shares_good": 1 },
            "uptime": 10,
            "health": { "gpu_temp_c": 64.7, "gpu_power_w": 150.0 },
        });
        let h = hiveos_stats_from_summary(&summary);
        // Rounded to a whole degree (HiveOS shows integers).
        assert_eq!(h["temp"], serde_json::json!([65]));
        // Power isn't a HiveOS h-stats field; it stays out of the object.
        assert!(h.get("power").is_none());
    }

    #[test]
    fn hiveos_temp_empty_when_gpu_temp_absent() {
        // Default/fleet build (no health object) ⇒ temp[] stays empty, exactly
        // as the pre-nvml behaviour.
        let summary = serde_json::json!({
            "hashrate": { "total": [3_000_000_000.0_f64] },
            "results": {},
            "uptime": 0,
        });
        let h = hiveos_stats_from_summary(&summary);
        assert_eq!(h["temp"], serde_json::json!([]));
        // A present-but-empty health object (no gpu_temp_c) is also empty temp.
        let summary2 = serde_json::json!({
            "hashrate": { "total": [0.0] },
            "results": {},
            "uptime": 0,
            "health": {},
        });
        assert_eq!(hiveos_stats_from_summary(&summary2)["temp"], serde_json::json!([]));
    }

    #[test]
    fn hiveos_stats_tolerates_null_and_missing_fields() {
        // hashrate present but total empty; results present but partial; uptime null.
        let summary = serde_json::json!({
            "hashrate": { "total": [] },
            "results": { "shares_good": 7 },
            "uptime": serde_json::Value::Null,
        });
        let h = hiveos_stats_from_summary(&summary);
        assert_eq!(h["hs"], serde_json::json!([0])); // empty total → 0
        assert_eq!(h["ar"], serde_json::json!([7, 0, 0, 0])); // only good present
        assert_eq!(h["uptime"], 0); // null → 0
    }

    #[test]
    fn parse_gpu_ids_accepts_lists_and_rejects_junk() {
        assert_eq!(parse_gpu_ids("0,2,3").unwrap(), vec![0, 2, 3]);
        assert_eq!(parse_gpu_ids("1").unwrap(), vec![1]);
        assert_eq!(parse_gpu_ids("").unwrap(), Vec::<usize>::new());
        // Whitespace is tolerated around entries and the whole string.
        assert_eq!(parse_gpu_ids(" 0, 2 ,3 ").unwrap(), vec![0, 2, 3]);
        assert_eq!(parse_gpu_ids("   ").unwrap(), Vec::<usize>::new());
        // A non-numeric entry is a clear Err.
        assert!(parse_gpu_ids("0,x,2").is_err());
        assert!(parse_gpu_ids("-1").is_err()); // usize: negative is junk
    }
}
