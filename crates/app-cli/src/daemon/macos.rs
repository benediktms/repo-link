//! macOS launchd backend for `rl daemon`. Compiled on every platform (we
//! never want a Linux clippy run to skip this file), but only invoked when
//! `cfg!(target_os = "macos")` is true.
//!
//! Idempotency contract (matches plan §"Idempotency contract"):
//! - `install`: write the plist only when bytes differ from what's on disk;
//!   then `bootout` (tolerating "not loaded") + `bootstrap` + `enable`. So
//!   re-running install picks up template changes without producing errors.
//! - `uninstall`: `bootout` (tolerating "not loaded") + delete the plist
//!   (tolerating "not present"). Safe on a fresh checkout.

use anyhow::{Result, anyhow};
use infra_config::{DAEMON_LABEL, default_launch_agent_path};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{InstallOutcome, StartStopOutcome, StatusOutcome, UninstallOutcome};
use crate::daemon::launcher::{LaunchOutcome, Launcher};
use crate::daemon::manifest::write_if_changed;

pub(super) const PLATFORM: &str = "macos";

const TEMPLATE: &str = include_str!("templates/launchd.plist");

pub(super) fn install(
    launcher: &dyn Launcher,
    binary_path: PathBuf,
    log_path: PathBuf,
) -> Result<InstallOutcome> {
    let manifest_path = default_launch_agent_path()?;
    let desired = render_plist(&binary_path, &log_path);
    let manifest_changed = write_if_changed(&manifest_path, &desired)?;

    let uid = current_uid()?;
    let domain = format!("gui/{uid}");
    let label_target = format!("{domain}/{DAEMON_LABEL}");
    let manifest_str = path_to_string(&manifest_path)?;

    // bootout first — tolerating NotFound — so a re-install always picks up
    // the new bytes even when an older copy is already loaded. A genuine
    // Failed (e.g. EPERM, "operation in progress") must surface; only the
    // "not currently loaded" case is benign.
    let bootout = launcher.run(&["launchctl", "bootout", &domain, &manifest_str])?;
    if let LaunchOutcome::Failed { code, stderr } = &bootout {
        return Err(anyhow!("launchctl bootout failed (exit {code}): {stderr}"));
    }
    let bootstrap = launcher.run(&["launchctl", "bootstrap", &domain, &manifest_str])?;
    require_success("launchctl bootstrap", &bootstrap)?;
    let enable = launcher.run(&["launchctl", "enable", &label_target])?;
    require_success("launchctl enable", &enable)?;

    Ok(InstallOutcome {
        label: DAEMON_LABEL,
        platform: PLATFORM,
        manifest_path,
        manifest_changed,
        loaded: matches!(bootstrap, LaunchOutcome::Success { .. }),
    })
}

pub(super) fn uninstall(launcher: &dyn Launcher) -> Result<UninstallOutcome> {
    let manifest_path = default_launch_agent_path()?;
    let uid = current_uid()?;
    let domain = format!("gui/{uid}");
    let manifest_str = path_to_string(&manifest_path)?;

    let bootout = launcher.run(&["launchctl", "bootout", &domain, &manifest_str])?;
    let was_loaded = matches!(bootout, LaunchOutcome::Success { .. });
    if let LaunchOutcome::Failed { code, stderr } = &bootout {
        return Err(anyhow!("launchctl bootout failed (exit {code}): {stderr}"));
    }

    let manifest_existed = manifest_path.exists();
    if manifest_existed {
        std::fs::remove_file(&manifest_path)?;
    }

    Ok(UninstallOutcome {
        label: DAEMON_LABEL,
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
    let uid = current_uid()?;
    let label_target = format!("gui/{uid}/{DAEMON_LABEL}");

    let probe = launcher.run(&["launchctl", "print", &label_target])?;
    let (unit_loaded, unit_pid) = match probe {
        LaunchOutcome::Success { stdout } => (true, parse_launchctl_pid(&stdout)),
        LaunchOutcome::NotFound => (false, None),
        LaunchOutcome::Failed { code, stderr } => {
            return Err(anyhow!("launchctl print failed (exit {code}): {stderr}"));
        }
    };

    let last_tick = super::read_last_tick(&last_tick_path)?;
    let wedged = unit_loaded && super::is_wedged(last_tick.as_ref());

    Ok(StatusOutcome {
        label: DAEMON_LABEL,
        platform: PLATFORM,
        unit_loaded,
        unit_pid,
        last_tick,
        wedged,
        log_path,
    })
}

/// Pull the `pid = NNN` line out of `launchctl print` output. The block
/// looks like:
///
/// ```text
/// com.benediktms.repo-link = {
///     active count = 1
///     pid = 12345
///     ...
/// }
/// ```
///
/// Returns `None` if the line is absent (daemon registered but not yet
/// running, e.g. during enable-without-RunAtLoad).
fn parse_launchctl_pid(stdout: &str) -> Option<u32> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("pid = ") {
            return rest.trim().parse::<u32>().ok();
        }
    }
    None
}

