//! `rl daemon` — install / uninstall / status / start / stop the platform
//! unit that keeps `rld` running across reboots. One source of truth per
//! platform lives in [`macos`] / [`linux`]; the dispatcher in [`dispatch`]
//! picks via `cfg!(target_os = "macos")`. Both modules compile on every
//! platform so a Linux CI run still type-checks the macOS path.

use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::Subcommand;
use infra_config::RepoLinkConfig;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

mod launcher;
mod linux;
mod macos;
mod manifest;

use launcher::current_launcher;

#[derive(Subcommand, Debug)]
pub enum DaemonCmd {
    /// Write the platform unit file and load it (idempotent).
    Install,
    /// Remove the unit file and unload it (idempotent).
    Uninstall,
    /// Report whether the unit is loaded, plus the last tick.
    Status,
    /// Toggle the persistent unit on (idempotent).
    Start,
    /// Toggle the persistent unit off (idempotent).
    Stop,
}

#[derive(Debug, Serialize)]
pub struct InstallOutcome {
    pub label: &'static str,
    pub platform: &'static str,
    pub manifest_path: PathBuf,
    /// `true` iff the manifest bytes on disk changed during this call.
    /// `install` writes only when the desired content differs from what's
    /// already there, so re-running `install` on a fresh build returns
    /// `false` here.
    pub manifest_changed: bool,
    pub loaded: bool,
}

#[derive(Debug, Serialize)]
pub struct UninstallOutcome {
    pub label: &'static str,
    pub platform: &'static str,
    pub manifest_path: PathBuf,
    pub manifest_existed: bool,
    pub was_loaded: bool,
}

#[derive(Debug, Serialize)]
pub struct StatusOutcome {
    pub label: &'static str,
    pub platform: &'static str,
    pub unit_loaded: bool,
    pub unit_pid: Option<u32>,
    pub last_tick: Option<LastTickDto>,
    pub wedged: bool,
    pub log_path: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct StartStopOutcome {
    pub label: &'static str,
    pub platform: &'static str,
}

/// CLI-side view of the heartbeat file. The schema is the contract; this
/// struct does not depend on `app-daemon::LastTick`. If their fields drift,
/// the integration test `daemon_status_reads_last_tick_when_present` fails.
#[derive(Debug, Deserialize, Serialize)]
pub struct LastTickDto {
    pub tick_at: DateTime<Utc>,
    pub interval_secs: u64,
    pub report: serde_json::Value,
}

pub async fn dispatch(cmd: DaemonCmd, cfg: &RepoLinkConfig) -> Result<()> {
    let launcher = current_launcher();
    let outcome_json = match cmd {
        DaemonCmd::Install => {
            let bin = installed_rld_path()?;
            let log_path = daemon_install_log_path(cfg)?;
            let out = if cfg!(target_os = "macos") {
                macos::install(launcher.as_ref(), bin, log_path)?
            } else if cfg!(target_os = "linux") {
                linux::install(launcher.as_ref(), bin, log_path)?
            } else {
                return Err(unsupported_platform());
            };
            serde_json::to_string_pretty(&out)?
        }
        DaemonCmd::Uninstall => {
            let out = if cfg!(target_os = "macos") {
                macos::uninstall(launcher.as_ref())?
            } else if cfg!(target_os = "linux") {
                linux::uninstall(launcher.as_ref())?
            } else {
                return Err(unsupported_platform());
            };
            serde_json::to_string_pretty(&out)?
        }
        DaemonCmd::Status => {
            let last_tick = last_tick_path(cfg)?;
            let log_path = daemon_status_log_path(cfg)?;
            let out = if cfg!(target_os = "macos") {
                macos::status(launcher.as_ref(), last_tick, log_path)?
            } else if cfg!(target_os = "linux") {
                linux::status(launcher.as_ref(), last_tick, log_path)?
            } else {
                return Err(unsupported_platform());
            };
            serde_json::to_string_pretty(&out)?
        }
        DaemonCmd::Start => {
            let out = if cfg!(target_os = "macos") {
                macos::start(launcher.as_ref())?
            } else if cfg!(target_os = "linux") {
                linux::start(launcher.as_ref())?
            } else {
                return Err(unsupported_platform());
            };
            serde_json::to_string_pretty(&out)?
        }
        DaemonCmd::Stop => {
            let out = if cfg!(target_os = "macos") {
                macos::stop(launcher.as_ref())?
            } else if cfg!(target_os = "linux") {
                linux::stop(launcher.as_ref())?
            } else {
                return Err(unsupported_platform());
            };
            serde_json::to_string_pretty(&out)?
        }
    };
    println!("{outcome_json}");
    Ok(())
}

/// Surface a clear error when `rl daemon` is invoked on something that
/// isn't macOS or Linux. Better than letting the call fall through into
/// `systemctl`/`launchctl` and exploding with a confusing "command not
/// found" further down the stack.
fn unsupported_platform() -> anyhow::Error {
    anyhow::anyhow!(
        "rl daemon is only supported on macOS (launchd) and Linux (systemd --user)"
    )
}

/// Absolute path to the `rld` binary the unit will launch. `just install`
/// is the canonical install method, which symlinks `~/.local/bin/rld` to
/// `target/release/rld` — so that's the single supported lookup. Tests set
/// `REPO_LINK_RLD_PATH` to point at a tempdir without touching `$HOME`.
fn installed_rld_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("REPO_LINK_RLD_PATH") {
        return Ok(PathBuf::from(p));
    }
    let canonical = home_dir()?.join(".local").join("bin").join("rld");
    if canonical.exists() {
        return Ok(canonical);
    }
    Err(anyhow::anyhow!(
        "`rld` not found at ~/.local/bin/rld; run `just install` first"
    ))
}

