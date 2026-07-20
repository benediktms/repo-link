//! CLI logging — a minimal `tracing` subscriber so advisories emitted on the
//! synchronous command path actually reach the user. Without this, the `rl` /
//! `repo-link` binaries install no subscriber and every `tracing::warn!` /
//! `debug!` fired inside the application services (e.g. RFC 0006's Type/Priority
//! "unavailable" / "unmapped" advisories in `application-task` /
//! `application-sync`) is silently dropped.
//!
//! Writes to **stderr only**: stdout is reserved for machine-readable JSON (see
//! [`crate::render`]), so log lines must never leak onto it. Default level is
//! `warn` (the advisories are `warn!`), keeping noisy HTTP crates quiet without
//! explicit tamping; `RUST_LOG` overrides (e.g. `RUST_LOG=debug` surfaces the
//! deferred `debug!` cases). Deliberately distinct from `app-daemon`'s
//! subscriber, which is daemon-shaped (stdout / launchd rolling file).

use tracing_subscriber::EnvFilter;

/// Install the process-global CLI subscriber. Idempotent and non-panicking:
/// `try_init` makes a second call (e.g. a future in-process `run` caller) a
/// no-op rather than a panic.
pub(crate) fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .try_init();
}
