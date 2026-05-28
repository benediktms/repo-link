//! [`RepoLinkConfig`] — the resolved runtime configuration.

use std::path::PathBuf;

use figment::{Figment, providers::Env};
use serde::Deserialize;

use crate::error::{ConfigError, TokenFileError};
use crate::paths::{default_db_path, default_token_file_path};
use crate::token_file::read_token_file_contents;

#[derive(Debug, Deserialize, Default)]
struct RawEnv {
    db: Option<PathBuf>,
    github_token: Option<String>,
    github_token_file: Option<PathBuf>,
    github_login: Option<String>,
    github_api_base_url: Option<String>,
    user: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RepoLinkConfig {
    pub database_path: PathBuf,
    /// Env-derived token only (REPO_LINK_GITHUB_TOKEN or GITHUB_TOKEN).
    /// For the full resolution chain that also walks to the on-disk file,
    /// call [`Self::resolve_github_token`].
    pub github_token: Option<String>,
    /// Env-derived GitHub login only (REPO_LINK_GITHUB_LOGIN). For the full
    /// resolution chain that also reads the cached login from the token
    /// file, call [`Self::resolve_github_login`].
    pub github_login: Option<String>,
    /// Override for the GitHub REST API root, sourced from
    /// `REPO_LINK_GITHUB_API_BASE_URL`. `None` means the provider talks to
    /// `api.github.com`. Useful for GitHub Enterprise and for pointing the
    /// CLI at a mock server in integration tests.
    pub github_api_base_url: Option<String>,
    pub default_user: Option<String>,
    /// Resolved path for the on-disk GitHub token. The file may or may not
    /// exist; it's read on demand by [`Self::resolve_github_token`] /
    /// [`Self::resolve_github_login`] and written to by `rl gh auth`.
    ///
    /// File format: line 1 is the token, optional line 2 is the cached
    /// GitHub login. Single-line files written by older versions still
    /// parse — the login simply comes back as `None`.
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
        let github_login = raw.github_login.filter(|s| !s.is_empty());
        let github_api_base_url = raw.github_api_base_url.filter(|s| !s.is_empty());
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
            github_login,
            github_api_base_url,
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
        Ok(read_token_file_contents(&self.token_file_path)?.token)
    }

    /// Effective GitHub login: env-derived value (`REPO_LINK_GITHUB_LOGIN`)
    /// wins; otherwise read from line 2 of the token file. Returns
    /// `Ok(None)` if neither source provides a login. Inherits the same
    /// permission check as the token, since they share a file.
    pub fn resolve_github_login(&self) -> Result<Option<String>, TokenFileError> {
        if let Some(l) = self.github_login.as_deref()
            && !l.is_empty()
        {
            return Ok(Some(l.to_string()));
        }
        Ok(read_token_file_contents(&self.token_file_path)?.login)
    }
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
            github_login: None,
            github_api_base_url: None,
            default_user: None,
            token_file_path: PathBuf::from("/tmp/github_token"),
        }
        .with_database_path("/tmp/y.db".into());
        assert_eq!(cfg.database_path, PathBuf::from("/tmp/y.db"));
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
            github_login: None,
            github_api_base_url: None,
            default_user: None,
            token_file_path: path,
        };
        assert_eq!(
            cfg.resolve_github_token().unwrap().as_deref(),
            Some("from-env")
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_github_login_reads_line_two_of_token_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "tok\nbenediktms\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let cfg = RepoLinkConfig {
            database_path: PathBuf::from("/tmp/x.db"),
            github_token: None,
            github_login: None,
            github_api_base_url: None,
            default_user: None,
            token_file_path: path,
        };
        assert_eq!(
            cfg.resolve_github_login().unwrap().as_deref(),
            Some("benediktms")
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_github_login_prefers_env_over_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "tok\nfrom-file\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let cfg = RepoLinkConfig {
            database_path: PathBuf::from("/tmp/x.db"),
            github_token: None,
            github_login: Some("from-env".into()),
            github_api_base_url: None,
            default_user: None,
            token_file_path: path,
        };
        assert_eq!(
            cfg.resolve_github_login().unwrap().as_deref(),
            Some("from-env")
        );
    }

    #[test]
    fn resolve_github_login_returns_none_when_no_source() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = RepoLinkConfig {
            database_path: PathBuf::from("/tmp/x.db"),
            github_token: None,
            github_login: None,
            github_api_base_url: None,
            default_user: None,
            token_file_path: dir.path().join("does-not-exist"),
        };
        assert!(cfg.resolve_github_login().unwrap().is_none());
    }
}
