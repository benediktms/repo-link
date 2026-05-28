//! `rl gh` dispatch — token persistence, `gh` CLI passthrough, and the
//! platform-specific token-file writers.

use anyhow::{Result, anyhow};
use infra_config::RepoLinkConfig;

use crate::cli::GhCmd;
use crate::services::build_github_provider;

pub(crate) async fn gh_dispatch(cmd: GhCmd, cfg: &RepoLinkConfig) -> Result<()> {
    match cmd {
        GhCmd::Auth { token, force } => gh_auth(token, force, cfg).await,
    }
}

async fn gh_auth(token: Option<String>, force: bool, cfg: &RepoLinkConfig) -> Result<()> {
    // Guard against overwriting an existing token file without explicit consent.
    if cfg.token_file_path.exists() && !force {
        eprint!(
            "token file {} already exists. Overwrite? [y/N]: ",
            cfg.token_file_path.display()
        );
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| anyhow!("failed to read confirmation: {e}"))?;
        let answer = line.trim().to_lowercase();
        if answer != "y" && answer != "yes" {
            return Err(anyhow!("aborted; pass --force to overwrite"));
        }
    }

    // Resolve the token: explicit --token wins, then a best-effort fetch from
    // the official `gh` CLI (so `gh auth login` users don't need to copy a
    // PAT by hand), and finally fall back to a hidden interactive prompt.
    let raw_token = match token {
        Some(t) => t,
        None => match try_gh_cli_token() {
            Some(t) => {
                eprintln!("note: using token from `gh auth token`.");
                t
            }
            None => rpassword::prompt_password("Paste GitHub token (input hidden): ")
                .map_err(|e| anyhow!("failed to read token: {e}"))?,
        },
    };
    let trimmed = raw_token.trim().to_string();
    if trimmed.is_empty() {
        return Err(anyhow!("token must not be empty"));
    }

    // Best-effort: fetch the authenticated user's login and cache it next to
    // the token. A network failure / invalid token shouldn't block the auth
    // flow — the token still gets persisted and downstream verbs that need
    // the login (e.g. `task claim`) report a clear "re-run rl gh auth" hint.
    let login = match build_github_provider(&trimmed, cfg) {
        Ok(provider) => match provider.current_user_login().await {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!(
                    "note: token saved, but couldn't fetch GitHub login ({e}). \
                     Re-run `rl gh auth` once connectivity / the token is good \
                     so commands like `rl task claim` can resolve your handle."
                );
                None
            }
        },
        Err(e) => {
            eprintln!("note: token saved, but provider init failed ({e}).");
            None
        }
    };

    write_token_file(&cfg.token_file_path, &trimmed, login.as_deref())?;

    let path_str = cfg
        .token_file_path
        .canonicalize()
        .unwrap_or_else(|_| cfg.token_file_path.clone())
        .display()
        .to_string();

    #[cfg(unix)]
    let mode_value = "0600";
    #[cfg(not(unix))]
    let mode_value = "unrestricted";

    let mut payload = serde_json::json!({ "file": path_str, "mode": mode_value });
    if let Some(l) = login.as_deref() {
        payload["login"] = serde_json::Value::String(l.to_string());
    }
    println!("{payload}");

    Ok(())
}

/// Best-effort: read the token cached by the official `gh` CLI. Any failure
/// path (gh not on PATH, not logged in, non-zero exit, empty stdout) falls
/// through to the next source. `gh auth token` is fast in practice; we don't
/// add an explicit timeout because a user can ctrl-c and `gh` itself doesn't
/// hang on cached credentials.
fn try_gh_cli_token() -> Option<String> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let trimmed = s.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Render the two-line file body: token on line 1, optional cached GitHub
/// login on line 2. Single-line files (login = None) keep parsing through
/// `infra_config::resolve_github_token` exactly as before — the second line
/// is purely additive.
fn render_token_file_body(token: &str, login: Option<&str>) -> String {
    match login {
        Some(l) => format!("{token}\n{l}\n"),
        None => token.to_string(),
    }
}

#[cfg(unix)]
fn write_token_file(path: &std::path::Path, token: &str, login: Option<&str>) -> Result<()> {
    use std::fs::DirBuilder;
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    // Ensure parent directory exists with mode 0o700.
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|e| anyhow!("failed to create config dir: {e}"))?;
    }

    // Create or truncate with mode 0o600. The `mode` on `OpenOptions` only
    // applies at creation time; `set_permissions` below re-asserts 0o600 so
    // an existing file that was loosened gets tightened back on overwrite.
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| anyhow!("failed to open token file: {e}"))?;
    file.write_all(render_token_file_body(token, login).as_bytes())
        .map_err(|e| anyhow!("failed to write token: {e}"))?;
    drop(file);

    // Re-assert permissions in case the file pre-existed with looser bits.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .map_err(|e| anyhow!("failed to set permissions: {e}"))?;

    Ok(())
}

#[cfg(not(unix))]
fn write_token_file(path: &std::path::Path, token: &str, login: Option<&str>) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow!("failed to create config dir: {e}"))?;
        }
    }
    std::fs::write(path, render_token_file_body(token, login))
        .map_err(|e| anyhow!("failed to write token file: {e}"))?;
    Ok(())
}
