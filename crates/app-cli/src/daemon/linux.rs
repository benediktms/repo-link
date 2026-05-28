//! Linux systemd `--user` backend for `rl daemon`. Compiled on every
//! platform; invoked only when `cfg!(target_os = "linux")` is true.
//!
//! Idempotency contract: same shape as macOS.
//! - `install`: write the unit file only when bytes differ, then
//!   `daemon-reload` + `enable --now`. `enable --now` is itself idempotent:
//!   it starts the unit if stopped and leaves it running if already up.
//! - `uninstall`: `disable --now` (tolerating "not loaded") + delete the
//!   unit file (tolerating "not present").

use anyhow::{Result, anyhow};
use infra_config::default_systemd_unit_path;
use std::path::{Path, PathBuf};

use super::{InstallOutcome, StartStopOutcome, StatusOutcome, UninstallOutcome};
use crate::daemon::launcher::{LaunchOutcome, Launcher};
use crate::daemon::manifest::write_if_changed;

pub(super) const PLATFORM: &str = "linux";
pub(super) const UNIT_NAME: &str = "repo-link.service";

const TEMPLATE: &str = include_str!("templates/systemd.service");

pub(super) fn install(
    launcher: &dyn Launcher,
    binary_path: PathBuf,
    _log_path: PathBuf,
) -> Result<InstallOutcome> {
    // systemd captures stdout/stderr automatically (journald), so the
    // _log_path arg is accepted for signature parity with macOS but unused.
    let manifest_path = default_systemd_unit_path()?;
    let desired = render_unit(&binary_path);
    let manifest_changed = write_if_changed(&manifest_path, &desired)?;

    let reload = launcher.run(&["systemctl", "--user", "daemon-reload"])?;
    require_success("systemctl --user daemon-reload", &reload)?;
    let enable = launcher.run(&["systemctl", "--user", "enable", "--now", UNIT_NAME])?;
    require_success(
        &format!("systemctl --user enable --now {UNIT_NAME}"),
        &enable,
    )?;

    Ok(InstallOutcome {
        label: infra_config::DAEMON_LABEL,
        platform: PLATFORM,
        manifest_path,
        manifest_changed,
        loaded: matches!(enable, LaunchOutcome::Success { .. }),
    })
}

pub(super) fn uninstall(launcher: &dyn Launcher) -> Result<UninstallOutcome> {
    let manifest_path = default_systemd_unit_path()?;

    let disable = launcher.run(&["systemctl", "--user", "disable", "--now", UNIT_NAME])?;
    let was_loaded = matches!(disable, LaunchOutcome::Success { .. });
    if let LaunchOutcome::Failed { code, stderr } = &disable {
        return Err(anyhow!(
            "systemctl --user disable --now {UNIT_NAME} failed (exit {code}): {stderr}"
        ));
    }

    let manifest_existed = manifest_path.exists();
    if manifest_existed {
        std::fs::remove_file(&manifest_path)?;
        // `daemon-reload` so systemd forgets the removed unit immediately.
        // Failure here is non-fatal — the unit file is already gone.
        let _ = launcher.run(&["systemctl", "--user", "daemon-reload"])?;
    }

    Ok(UninstallOutcome {
        label: infra_config::DAEMON_LABEL,
        platform: PLATFORM,
        manifest_path,
        manifest_existed,
        was_loaded,
    })
}

pub(super) fn status(
    launcher: &dyn Launcher,
    last_tick_path: PathBuf,
    log_path: PathBuf,
) -> Result<StatusOutcome> {
    let probe = launcher.run(&[
        "systemctl",
        "--user",
        "show",
        "-p",
        "MainPID",
        "-p",
        "ActiveState",
        UNIT_NAME,
    ])?;
    let (unit_loaded, unit_pid) = match probe {
        LaunchOutcome::Success { stdout } => parse_systemctl_show(&stdout),
        LaunchOutcome::NotFound => (false, None),
        LaunchOutcome::Failed { code, stderr } => {
            return Err(anyhow!(
                "systemctl --user show {UNIT_NAME} failed (exit {code}): {stderr}"
            ));
        }
    };

    let last_tick = super::read_last_tick(&last_tick_path)?;
    let wedged = unit_loaded && super::is_wedged(last_tick.as_ref());

    Ok(StatusOutcome {
        label: infra_config::DAEMON_LABEL,
        platform: PLATFORM,
        unit_loaded,
        unit_pid,
        last_tick,
        wedged,
        log_path,
    })
}

