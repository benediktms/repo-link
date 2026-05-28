//! rld — repo-link's background reconciliation + sync daemon.
//!
//! One periodic tick performs, for each non-archived workspace:
//! 1. `RepoBindingService::reconcile_worktrees` (mark-only) — fold any
//!    vanished worktrees into the binding status by flipping them to
//!    `MissingPath`. Never prunes from this call.
//! 2. Grace-counter pass (only when `--prune` is set) — re-probe each
//!    `MissingPath` worktree, bump a process-local counter while it stays
//!    missing, and drop the link once the counter hits
//!    `missing_grace_ticks` (default 3) consecutive misses. The counter
//!    resets if the path is observable again, so a transient unmount
//!    won't trigger a prune. Counts are NOT persisted across daemon
//!    restarts — restart resets to zero, which is the safer direction.
//! 3. If a `SyncService` is configured, push every task that is in
//!    `DirtyLocal` state. (Pull-side reconciliation is opt-in to keep the
//!    daemon from hammering the GitHub API; trigger it via `rl sync pull`.)
//!
//! The runtime is `tokio` with a single ticker + a ctrl-c watcher. The loop
//! is fully testable via `Daemon::tick_once`.

mod cli;
mod daemon;
mod error;
mod logging;
mod report;

pub use cli::{Args, run_cli};
pub use daemon::Daemon;
pub use error::DaemonError;
pub use logging::{LogFormat, init_subscriber};
pub use report::{LastTick, TickReport};
