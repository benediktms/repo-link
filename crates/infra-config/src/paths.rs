//! Platform-derived default paths + the daemon label.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

use crate::error::ConfigError;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
