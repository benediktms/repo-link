//! Daemon logging setup.
//!
//! Two output lanes, chosen by `--log-format`:
//! - `pretty`: ANSI-coloured `fmt::Layer` to stdout. Cheap to tail
//!   locally during development.
//! - `json`: structured `fmt::Layer().json()` to a daily-rotated file at
//!   `<log_dir>/daemon.log`. Intended for the installed launchd/systemd
//!   service, where another tool ingests the events.
//!
//! The file sink is wrapped in `tracing_appender::non_blocking` so writes
//! never block a tick. The returned [`WorkerGuard`] keeps the background
//! flush thread alive — callers must hold onto it for the daemon's
//! lifetime (drop at process exit flushes any buffered events).

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Default)]
#[clap(rename_all = "lowercase")]
pub enum LogFormat {
    /// ANSI-coloured pretty text to stdout. The default for foreground runs.
    #[default]
    Pretty,
    /// JSON-per-line to `<log_dir>/daemon.log`, daily-rotated. Used by the
    /// installed daemon manifest so structured ingestion (jq, Datadog, etc.)
    /// gets a stable shape.
    Json,
}

/// Initialise the global tracing subscriber. Idempotent in the sense that
/// `init` only succeeds once per process; subsequent calls return their
/// own guard but the second `set_global_default` call would fail — call
/// this exactly once from `run_cli`.
///
/// `log_dir` is created on demand. If it can't be created (e.g. read-only
/// FS in a test sandbox), JSON falls back to stdout so the daemon still
/// produces some output.
pub fn init_subscriber(format: LogFormat, log_dir: &Path) -> Option<WorkerGuard> {
    // Default filter: info+ for everything we emit, plus the noisy crates
    // tamped down. `RUST_LOG=…` overrides this for ad-hoc debugging.
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hyper=warn,sqlx=warn,reqwest=warn"));

    match format {
        LogFormat::Pretty => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(tracing_subscriber::fmt::layer().with_target(false))
                .init();
            None
        }
        LogFormat::Json => {
            if let Err(e) = std::fs::create_dir_all(log_dir) {
                eprintln!(
                    "[daemon] could not create log dir {} ({e}); falling back to stdout json",
                    log_dir.display()
                );
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(tracing_subscriber::fmt::layer().json())
                    .init();
                return None;
            }
            let file_appender = tracing_appender::rolling::daily(log_dir, "daemon.log");
            let (writer, guard) = tracing_appender::non_blocking(file_appender);
            tracing_subscriber::registry()
                .with(env_filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_writer(writer)
                        .with_ansi(false),
                )
                .init();
            Some(guard)
        }
    }
}