/// `systemctl show -p MainPID -p ActiveState` emits two `KEY=VALUE` lines.
/// We treat the unit as loaded when `ActiveState` is one of the running
/// states. `MainPID=0` (the systemd convention for "no process") collapses
/// to `None` so the JSON outcome doesn't lie about a dead daemon's pid.
fn parse_systemctl_show(stdout: &str) -> (bool, Option<u32>) {
    let mut pid: Option<u32> = None;
    let mut active: Option<&str> = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("MainPID=") {
            pid = rest.trim().parse::<u32>().ok().filter(|p| *p > 0);
        } else if let Some(rest) = line.strip_prefix("ActiveState=") {
            active = Some(rest.trim());
        }
    }
    let loaded = matches!(active, Some("active" | "activating" | "reloading"));
    (loaded, pid)
}

pub(super) fn start(launcher: &dyn Launcher) -> Result<StartStopOutcome> {
    // `enable --now` is idempotent: it sets the persistent "should be loaded"
    // bit and starts the unit if not already running.
    let enable = launcher.run(&["systemctl", "--user", "enable", "--now", UNIT_NAME])?;
    require_success(
        &format!("systemctl --user enable --now {UNIT_NAME}"),
        &enable,
    )?;
    Ok(StartStopOutcome {
        label: infra_config::DAEMON_LABEL,
        platform: PLATFORM,
    })
}

pub(super) fn stop(launcher: &dyn Launcher) -> Result<StartStopOutcome> {
    // `disable --now` is the symmetric idempotent toggle: clears the
    // persistent bit and stops the unit if running. NotFound is tolerated
    // because stopping a never-installed daemon is a legal no-op.
    let disable = launcher.run(&["systemctl", "--user", "disable", "--now", UNIT_NAME])?;
    if let LaunchOutcome::Failed { code, stderr } = &disable {
        return Err(anyhow!(
            "systemctl --user disable --now {UNIT_NAME} failed (exit {code}): {stderr}"
        ));
    }
    Ok(StartStopOutcome {
        label: infra_config::DAEMON_LABEL,
        platform: PLATFORM,
    })
}

fn render_unit(binary_path: &Path) -> String {
    TEMPLATE.replace("{{BINARY_PATH}}", &binary_path.to_string_lossy())
}

fn require_success(action: &str, outcome: &LaunchOutcome) -> Result<()> {
    match outcome {
        LaunchOutcome::Success { .. } => Ok(()),
        LaunchOutcome::NotFound => Err(anyhow!(
            "{action}: systemd reported the unit is not registered"
        )),
        LaunchOutcome::Failed { code, stderr } => {
            Err(anyhow!("{action} failed (exit {code}): {stderr}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_systemctl_show_active_with_pid() {
        let stdout = "MainPID=4242\nActiveState=active\n";
        assert_eq!(parse_systemctl_show(stdout), (true, Some(4242)));
    }

    #[test]
    fn parse_systemctl_show_inactive_zero_pid() {
        let stdout = "MainPID=0\nActiveState=inactive\n";
        assert_eq!(parse_systemctl_show(stdout), (false, None));
    }

    #[test]
    fn parse_systemctl_show_treats_activating_as_loaded() {
        let stdout = "MainPID=0\nActiveState=activating\n";
        let (loaded, pid) = parse_systemctl_show(stdout);
        assert!(loaded);
        assert_eq!(pid, None);
    }

    #[test]
    fn parse_systemctl_show_failed_is_not_loaded() {
        let stdout = "MainPID=0\nActiveState=failed\n";
        assert_eq!(parse_systemctl_show(stdout), (false, None));
    }

    #[test]
    fn render_unit_substitutes_binary_path() {
        let rendered = render_unit(std::path::Path::new("/opt/repo-link/rld"));
        assert!(rendered.contains("ExecStart=/opt/repo-link/rld --log-format=json"));
        assert!(!rendered.contains("{{"));
    }
}
