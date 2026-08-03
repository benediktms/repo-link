//! Error types for config resolution and token-file loading.

use std::path::PathBuf;

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
/// a token that's exposed via `ls -la` or backups. `InsecureAcl` is the
/// Windows counterpart — any principal in the file's DACL other than the
/// current user, including the SYSTEM and Administrators entries a file
/// inherits from `%APPDATA%`. `Acl` reports the DACL query itself failing,
/// which is an operational fault rather than a verdict about the file.
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
    #[error(
        "token file {p} grants access to {principal}, not just your account.\n\
         Restrict it by re-running: rl gh auth",
        p = path.display()
    )]
    InsecureAcl { path: PathBuf, principal: String },
    #[error(
        "token file {p} is owned by {owner}, not by you — its owner can rewrite \
         the permissions at will.\nDelete it and re-run: rl gh auth",
        p = path.display()
    )]
    ForeignOwner { path: PathBuf, owner: String },
    #[error("token file {p}: {message}", p = path.display())]
    Acl { path: PathBuf, message: String },
}
