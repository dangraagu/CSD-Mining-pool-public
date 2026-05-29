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
