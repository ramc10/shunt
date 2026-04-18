use anyhow::Result;
use std::path::Path;
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
