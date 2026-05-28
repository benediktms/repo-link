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
    github_login: Option<String>,
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

pub fn default_db_path() -> Result<PathBuf, ConfigError> {
    let dirs = ProjectDirs::from("", "", "repo-link").ok_or(ConfigError::NoDataDir)?;
    Ok(dirs.data_dir().join("repo-link.db"))
}

pub fn default_token_file_path() -> Result<PathBuf, ConfigError> {
    let dirs = ProjectDirs::from("", "", "repo-link").ok_or(ConfigError::NoDataDir)?;
    Ok(dirs.config_dir().join("github_token"))
}

/// launchd label / systemd unit base name. Single source of truth so the
/// plist, the unit file, the `launchctl` argv, and `daemon status` agree.
pub const DAEMON_LABEL: &str = "com.benediktms.repo-link";

/// Path the daemon writes structured JSON logs to when launched under
/// launchd/systemd. Co-located with the SQLite DB in the platform data dir
/// so a single directory holds the daemon's entire state.
pub fn default_daemon_log_path() -> Result<PathBuf, ConfigError> {
    let dirs = ProjectDirs::from("", "", "repo-link").ok_or(ConfigError::NoDataDir)?;
    Ok(dirs.data_dir().join("daemon.log"))
}

/// Heartbeat file written by the daemon at the end of each tick. `rl daemon
/// status` reads it to flag a "loaded but wedged" daemon.
pub fn default_last_tick_path() -> Result<PathBuf, ConfigError> {
    let dirs = ProjectDirs::from("", "", "repo-link").ok_or(ConfigError::NoDataDir)?;
    Ok(dirs.data_dir().join("last_tick.json"))
}

/// macOS launchd user agent plist path. Deterministic on any platform —
/// derived from `$HOME` plus the macOS convention — so unit tests can assert
/// the path without `cfg(target_os)` gates. On Linux this path is computed
/// but never used (Linux uses [`default_systemd_unit_path`]).
pub fn default_launch_agent_path() -> Result<PathBuf, ConfigError> {
    Ok(home_dir()?
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{DAEMON_LABEL}.plist")))
}

/// Linux systemd `--user` unit path. Resolves `$XDG_CONFIG_HOME` first
/// (per the XDG Base Directory spec), falling back to `$HOME/.config`.
/// Always-defined for the same reason as [`default_launch_agent_path`].
pub fn default_systemd_unit_path() -> Result<PathBuf, ConfigError> {
    Ok(xdg_config_dir()?
        .join("systemd")
        .join("user")
        .join("repo-link.service"))
}

fn home_dir() -> Result<PathBuf, ConfigError> {
    use directories::BaseDirs;
    BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .ok_or(ConfigError::NoDataDir)
}

fn xdg_config_dir() -> Result<PathBuf, ConfigError> {
    // Per the XDG Base Directory spec: "All paths set in these environment
    // variables must be absolute. If an implementation encounters a relative
    // path in any of these variables it should consider the path invalid and
    // ignore it." Otherwise a stray `XDG_CONFIG_HOME=tmp` from the user's
    // shell would silently route the systemd unit under `$PWD/tmp/...`.
    if let Ok(v) = std::env::var("XDG_CONFIG_HOME")
        && !v.is_empty()
        && Path::new(&v).is_absolute()
    {
        return Ok(PathBuf::from(v));
    }
    Ok(home_dir()?.join(".config"))
}

/// Parsed contents of the on-disk token file. Two-line format: line 1 is
/// the token, optional line 2 is the cached GitHub login. Single-line files
/// written before the login was cached parse with `login = None`.
#[derive(Debug, Default)]
pub struct TokenFileContents {
    pub token: Option<String>,
    pub login: Option<String>,
}

fn read_token_file_contents(path: &Path) -> Result<TokenFileContents, TokenFileError> {
    use std::io::Read;

    // Open first, then fstat through the file handle so the permission check
    // and the content read both target the same inode. Avoids a TOCTOU swap
    // between two path-based syscalls.
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TokenFileContents::default());
        }
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
    let mut lines = raw.lines();
    let token = lines
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let login = lines
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(TokenFileContents { token, login })
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
            github_login: None,
            default_user: None,
            token_file_path: PathBuf::from("/tmp/github_token"),
        }
        .with_database_path("/tmp/y.db".into());
        assert_eq!(cfg.database_path, PathBuf::from("/tmp/y.db"));
    }

    #[test]
    fn read_token_file_missing_returns_empty_contents() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist");
        let c = read_token_file_contents(&path).unwrap();
        assert!(c.token.is_none());
        assert!(c.login.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn read_token_file_empty_returns_empty_contents() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "   \n  \n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let c = read_token_file_contents(&path).unwrap();
        assert!(c.token.is_none());
        assert!(c.login.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn read_token_file_legacy_single_line_token_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "abc123\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let c = read_token_file_contents(&path).unwrap();
        assert_eq!(c.token.as_deref(), Some("abc123"));
        assert!(c.login.is_none(), "single-line file must not invent a login");
    }

    #[cfg(unix)]
    #[test]
    fn read_token_file_two_line_yields_token_and_login() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "abc123\nbenediktms\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let c = read_token_file_contents(&path).unwrap();
        assert_eq!(c.token.as_deref(), Some("abc123"));
        assert_eq!(c.login.as_deref(), Some("benediktms"));
    }

    #[cfg(unix)]
    #[test]
    fn read_token_file_rejects_group_or_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("github_token");
        std::fs::write(&path, "abc123").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = read_token_file_contents(&path).unwrap_err();
        match err {
            TokenFileError::InsecurePermissions { mode, .. } => assert_eq!(mode, 0o644),
            other => panic!("expected InsecurePermissions, got {other:?}"),
        }
    }

    #[test]
    fn daemon_log_path_ends_with_daemon_log() {
        let p = default_daemon_log_path().unwrap();
        assert_eq!(p.file_name().unwrap(), "daemon.log");
        // Co-located with the db.
        assert_eq!(p.parent(), default_db_path().unwrap().parent());
    }

    #[test]
    fn last_tick_path_ends_with_last_tick_json() {
        let p = default_last_tick_path().unwrap();
        assert_eq!(p.file_name().unwrap(), "last_tick.json");
        assert_eq!(p.parent(), default_db_path().unwrap().parent());
    }

    #[test]
    fn launch_agent_path_uses_label_and_library_launchagents() {
        let p = default_launch_agent_path().unwrap();
        assert_eq!(
            p.file_name().unwrap(),
            format!("{DAEMON_LABEL}.plist").as_str()
        );
        let parent = p.parent().unwrap();
        assert_eq!(parent.file_name().unwrap(), "LaunchAgents");
        assert_eq!(parent.parent().unwrap().file_name().unwrap(), "Library");
    }

    #[test]
    fn systemd_unit_path_ends_under_systemd_user() {
        let p = default_systemd_unit_path().unwrap();
        assert_eq!(p.file_name().unwrap(), "repo-link.service");
        let parent = p.parent().unwrap();
        assert_eq!(parent.file_name().unwrap(), "user");
        assert_eq!(parent.parent().unwrap().file_name().unwrap(), "systemd");
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
            default_user: None,
            token_file_path: dir.path().join("does-not-exist"),
        };
        assert!(cfg.resolve_github_login().unwrap().is_none());
    }
}