fn home_dir() -> Result<PathBuf> {
    use directories::BaseDirs;
    BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))
}

/// Heartbeat file path — co-located with the SQLite db so `--db` relocates
/// everything together (matches what the daemon writes).
fn last_tick_path(cfg: &RepoLinkConfig) -> Result<PathBuf> {
    Ok(db_parent(cfg)?.join("last_tick.json"))
}

/// Path the launchd plist redirects stdout/stderr to. Stays at the
/// fixed `daemon.log` name (no date suffix) because it's the catch-all
/// for output that happens *before* tracing-appender is initialised —
/// panics, env-var failures, etc. Distinct from the daily-rotated
/// segments tracing-appender writes (see `daemon_status_log_path`).
fn daemon_install_log_path(cfg: &RepoLinkConfig) -> Result<PathBuf> {
    Ok(db_parent(cfg)?.join("daemon.log"))
}

/// Path reported by `rl daemon status.log_path` — the file users
/// actually want to tail to see daemon events. With daily rotation in
/// `app-daemon::logging`, this is the newest `daemon.YYYY-MM-DD.log`
/// segment in the log directory. Falls back to `daemon.log` when no
/// segments exist yet (e.g., fresh install or rld has never logged) so
/// callers always get a usable path.
fn daemon_status_log_path(cfg: &RepoLinkConfig) -> Result<PathBuf> {
    let dir = db_parent(cfg)?;
    Ok(newest_log_segment(dir).unwrap_or_else(|| dir.join("daemon.log")))
}

/// Glob `<dir>` for tracing-appender's daily segments named
/// `daemon.YYYY-MM-DD.log` and return the lexicographically-largest
/// one — which, since ISO-8601 dates sort the same as time, is the
/// newest. Returns `None` if no matching segments exist.
fn newest_log_segment(dir: &std::path::Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut latest: Option<(String, PathBuf)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Strict shape: `daemon.<10-char ISO date>.log`. This excludes
        // the legacy `daemon.log` (the plist's panic catcher) — that
        // file is the fallback path, not a segment.
        let Some(stripped) = name_str
            .strip_prefix("daemon.")
            .and_then(|s| s.strip_suffix(".log"))
        else {
            continue;
        };
        if stripped.len() != 10 {
            continue;
        }
        let path = entry.path();
        match &latest {
            Some((existing, _)) if existing.as_str() >= stripped => {}
            _ => latest = Some((stripped.to_string(), path)),
        }
    }
    latest.map(|(_, path)| path)
}

