//! rld — repo-link's background reconciliation + sync daemon.
//!
//! The runtime is `tokio` running **two concurrent background tasks** (Stage 7,
//! #55) coordinated by a shared `tokio::sync::watch<bool>` cancellation.
//!
//! **Poller task** (`Daemon::tick_once`, cadence `PROJECT_POLLER_INTERVAL`).
//! For each non-archived workspace it runs, in order: (1)
//! `RepoBindingService::reconcile_worktrees` (mark-only — fold vanished
//! worktrees to `MissingPath`, never prunes here); (2) a grace-counter pass
//! (only when `--prune` is set — re-probe each `MissingPath` worktree, bump a
//! process-local counter while it stays missing, drop the link once the counter
//! hits `missing_grace_ticks` consecutive misses, reset if the path returns;
//! counts are NOT persisted across restarts); (3) the project poll
//! (`ProjectPoller::poll_once` — pull project-board state back from GitHub and
//! correlate items with local tasks). This task also writes the single combined
//! `last_tick.json` heartbeat, so `rl daemon status` has one primary cadence to
//! measure "wedged" against.
//!
//! **Drainer task** (`Daemon::drain_tick`). Drains the outbound outbox. Its
//! primary trigger is just-in-time (`tokio::sync::Notify`); a fixed
//! `OUTBOX_DRAINER_PERIODIC_SWEEP` ticker catches CLI-originated (cross-process)
//! enqueues the in-process Notify can't see.
//!
//! Each task does its first unit of work immediately on startup (#88), then
//! settles into its cadence. A panic in either task trips the shared
//! cancellation so the other stops too; SIGINT and (on unix) SIGTERM both
//! trigger a clean shutdown. Both task bodies are testable via the public
//! `Daemon::tick_once` / `Daemon::drain_tick`.

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
