//! infra-config — resolves the runtime configuration for repo-link.
//!
//! Layering, lowest-to-highest precedence:
//! 1. Platform defaults (data dir from `directories::ProjectDirs`).
//! 2. Environment variables prefixed `REPO_LINK_`, plus the conventional
//!    `GITHUB_TOKEN` / `USER` fallbacks.
//! 3. Explicit overrides passed at the call site.
//!
//! Backed by `figment` so adding TOML/JSON sources later is a one-liner.

use std::path::PathBuf;

use directories::ProjectDirs;
use figment::{Figment, providers::Env};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config: {0}")]
    Figment(String),
    #[error("could not determine platform data directory")]
    NoDataDir,
}

#[derive(Debug, Deserialize, Default)]
struct RawEnv {
    db: Option<PathBuf>,
    github_token: Option<String>,
    user: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RepoLinkConfig {
    pub database_path: PathBuf,
    pub github_token: Option<String>,
    pub default_user: Option<String>,
}

impl RepoLinkConfig {
    /// Resolve config from env vars + platform defaults. Pure read, no I/O.
    pub fn from_env() -> Result<Self, ConfigError> {
        let raw: RawEnv = Figment::new()
            .merge(Env::prefixed("REPO_LINK_"))
            .extract()
            .map_err(|e| ConfigError::Figment(e.to_string()))?;

        let github_token = raw
            .github_token
            .or_else(|| std::env::var("GITHUB_TOKEN").ok());
        let default_user = raw.user.or_else(|| std::env::var("USER").ok());
        let database_path = match raw.db {
            Some(p) => p,
            None => default_db_path()?,
        };
        Ok(Self {
            database_path,
            github_token,
            default_user,
        })
    }

    /// Override the database path. Useful for tests and `--db` flags.
    pub fn with_database_path(mut self, path: PathBuf) -> Self {
        self.database_path = path;
        self
    }
}

pub fn default_db_path() -> Result<PathBuf, ConfigError> {
    let dirs = ProjectDirs::from("", "", "repo-link").ok_or(ConfigError::NoDataDir)?;
    Ok(dirs.data_dir().join("repo-link.db"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_db_override_wins() {
        // We can't easily isolate env vars in unit tests without a mutex,
        // so exercise the override path directly.
        let cfg = RepoLinkConfig {
            database_path: PathBuf::from("/tmp/x.db"),
            github_token: None,
            default_user: None,
        }
        .with_database_path("/tmp/y.db".into());
        assert_eq!(cfg.database_path, PathBuf::from("/tmp/y.db"));
    }
}