fn db_parent(cfg: &RepoLinkConfig) -> Result<&std::path::Path> {
    cfg.database_path.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "database path has no parent directory: {}",
            cfg.database_path.display()
        )
    })
}

/// Read `last_tick.json` if it exists. Missing file is normal (the daemon
/// hasn't ticked yet, or status is being run on a brand-new install) and
/// returns `Ok(None)`; only deserialization errors propagate.
pub(super) fn read_last_tick(path: &std::path::Path) -> Result<Option<LastTickDto>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(serde_json::from_str(&s)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!(
            "failed to read {}: {e}",
            path.display()
        )),
    }
}

/// Wedged := the unit is loaded but its last tick is older than `2 ×
/// interval_secs`. Returns `false` when there's no heartbeat to compare
/// against — `unit_loaded` carries that signal independently.
pub(super) fn is_wedged(last_tick: Option<&LastTickDto>) -> bool {
    match last_tick {
        Some(lt) => {
            let age = Utc::now() - lt.tick_at;
            let threshold = chrono::Duration::seconds(lt.interval_secs.saturating_mul(2) as i64);
            age > threshold
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last_tick_at(when: DateTime<Utc>, interval_secs: u64) -> LastTickDto {
        LastTickDto {
            tick_at: when,
            interval_secs,
            report: serde_json::json!({}),
        }
    }

    #[test]
    fn is_wedged_false_when_no_tick() {
        assert!(!is_wedged(None));
    }

    #[test]
    fn is_wedged_false_when_tick_is_fresh() {
        let lt = last_tick_at(Utc::now(), 60);
        assert!(!is_wedged(Some(&lt)));
    }

    #[test]
    fn is_wedged_true_when_tick_exceeds_two_intervals() {
        // interval=60s → threshold=120s. Tick from 200s ago → wedged.
        let lt = last_tick_at(Utc::now() - chrono::Duration::seconds(200), 60);
        assert!(is_wedged(Some(&lt)));
    }

    #[test]
    fn is_wedged_false_at_exactly_one_interval() {
        let lt = last_tick_at(Utc::now() - chrono::Duration::seconds(60), 60);
        assert!(!is_wedged(Some(&lt)));
    }

    #[test]
    fn newest_log_segment_returns_none_when_dir_is_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(newest_log_segment(dir.path()), None);
    }

    #[test]
    fn newest_log_segment_ignores_the_legacy_undated_daemon_log() {
        // `daemon.log` is the launchd panic-catcher and must not be
        // mistaken for a segment — it has no date in the middle, so
        // strict shape rejects it.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("daemon.log"), "").unwrap();
        assert_eq!(newest_log_segment(dir.path()), None);
    }

    #[test]
    fn newest_log_segment_picks_the_lexicographically_largest_dated_segment() {
        // ISO-8601 dates sort the same as time, so lex-max == newest.
        let dir = tempfile::TempDir::new().unwrap();
        for date in ["2026-05-24", "2026-05-26", "2026-05-25"] {
            std::fs::write(dir.path().join(format!("daemon.{date}.log")), "").unwrap();
        }
        let newest = newest_log_segment(dir.path()).unwrap();
        assert_eq!(newest.file_name().unwrap(), "daemon.2026-05-26.log");
    }

    #[test]
    fn newest_log_segment_ignores_unrelated_files_in_the_dir() {
        // The log dir is also home to `last_tick.json`, the legacy
        // `daemon.log`, and (possibly) tracing-appender lock files —
        // none of those should be returned.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("last_tick.json"), "{}").unwrap();
        std::fs::write(dir.path().join("daemon.log"), "").unwrap();
        std::fs::write(dir.path().join("daemon.2026-05-26.log"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        let newest = newest_log_segment(dir.path()).unwrap();
        assert_eq!(newest.file_name().unwrap(), "daemon.2026-05-26.log");
    }
}