pub(super) fn start(launcher: &dyn Launcher) -> Result<StartStopOutcome> {
    let uid = current_uid()?;
    let domain = format!("gui/{uid}");
    let label_target = format!("{domain}/{DAEMON_LABEL}");
    let manifest_path = default_launch_agent_path()?;
    let manifest_str = path_to_string(&manifest_path)?;

    // Probe before bootstrap so we don't try to bootstrap an already-loaded
    // job (launchd would error out). Failed must surface — collapsing it to
    // `false` would mask real launchctl errors and then explode confusingly
    // inside the subsequent `bootstrap`.
    let probe = launcher.run(&["launchctl", "print", &label_target])?;
    let already_loaded = match probe {
        LaunchOutcome::Success { .. } => true,
        LaunchOutcome::NotFound => false,
        LaunchOutcome::Failed { code, stderr } => {
            return Err(anyhow!("launchctl print failed (exit {code}): {stderr}"));
        }
    };

    // enable is always safe — flips the persistent bit on regardless of
    // whether it was already on.
    let enable = launcher.run(&["launchctl", "enable", &label_target])?;
    require_success("launchctl enable", &enable)?;

    if !already_loaded {
        let bootstrap = launcher.run(&["launchctl", "bootstrap", &domain, &manifest_str])?;
        require_success("launchctl bootstrap", &bootstrap)?;
    }

    Ok(StartStopOutcome {
        label: DAEMON_LABEL,
        platform: PLATFORM,
    })
}

pub(super) fn stop(launcher: &dyn Launcher) -> Result<StartStopOutcome> {
    let uid = current_uid()?;
    let domain = format!("gui/{uid}");
    let label_target = format!("{domain}/{DAEMON_LABEL}");
    let manifest_path = default_launch_agent_path()?;
    let manifest_str = path_to_string(&manifest_path)?;

    // disable flips the persistent bit off. NotFound is tolerated because
    // stopping a never-installed daemon is a legal idempotent no-op.
    let disable = launcher.run(&["launchctl", "disable", &label_target])?;
    if let LaunchOutcome::Failed { code, stderr } = &disable {
        return Err(anyhow!("launchctl disable failed (exit {code}): {stderr}"));
    }

    // bootout actually stops the running process. Same idempotent treatment.
    let bootout = launcher.run(&["launchctl", "bootout", &domain, &manifest_str])?;
    if let LaunchOutcome::Failed { code, stderr } = &bootout {
        return Err(anyhow!("launchctl bootout failed (exit {code}): {stderr}"));
    }

    Ok(StartStopOutcome {
        label: DAEMON_LABEL,
        platform: PLATFORM,
    })
}

fn render_plist(binary_path: &Path, log_path: &Path) -> String {
    TEMPLATE
        .replace("{{LABEL}}", DAEMON_LABEL)
        .replace("{{BINARY_PATH}}", &binary_path.to_string_lossy())
        .replace("{{LOG_PATH}}", &log_path.to_string_lossy())
}

fn path_to_string(p: &Path) -> Result<String> {
    p.to_str()
        .map(String::from)
        .ok_or_else(|| anyhow!("path is not valid UTF-8: {p:?}"))
}

fn current_uid() -> Result<u32> {
    let out = Command::new("id").arg("-u").output()?;
    if !out.status.success() {
        return Err(anyhow!(
            "`id -u` exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let s = String::from_utf8(out.stdout)?;
    s.trim()
        .parse::<u32>()
        .map_err(|e| anyhow!("uid parse: {e}"))
}

fn require_success(action: &str, outcome: &LaunchOutcome) -> Result<()> {
    match outcome {
        LaunchOutcome::Success { .. } => Ok(()),
        LaunchOutcome::NotFound => Err(anyhow!(
            "{action}: launchd reported the unit is not registered"
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
    fn parse_launchctl_pid_finds_pid_in_typical_block() {
        let stdout = "\
com.benediktms.repo-link = {
    active count = 1
    path = /Users/x/Library/LaunchAgents/com.benediktms.repo-link.plist
    type = LaunchAgent
    state = running
    pid = 12345
    end_pid = -
}";
        assert_eq!(parse_launchctl_pid(stdout), Some(12345));
    }

    #[test]
    fn parse_launchctl_pid_returns_none_when_missing() {
        let stdout = "com.benediktms.repo-link = {\n    state = waiting\n}";
        assert_eq!(parse_launchctl_pid(stdout), None);
    }

    #[test]
    fn render_plist_substitutes_all_placeholders() {
        let rendered = render_plist(
            std::path::Path::new("/usr/local/bin/rld"),
            std::path::Path::new("/var/log/daemon.log"),
        );
        assert!(rendered.contains("<string>com.benediktms.repo-link</string>"));
        assert!(rendered.contains("<string>/usr/local/bin/rld</string>"));
        assert!(rendered.contains("<string>/var/log/daemon.log</string>"));
        assert!(!rendered.contains("{{")); // no placeholders left
    }
}
