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
            // csd extensions (xmrig consumers ignore unknown fields): connection
            // churn so an operator sees an unstable link / flaky primary pool.
            // NB: "failovers" = endpoint ROTATIONS (accept-deadman dead-man),
            // distinct from xmrig's own connection-`failures` notion; every
            // failover is also a reconnect, so reconnects >= failovers.
            "reconnects": health.reconnects,
            "failovers": health.failovers,
        },
    })
}

/// The routes the `--stats-port` server answers.
#[derive(Debug, PartialEq, Eq)]
pub enum StatsRoute {
    /// xmrig-compatible summary JSON.
    Summary,
    /// Liveness probe (200 + a tiny body) for an external monitor (Uptime Robot).
    Health,
    /// Anything else → 404.
    NotFound,
}

/// Extract the request target (path) from an HTTP request line like
/// `"GET /1/summary HTTP/1.1"`. Returns `None` unless it's a `GET`.
pub fn request_target(request_line: &str) -> Option<&str> {
    let mut parts = request_line.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    parts.next()
}

/// Route a request path (query string ignored) to a [`StatsRoute`].
pub fn route(path: &str) -> StatsRoute {
    match path.split('?').next().unwrap_or(path) {
        "/1/summary" | "/summary" | "/" => StatsRoute::Summary,
        "/healthz" | "/health" => StatsRoute::Health,
        _ => StatsRoute::NotFound,
    }
}

