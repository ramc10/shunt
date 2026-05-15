use anyhow::Result;
use std::path::Path;
use std::time::SystemTime;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Initialise logging: JSON lines to file + human-readable to stderr.
///
/// Returns a `WorkerGuard` that must be kept alive for the duration of the
/// process — dropping it flushes and closes the log file writer.
pub fn setup(log_file: &Path, level: &str) -> Result<WorkerGuard> {
    if let Some(parent) = log_file.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file_appender = tracing_appender::rolling::daily(
        log_file.parent().unwrap_or(Path::new(".")),
        log_file.file_name().unwrap_or(std::ffi::OsStr::new("proxy.log")),
    );
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level));

    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(non_blocking)
        .with_filter(filter.clone());

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_filter(filter);

    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .init();

    Ok(guard)
}

/// Delete rotated log files older than `keep_days` days in the same directory
/// as the log file. Files are matched by the log file name prefix (e.g. "proxy.log").
/// This prevents unbounded accumulation of daily-rotated log files.
pub fn prune_old_logs(log_file: &Path, keep_days: u64) {
    let dir = match log_file.parent() {
        Some(d) => d,
        None => return,
    };
    let prefix = match log_file.file_name().and_then(|n| n.to_str()) {
        Some(p) => p.to_owned(),
        None => return,
    };
    let cutoff = SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(keep_days * 24 * 3600))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        // Only prune files that start with our log prefix but are not the current file
        // (e.g. "proxy.log.2024-01-01", not "proxy.log" itself)
        if !name.starts_with(&prefix) || name == prefix { continue }
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if modified < cutoff {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};

    fn set_mtime(path: &std::path::Path, age_secs: u64) {
        // Back-date the file by writing via filetime crate is not available,
        // but we can use std::fs::File + set_modified via the filetime workaround:
        // Instead we directly set via libc on unix.
        #[cfg(unix)]
        {
            let past = SystemTime::now()
                .checked_sub(Duration::from_secs(age_secs))
                .unwrap();
            let secs = past.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
            let ts = libc::timespec { tv_sec: secs as libc::time_t, tv_nsec: 0 };
            let times = [ts, ts];
            let path_cstr = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            unsafe { libc::utimensat(libc::AT_FDCWD, path_cstr.as_ptr(), times.as_ptr(), 0) };
        }
        // On non-unix platforms the test is a no-op (we just don't back-date).
        let _ = path; let _ = age_secs;
    }

    #[test]
    fn test_prune_old_logs_removes_stale_rotated_files() {
        let dir = std::env::temp_dir().join(format!(
            "shunt_prune_test_{}",
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        let log_file = dir.join("proxy.log");
        // Current log file — must NOT be deleted
        fs::write(&log_file, b"current").unwrap();

        // Old rotated file (10 days old) — MUST be deleted
        let old = dir.join("proxy.log.2020-01-01");
        fs::write(&old, b"old").unwrap();
        set_mtime(&old, 10 * 24 * 3600);

        // Recent rotated file (1 day old) — must NOT be deleted
        let recent = dir.join("proxy.log.2024-12-31");
        fs::write(&recent, b"recent").unwrap();
        set_mtime(&recent, 24 * 3600);

        // Unrelated file — must NOT be deleted
        let other = dir.join("other.log");
        fs::write(&other, b"unrelated").unwrap();

        prune_old_logs(&log_file, 7);

        #[cfg(unix)]
        {
            assert!(log_file.exists(), "current log must survive");
            assert!(!old.exists(),    "old rotated log must be pruned");
            assert!(recent.exists(),  "recent rotated log must survive");
        }
        assert!(other.exists(), "unrelated file must survive");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_prune_old_logs_keeps_current_log() {
        let dir = std::env::temp_dir().join(format!(
            "shunt_prune_current_{}",
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        let log_file = dir.join("proxy.log");
        fs::write(&log_file, b"current").unwrap();
        // Back-date the main log file too — it should still be kept because name == prefix
        set_mtime(&log_file, 30 * 24 * 3600);

        prune_old_logs(&log_file, 1);
        assert!(log_file.exists(), "exact log file name must never be pruned");

        fs::remove_dir_all(&dir).ok();
    }
}
