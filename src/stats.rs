//! Miner telemetry in the xmrig `/1/summary` JSON shape (P3 D2).
//!
//! xmrig's summary schema is the de-facto standard that mining dashboards
//! (Awesome Miner, Home Assistant, custom scrapers) already understand, so we
//! emit a compatible subset. This module is the **pure JSON builder**; the
//! `--stats-port` HTTP server that serves it (and pulls the live hashrate +
//! [`HealthSnapshot`]) is wired separately.

use crate::stratum::client::HealthSnapshot;

/// Build an xmrig `/1/summary`-shaped JSON summary from the worker id, the live
/// [`HealthSnapshot`], the three hashrate windows (10s / 60s / 15m, in **GH/s**),
/// and uptime. Pure → unit-tested. xmrig reports hashrate in H/s, so the GH/s
/// inputs are scaled by 1e9.
pub fn summary_json(
    worker: &str,
    health: &HealthSnapshot,
    hashrate_ghs: [f64; 3],
    uptime_s: u64,
) -> serde_json::Value {
    let hs: Vec<f64> = hashrate_ghs.iter().map(|g| g * 1e9).collect();
    let highest = hs.iter().copied().fold(0.0_f64, f64::max);
    serde_json::json!({
        "id": "csd-pool-miner",
        "worker_id": worker,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime": uptime_s,
        "hashrate": {
            "total": hs,        // [10s, 60s, 15m] in H/s
            "highest": highest,
        },
        "results": {
            "shares_good": health.accepted,
            "shares_total": health.submitted,
            "shares_rejected": health.rejected,
            "shares_stale": health.stale,
            "best": [],
        },
        "connection": {
            "pool": health.endpoint,
            "uptime": uptime_s,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_json_is_xmrig_shaped() {
        let h = HealthSnapshot {
            accepted: 5,
            rejected: 1,
            stale: 2,
            submitted: 9,
            job_age_s: Some(3),
            endpoint: "pool.test:3333".to_string(),
        };
        let v = summary_json("csd1abc", &h, [1.0, 1.2, 1.1], 3600);
        assert_eq!(v["worker_id"], "csd1abc");
        assert_eq!(v["uptime"], 3600);
        assert_eq!(v["results"]["shares_good"], 5);
        assert_eq!(v["results"]["shares_total"], 9);
        assert_eq!(v["results"]["shares_rejected"], 1);
        assert_eq!(v["connection"]["pool"], "pool.test:3333");
        let total = v["hashrate"]["total"].as_array().unwrap();
        assert_eq!(total.len(), 3);
        assert_eq!(total[0], 1.0e9); // 1 GH/s → 1e9 H/s
        assert_eq!(v["hashrate"]["highest"], 1.2e9);
    }
}