/// Format a minimal HTTP/1.1 response (always `Connection: close`).
pub fn http_response(status: u16, content_type: &str, body: &str) -> String {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "OK",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// The longest hashrate window (15 minutes) in milliseconds — also the prune
/// horizon, so the sample buffer stays bounded.
const WINDOW_15M_MS: u64 = 15 * 60 * 1000;

/// Rolling hashrate windows (10s / 60s / 15m) feeding xmrig's `hashrate.total`
/// triple. Fed combined GH/s samples stamped with a monotonic millisecond clock
/// (injected, like the watchdog's `now_ms`), so the windowing is unit-tested
/// without real time. Each window reports the mean of the samples within it.
#[derive(Default)]
pub struct HashrateWindows {
    /// `(now_ms, combined_ghs)`, pruned to the 15m horizon on every `record`.
    samples: Vec<(u64, f64)>,
}

impl HashrateWindows {
    /// An empty tracker (all windows read 0.0 until the first `record`).
    pub fn new() -> Self {
        HashrateWindows {
            samples: Vec::new(),
        }
    }

    /// Record a combined-hashrate sample (GH/s) at `now_ms`, then drop samples
    /// older than the 15m horizon so memory stays bounded.
    pub fn record(&mut self, ghs: f64, now_ms: u64) {
        self.samples.push((now_ms, ghs));
        let cutoff = now_ms.saturating_sub(WINDOW_15M_MS);
        self.samples.retain(|&(t, _)| t >= cutoff);
    }

    /// The `[10s, 60s, 15m]` window means (GH/s) at `now_ms`. A window with no
    /// samples reads 0.0.
    pub fn windows(&self, now_ms: u64) -> [f64; 3] {
        [
            self.window_avg(now_ms, 10 * 1000),
            self.window_avg(now_ms, 60 * 1000),
            self.window_avg(now_ms, WINDOW_15M_MS),
        ]
    }

    /// Mean of the samples within `span_ms` of `now_ms` (0.0 if none).
    fn window_avg(&self, now_ms: u64, span_ms: u64) -> f64 {
        let cutoff = now_ms.saturating_sub(span_ms);
        let (sum, n) = self
            .samples
            .iter()
            .filter(|&&(t, _)| t >= cutoff)
            .fold((0.0_f64, 0u32), |(s, c), &(_, g)| (s + g, c + 1));
        if n == 0 {
            0.0
        } else {
            sum / n as f64
        }
    }
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
            reconnects: 4,
            failovers: 2,
        };
        let v = summary_json("csd1abc", &h, [1.0, 1.2, 1.1], 3600);
        assert_eq!(v["worker_id"], "csd1abc");
        assert_eq!(v["uptime"], 3600);
        assert_eq!(v["results"]["shares_good"], 5);
        assert_eq!(v["results"]["shares_total"], 9);
        assert_eq!(v["results"]["shares_rejected"], 1);
        assert_eq!(v["connection"]["pool"], "pool.test:3333");
        assert_eq!(v["connection"]["reconnects"], 4);
        assert_eq!(v["connection"]["failovers"], 2);
        let total = v["hashrate"]["total"].as_array().unwrap();
        assert_eq!(total.len(), 3);
        assert_eq!(total[0], 1.0e9); // 1 GH/s → 1e9 H/s
        assert_eq!(v["hashrate"]["highest"], 1.2e9);
    }

    #[test]
    fn request_target_extracts_get_path() {
        assert_eq!(request_target("GET /1/summary HTTP/1.1"), Some("/1/summary"));
        assert_eq!(request_target("GET / HTTP/1.1"), Some("/"));
        assert_eq!(request_target("POST /1/summary HTTP/1.1"), None); // only GET
        assert_eq!(request_target(""), None);
        assert_eq!(request_target("GET"), None); // no target
    }

    #[test]
    fn route_maps_paths_and_ignores_query() {
        assert_eq!(route("/1/summary"), StatsRoute::Summary);
        assert_eq!(route("/summary"), StatsRoute::Summary);
        assert_eq!(route("/"), StatsRoute::Summary);
        assert_eq!(route("/1/summary?token=abc"), StatsRoute::Summary); // query stripped
        assert_eq!(route("/healthz"), StatsRoute::Health);
        assert_eq!(route("/health"), StatsRoute::Health);
        assert_eq!(route("/admin"), StatsRoute::NotFound);
        assert_eq!(route("/1/config"), StatsRoute::NotFound);
    }

    #[test]
    fn http_response_has_status_and_correct_content_length() {
        let body = r#"{"ok":true}"#;
        let resp = http_response(200, "application/json", body);
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(resp.contains("Connection: close\r\n"));
        assert!(resp.ends_with(body)); // body is last, after the blank line
        assert!(http_response(404, "text/plain", "nope").starts_with("HTTP/1.1 404 Not Found\r\n"));
    }

    #[test]
    fn hashrate_windows_constant_rate_converges_all_windows() {
        let mut w = HashrateWindows::new();
        // A constant 2.0 GH/s once a second for 20 minutes.
        for i in 0..=1200u64 {
            w.record(2.0, i * 1000);
        }
        let [s10, s60, s15m] = w.windows(1200 * 1000);
        assert!((s10 - 2.0).abs() < 1e-9, "10s={s10}");
        assert!((s60 - 2.0).abs() < 1e-9, "60s={s60}");
        assert!((s15m - 2.0).abs() < 1e-9, "15m={s15m}");
    }

    #[test]
    fn hashrate_windows_step_moves_short_window_first() {
        let mut w = HashrateWindows::new();
        // 10 minutes at 1.0 GH/s, then a step up to 5.0 for the last 10 seconds.
        for i in 0..600u64 {
            w.record(1.0, i * 1000);
        }
        let base = 600 * 1000;
        for i in 0..=10u64 {
            w.record(5.0, base + i * 1000);
        }
        let now = base + 10 * 1000;
        let [s10, s60, s15m] = w.windows(now);
        assert!(s10 > s60, "10s ({s10}) should exceed 60s ({s60})");
        assert!(s60 > s15m, "60s ({s60}) should exceed 15m ({s15m})");
        assert!(s10 > 4.0, "10s window should be near the 5.0 step, got {s10}");
    }

    #[test]
    fn hashrate_windows_empty_reads_zero() {
        let w = HashrateWindows::new();
        assert_eq!(w.windows(123_456), [0.0, 0.0, 0.0]);
    }
}
