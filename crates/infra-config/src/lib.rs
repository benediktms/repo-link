//! infra-config — resolves the runtime configuration for repo-link.
//!
//! Layering, lowest-to-highest precedence:
//! 1. Platform defaults (data dir from `directories::ProjectDirs`).
//! 2. On-disk token file at the platform config dir (fallback for env vars).
//! 3. Environment variables prefixed `REPO_LINK_`, plus the conventional
//!    `GITHUB_TOKEN` / `USER` fallbacks.
//! 4. Explicit overrides passed at the call site.
//!
//! Backed by `figment` so adding TOML/JSON sources later is a one-liner.

mod config;
mod error;
mod paths;
mod token_file;

pub use config::RepoLinkConfig;
pub use error::{ConfigError, TokenFileError};
pub use paths::{
    DAEMON_LABEL, default_daemon_log_path, default_db_path, default_last_tick_path,
    default_launch_agent_path, default_systemd_unit_path, default_token_file_path,
};
pub use token_file::TokenFileContents;
