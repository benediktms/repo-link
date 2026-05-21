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

use std::path::{Path, PathBuf};

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

/// Failure modes when loading the on-disk GitHub token.
///
/// `InsecurePermissions` mirrors OpenSSH's private-key check: any group or
/// world bit set means we refuse to read the file rather than silently use
/// a token that's exposed via `ls -la` or backups.
///
/// Notably absent: there's no `NotFound` or `InvalidToken` variant.
/// - **Missing file** is not an error — the file is one entry in a fallback
///   chain (env → file). [`RepoLinkConfig::resolve_github_token`] returns
///   `Ok(None)` when neither source provides a token, and the CLI's
///   `sync_dispatch` turns that into the "set REPO_LINK_GITHUB_TOKEN or run
///   `rl gh auth`" user-facing error. Encoding missing as `Err` here would
///   force every caller to pattern-match around a normal flow.
/// - **Invalid token** can only be detected by the GitHub API at request
///   time, so it surfaces as a remote provider error from `infra-github`,
///   not as a config error.
#[derive(Debug, Error)]
pub enum TokenFileError {
    #[error("token file {p}: {source}", p = path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "token file {p} has insecure permissions (mode {mode:04o}).\n\
         Restrict access with: chmod 600 \"{p}\"",
        p = path.display()
    )]
    InsecurePermissions { path: PathBuf, mode: u32 },
}

#[derive(Debug, Deserialize, Default)]
struct RawEnv {
    db: Option<PathBuf>,
    github_token: Option<String>,
    github_token_file: Option<PathBuf>,
    user: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RepoLinkConfig {
    pub database_path: PathBuf,
    /// Env-derived token only (REPO_LINK_GITHUB_TOKEN or GITHUB_TOKEN).
    /// For the full resolution chain that also walks to the on-disk file,
    /// call [`Self::resolve_github_token`].
    pub github_token: Option<String>,
    pub default_user: Option<String>,
    /// Resolved path for the on-disk GitHub token. The file may or may not
    /// exist; it's read on demand by [`Self::resolve_github_token`] and
    /// written to by `rl gh auth`.
    pub token_file_path: PathBuf,
}

impl RepoLinkConfig {
    /// Resolve config from env vars + platform defaults. Pure read, no I/O
    /// against the token file — that's deferred to `resolve_github_token`
    /// so commands that don't need a token (e.g. `workspace list`) stay
    /// unaffected by a misconfigured file.
    pub fn from_env() -> Result<Self, ConfigError> {
        let raw: RawEnv = Figment::new()
            .merge(Env::prefixed("REPO_LINK_"))
            .extract()
            .map_err(|e| ConfigError::Figment(e.to_string()))?;

        // Filter empties per-source so a blank REPO_LINK_GITHUB_TOKEN (common
        // in CI where vars are set but unpopulated) cascades to GITHUB_TOKEN
        // instead of short-circuiting the precedence chain.
        let github_token = raw
            .github_token
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("GITHUB_TOKEN").ok().filter(|s| !s.is_empty()));
        let default_user = raw.user.or_else(|| std::env::var("USER").ok());
        let database_path = match raw.db {
            Some(p) => p,
            None => default_db_path()?,
        };
        let token_file_path = match raw.github_token_file {
            Some(p) => p,
            None => default_token_file_path()?,
        };
        Ok(Self {
            database_path,
            github_token,
            default_user,
            token_file_path,
        })
    }

    /// Override the database path. Useful for tests and `--db` flags.
    pub fn with_database_path(mut self, path: PathBuf) -> Self {
        self.database_path = path;
        self
    }

    /// Effective GitHub token: env-derived value wins; otherwise read from
    /// `token_file_path`. Returns `Ok(None)` if neither source provides a
    /// token. Insecure file permissions or other I/O errors propagate so
    /// the caller can surface the security issue verbatim.
    pub fn resolve_github_token(&self) -> Result<Option<String>, TokenFileError> {
        if let Some(t) = self.github_token.as_deref()
            && !t.is_empty()
        {
            return Ok(Some(t.to_string()));
        }
        read_token_file(&self.token_file_path)
    }
}

pub fn default_db_path() -> Result<PathBuf, ConfigError> {
    let dirs = ProjectDirs::from("", "", "repo-link").ok_or(ConfigError::NoDataDir)?;
    Ok(dirs.data_dir().join("repo-link.db"))
}

pub fn default_token_file_path() -> Result<PathBuf, ConfigError> {
    let dirs = ProjectDirs::from("", "", "repo-link").ok_or(ConfigError::NoDataDir)?;
    Ok(dirs.config_dir().join("github_token"))
}

fn read_token_file(path: &Path) -> Result<Option<String>, TokenFileError> {
    use std::io::Read;

    // Open first, then fstat through the file handle so the permission check
    // and the content read both target the same inode. Avoids a TOCTOU swap
    // between two path-based syscalls.
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(TokenFileError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let metadata = file.metadata().map_err(|source| TokenFileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    enforce_secure_permissions(&metadata, path)?;
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|source| TokenFileError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let trimmed = raw.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
}

#[cfg(unix)]
fn enforce_secure_permissions(
    metadata: &std::fs::Metadata,
    path: &Path,
) -> Result<(), TokenFileError> {
    use std::os::unix::fs::MetadataExt;
    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(TokenFileError::InsecurePermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn enforce_secure_permissions(
    _metadata: &std::fs::Metadata,
    _path: &Path,
) -> Result<(), TokenFileError> {
    Ok(())
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
            token_file_path: PathBuf::from("/tmp/github_token"),
        }
        .with_database_path("/tmp/y.db".into());
        assert_eq!(cfg.database_path, PathBuf::from("/tmp/y.db"));
    }

    #[test]
    fn read_token_file_missing_returns_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist");
        assert!(read_token_file(&path).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn read_token_file_empty_returns_none() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "   \n  \n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_token_file(&path).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn read_token_file_strips_trailing_whitespace() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "abc123\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_token_file(&path).unwrap().as_deref(), Some("abc123"));
    }

    #[cfg(unix)]
    #[test]
    fn read_token_file_rejects_group_or_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "abc123").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = read_token_file(&path).unwrap_err();
        match err {
            TokenFileError::InsecurePermissions { mode, .. } => assert_eq!(mode, 0o644),
            other => panic!("expected InsecurePermissions, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolve_github_token_prefers_env_over_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "from-file").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let cfg = RepoLinkConfig {
            database_path: PathBuf::from("/tmp/x.db"),
            github_token: Some("from-env".into()),
            default_user: None,
            token_file_path: path,
        };
        assert_eq!(cfg.resolve_github_token().unwrap().as_deref(), Some("from-env"));
    }
}
