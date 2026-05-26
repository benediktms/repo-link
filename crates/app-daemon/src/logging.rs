//! Daemon logging setup.
//!
//! Two output lanes, chosen by `--log-format`:
//! - `pretty`: ANSI-coloured `fmt::Layer` to stdout. Cheap to tail
//!   locally during development.
//! - `json`: structured `fmt::Layer().json()` to a daily-rotated file at
//!   `<log_dir>/daemon.YYYY-MM-DD.log`. The appender keeps the 7 newest
//!   segments and deletes older ones automatically. `rl daemon status`
//!   resolves `log_path` by globbing the directory for the newest segment
//!   so consumers always see the file currently being written to. The
//!   launchd plist's `StandardOutPath`/`StandardErrorPath` separately
//!   redirect to `<log_dir>/daemon.log` (no date suffix) — that's a
//!   pre-tracing panic catcher and is independent of this appender.
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
    /// JSON-per-line written to a daily-rotated segment at
    /// `<log_dir>/daemon.YYYY-MM-DD.log`, keeping the 7 most recent
    /// segments. Used by the installed daemon manifest so structured
    /// ingestion (jq, Datadog, etc.) gets a stable shape. Consumers that
    /// want a stable path use `rl daemon status.log_path`, which globs
    /// for the newest segment.
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
            // Daily-rotated segments at `<log_dir>/daemon.YYYY-MM-DD.log`,
            // keeping 7 days of history. The appender prunes old segments
            // on its own at midnight UTC. `rl daemon status` resolves the
            // newest segment via glob, so consumers don't need to know the
            // date pattern. If the builder fails (e.g., a permissions
            // change on `log_dir` between the `create_dir_all` above and
            // here), fall back to stdout JSON so the daemon at least
            // produces some output instead of silently swallowing events.
            let file_appender = match tracing_appender::rolling::RollingFileAppender::builder()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix("daemon")
                .filename_suffix("log")
                .max_log_files(7)
                .build(log_dir)
            {
                Ok(a) => a,
                Err(e) => {
                    eprintln!(
                        "[daemon] could not init rolling appender in {} ({e}); falling back to stdout json",
                        log_dir.display()
                    );
                    tracing_subscriber::registry()
                        .with(env_filter)
                        .with(tracing_subscriber::fmt::layer().json())
                        .init();
                    return None;
                }
            };
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
