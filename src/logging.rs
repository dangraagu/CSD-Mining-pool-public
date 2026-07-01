//! Instance log-file rotation for the miner.
//!
//! Duplicates the logic in `csd-node::logging` so the miner does not need
//! to depend on the node crate. Behavior: archive any pre-existing
//! `<log_dir>/<instance>.current.log` to `<log_dir>/<instance>-<ts>.log`,
//! then open a fresh `.current.log` for append.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

pub fn init(instance: &str, log_dir: &Path) -> Result<WorkerGuard> {
    std::fs::create_dir_all(log_dir).ok();
    rotate_previous(instance, log_dir).ok();

    let current = log_dir.join(format!("{}.current.log", instance));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&current)?;
    let (writer, guard) = tracing_appender::non_blocking(file);

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_ansi(false)
        .with_writer(writer);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    tracing::info!(
        "instance={} log_dir={} log_file={}",
        instance,
        log_dir.display(),
        current.display()
    );
    Ok(guard)
}

fn rotate_previous(instance: &str, log_dir: &Path) -> Result<()> {
    let cur = log_dir.join(format!("{}.current.log", instance));
    if !cur.exists() {
        return Ok(());
    }
    let mtime = match std::fs::metadata(&cur).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => SystemTime::now(),
    };
    let ts = format_ts_for_filename(mtime);
    let archived = log_dir.join(format!("{}-{}.log", instance, ts));
    let dest = make_unique(&archived);
    std::fs::rename(&cur, &dest)?;
    Ok(())
}

fn make_unique(base: &Path) -> PathBuf {
    if !base.exists() {
        return base.to_path_buf();
    }
    let mut i = 2u32;
    loop {
        let stem = base
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("log");
        let parent = base.parent().unwrap_or_else(|| Path::new("."));
        let candidate = parent.join(format!("{}-{}.log", stem, i));
        if !candidate.exists() {
            return candidate;
        }
        i += 1;
    }
}

fn format_ts_for_filename(t: SystemTime) -> String {
    use std::time::UNIX_EPOCH;
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = unix_to_ymdhms(secs as i64);
    format!("{:04}-{:02}-{:02}T{:02}-{:02}-{:02}Z", y, mo, d, h, mi, s)
}

fn unix_to_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400) as u32;
    let (y, m, d) = civil_from_days(days);
    let h = rem / 3600;
    let mi = (rem % 3600) / 60;
    let s = rem % 60;
    (y, m, d, h, mi, s)
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y } as i32;
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, UNIX_EPOCH};

    /// Create (and return the path of) a fresh, unique, empty scratch directory
    /// under the OS temp dir. Std-lib only — the crate has no `tempfile` dev-dep,
    /// and these characterization tests deliberately avoid adding one. Uniqueness
    /// comes from pid + a nanosecond clock read + a per-process counter so
    /// parallel test threads never collide.
    fn scratch_dir() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "csd-miner-logtest-{}-{}-{}",
            std::process::id(),
            nanos,
            n
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    // ---- calendar / filename timestamp helpers ----

    #[test]
    fn format_ts_epoch_is_canonical_string() {
        // The UNIX epoch formats to the fixed, dash-in-time filename form.
        assert_eq!(
            format_ts_for_filename(UNIX_EPOCH),
            "1970-01-01T00-00-00Z"
        );
    }

    #[test]
    fn format_ts_uses_dashes_in_time_component() {
        // 2021-07-01T12:34:56Z == unix 1625142896. Filename form keeps '-' (not
        // ':') in the time part so it is a legal filename on every OS.
        let t = UNIX_EPOCH + Duration::from_secs(1_625_142_896);
        assert_eq!(format_ts_for_filename(t), "2021-07-01T12-34-56Z");
    }

    #[test]
    fn unix_to_ymdhms_epoch() {
        assert_eq!(unix_to_ymdhms(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymdhms_leap_day_2024() {
        // 2024-02-29T12:00:00Z == 1709208000 (Feb 29 exists only in a leap year).
        assert_eq!(unix_to_ymdhms(1_709_208_000), (2024, 2, 29, 12, 0, 0));
    }

    #[test]
    fn unix_to_ymdhms_year_boundary() {
        // Last second of 2023 and first second of 2024 straddle the boundary.
        assert_eq!(unix_to_ymdhms(1_704_067_199), (2023, 12, 31, 23, 59, 59));
        assert_eq!(unix_to_ymdhms(1_704_067_200), (2024, 1, 1, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymdhms_general_instant() {
        // 2021-07-01T12:34:56Z == 1625142896.
        assert_eq!(unix_to_ymdhms(1_625_142_896), (2021, 7, 1, 12, 34, 56));
    }

    // ---- make_unique collision handling ----

    #[test]
    fn make_unique_returns_base_when_free() {
        let dir = scratch_dir();
        let base = dir.join("csd.log");
        // Nothing exists yet ⇒ the base path is returned unchanged.
        assert_eq!(make_unique(&base), base);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn make_unique_skips_to_dash_three_when_base_and_two_exist() {
        let dir = scratch_dir();
        let base = dir.join("csd.log");
        // Occupy the base and the '-2' candidate; make_unique must return '-3'.
        std::fs::write(&base, b"x").unwrap();
        std::fs::write(dir.join("csd-2.log"), b"x").unwrap();
        let got = make_unique(&base);
        assert_eq!(got, dir.join("csd-3.log"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- rotate_previous archival ----

    #[test]
    fn rotate_previous_noop_when_no_current_log() {
        let dir = scratch_dir();
        // No <instance>.current.log present ⇒ Ok, and the dir stays empty.
        rotate_previous("nodeA", &dir).unwrap();
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert!(entries.is_empty(), "rotate created a file with no current log");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotate_previous_archives_existing_current_log() {
        let dir = scratch_dir();
        let current = dir.join("nodeA.current.log");
        std::fs::write(&current, b"prior contents").unwrap();

        rotate_previous("nodeA", &dir).unwrap();

        // The .current.log is gone (renamed), and exactly one archived file
        // remains: nodeA-<ts>.log carrying the original bytes.
        assert!(!current.exists(), "current.log should have been renamed away");
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(files.len(), 1, "expected exactly one archived file, got {files:?}");
        let archived = &files[0];
        assert!(archived.starts_with("nodeA-"), "bad archive name {archived}");
        assert!(archived.ends_with(".log"), "bad archive name {archived}");
        assert_ne!(archived, "nodeA.current.log");
        let body = std::fs::read(dir.join(archived)).unwrap();
        assert_eq!(body, b"prior contents");
        std::fs::remove_dir_all(&dir).ok();
    }
}
