//! Windows Task Scheduler backend for `rl daemon`, driven through the
//! `schtasks` CLI. Compiled on every platform; invoked only when
//! `cfg!(target_os = "windows")` is true.
//!
//! Task Scheduler rather than a true Windows Service: `schtasks` is
//! argv-driven, so it satisfies the existing [`Launcher`] trait unchanged. A
//! service would need a control handler inside `rld` and a privileged install
//! step, and the only thing that buys is start-before-login.
//!
//! Idempotency contract: same shape as macOS and Linux.
//! - `install`: write the task XML only when bytes differ, then
//!   `/Create … /F` (replacing any existing registration) + start it if it
//!   isn't already up.
//! - `uninstall`: `/End` then `/Delete /F` (both tolerating "not registered")
//!   + delete the XML (tolerating "not present").
//!
//! `schtasks` reports an unregistered task name as a plain exit-1 failure
//! whose message is "The system cannot find the file specified" — the same
//! text a missing *file* produces. [`tolerate_missing_task`] narrows that
//! guess to the verbs where a missing task is a legal no-op, so `/Create`
//! failing on an unreachable XML path still surfaces as a real error.

use anyhow::{Result, anyhow};
use infra_config::DAEMON_LABEL;
use std::path::{Path, PathBuf};

use super::{InstallOutcome, StartStopOutcome, StatusOutcome, UninstallOutcome};
use crate::daemon::launcher::{LaunchOutcome, Launcher, require_success};
use crate::daemon::manifest::{path_to_string, write_if_changed, xml_escape_ascii};

pub(super) const PLATFORM: &str = "windows";

const TEMPLATE: &str = include_str!("templates/schtasks.xml");

pub(super) fn install(
    launcher: &dyn Launcher,
    binary_path: PathBuf,
    manifest_path: PathBuf,
    _log_path: PathBuf,
) -> Result<InstallOutcome> {
    // The task execs rld directly, which writes its own rotated JSON log, so
    // the _log_path arg is accepted for signature parity with macOS but unused.
    let desired = render_task_xml(&binary_path, &current_user()?);
    let manifest_changed = write_if_changed(&manifest_path, &desired)?;
    let manifest_str = path_to_string(&manifest_path)?;

    let create = launcher.run(&[
        "schtasks",
        "/Create",
        "/TN",
        DAEMON_LABEL,
        "/XML",
        &manifest_str,
        "/F",
    ])?;
    require_success("schtasks /Create", &create)?;
    let loaded = run_unless_already_running(launcher)?;

    Ok(InstallOutcome {
        label: DAEMON_LABEL,
        platform: PLATFORM,
        manifest_path,
        manifest_changed,
        loaded,
    })
}

pub(super) fn uninstall(
    launcher: &dyn Launcher,
    manifest_path: PathBuf,
) -> Result<UninstallOutcome> {
    // `/Delete` unregisters but does not promise to terminate a live
    // instance, and the uninstall contract on the other two platforms
    // (`launchctl bootout`, `systemctl disable --now`) does stop the process.
    end_task(launcher, "uninstall")?;

    let delete =
        tolerate_missing_task(launcher.run(&["schtasks", "/Delete", "/TN", DAEMON_LABEL, "/F"])?);
    let was_loaded = matches!(delete, LaunchOutcome::Success { .. });
    if let LaunchOutcome::Failed { code, stderr } = &delete {
        return Err(anyhow!("schtasks /Delete failed (exit {code}): {stderr}"));
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
    let unit_loaded = query_status(launcher)?.as_deref() == Some(RUNNING);

    let last_tick = super::read_last_tick(&last_tick_path)?;
    let wedged = unit_loaded && super::is_wedged(last_tick.as_ref());

    Ok(StatusOutcome {
        label: DAEMON_LABEL,
        platform: PLATFORM,
        unit_loaded,
        // Task Scheduler doesn't report the child pid through `schtasks`;
        // `unit_loaded` plus the heartbeat carry the liveness signal instead.
        unit_pid: None,
        last_tick,
        wedged,
        log_path,
    })
}

pub(super) fn start(launcher: &dyn Launcher) -> Result<StartStopOutcome> {
    // `/ENABLE` flips the persistent bit on and is safe regardless of run
    // state — the counterpart to `launchctl enable` / `systemctl enable`.
    let enable = launcher.run(&["schtasks", "/Change", "/TN", DAEMON_LABEL, "/ENABLE"])?;
    require_success("schtasks /Change /ENABLE", &enable)?;
    run_unless_already_running(launcher)?;

    Ok(StartStopOutcome {
        label: DAEMON_LABEL,
        platform: PLATFORM,
    })
}

pub(super) fn stop(launcher: &dyn Launcher) -> Result<StartStopOutcome> {
    end_task(launcher, "stop")?;

    // Disabling is the symmetric idempotent toggle. NotFound is tolerated
    // because stopping a never-installed daemon is a legal no-op.
    let disable = tolerate_missing_task(launcher.run(&[
        "schtasks",
        "/Change",
        "/TN",
        DAEMON_LABEL,
        "/DISABLE",
    ])?);
    if let LaunchOutcome::Failed { code, stderr } = &disable {
        return Err(anyhow!(
            "schtasks /Change /DISABLE failed (exit {code}): {stderr}"
        ));
    }

    Ok(StartStopOutcome {
        label: DAEMON_LABEL,
        platform: PLATFORM,
    })
}

/// Terminate the running instance, if any. `schtasks /End` fails the same way
/// whether the task was idle or the kill was genuinely refused, so a failure
/// is re-checked against the run state: still `Running` means the termination
/// really failed and must surface, because the caller has just been told the
/// daemon is gone while it keeps holding the db open.
fn end_task(launcher: &dyn Launcher, action: &str) -> Result<()> {
    let end = tolerate_missing_task(launcher.run(&["schtasks", "/End", "/TN", DAEMON_LABEL])?);
    let LaunchOutcome::Failed { code, stderr } = &end else {
        return Ok(());
    };
    if query_status(launcher)?.as_deref() == Some(RUNNING) {
        return Err(anyhow!(
            "schtasks /End failed during {action} and the task is still running (exit {code}): {stderr}"
        ));
    }
    Ok(())
}

/// Start the task unless it is already up. With
/// `MultipleInstancesPolicy=IgnoreNew` a start request against a running task
/// is rejected rather than ignored, so an unguarded `/Run` would make a
/// re-install or a repeat `start` fail on exactly the healthy case.
fn run_unless_already_running(launcher: &dyn Launcher) -> Result<bool> {
    if query_status(launcher)?.as_deref() == Some(RUNNING) {
        return Ok(true);
    }
    let run = launcher.run(&["schtasks", "/Run", "/TN", DAEMON_LABEL])?;
    require_success("schtasks /Run", &run)?;
    Ok(matches!(run, LaunchOutcome::Success { .. }))
}

fn query_status(launcher: &dyn Launcher) -> Result<Option<String>> {
    let probe = tolerate_missing_task(launcher.run(&[
        "schtasks",
        "/Query",
        "/TN",
        DAEMON_LABEL,
        "/FO",
        "LIST",
    ])?);
    match probe {
        LaunchOutcome::Success { stdout } => Ok(parse_schtasks_status(&stdout).map(String::from)),
        LaunchOutcome::NotFound => Ok(None),
        LaunchOutcome::Failed { code, stderr } => {
            Err(anyhow!("schtasks /Query failed (exit {code}): {stderr}"))
        }
    }
}

/// Reclassify "the task name isn't registered" as [`LaunchOutcome::NotFound`].
/// Applied only to the verbs where a missing task is a legal no-op — never to
/// `/Create`, whose identical message means the *XML file* was unreachable.
fn tolerate_missing_task(outcome: LaunchOutcome) -> LaunchOutcome {
    match outcome {
        LaunchOutcome::Failed { ref stderr, .. } if is_missing_task(stderr) => {
            LaunchOutcome::NotFound
        }
        other => other,
    }
}

fn is_missing_task(stderr: &str) -> bool {
    stderr.contains("cannot find the file specified")
        || stderr.contains("does not exist")
        || stderr.contains("The system cannot find the path specified")
}

/// Pull the run state out of `schtasks /Query /FO LIST`, whose output is a
/// block of `Label: value` lines:
///
/// ```text
/// Folder: \
/// HostName:                             DESKTOP-1
/// TaskName:                             \com.benediktms.repo-link
/// Next Run Time:                        N/A
/// Status:                               Running
/// ```
///
/// ponytail: label and value are both localised on a non-English Windows, so
/// this reads as "not loaded" there. Swap to PowerShell `Get-ScheduledTask`,
/// whose `State` is a locale-independent enum, if that matters.
fn parse_schtasks_status(stdout: &str) -> Option<&str> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("Status:"))
        .map(str::trim)
}

/// Task Scheduler's run state for a live instance. `unit_loaded` means "the
/// process is up" on the other two platforms — launchd reports a pid, systemd
/// reports `ActiveState=active` — so `Ready` (registered, nothing running)
/// must read as *not* loaded here too, otherwise a task whose binary never
/// launches is indistinguishable from a healthy one.
const RUNNING: &str = "Running";

fn render_task_xml(binary_path: &Path, user_id: &str) -> String {
    TEMPLATE
        .replace("{{LABEL}}", DAEMON_LABEL)
        .replace(
            "{{BINARY_PATH}}",
            &xml_escape_ascii(&binary_path.to_string_lossy()),
        )
        .replace("{{USER_ID}}", &xml_escape_ascii(user_id))
}

/// `DOMAIN\user` for the account the task runs as. Task Scheduler defaults a
/// principal without a `UserId` to password logon, which would make
/// `schtasks /Create` prompt; naming the account with `InteractiveToken`
/// avoids that and scopes the logon trigger to this user.
fn current_user() -> Result<String> {
    let user = std::env::var("USERNAME")
        .ok()
        .filter(|u| !u.is_empty())
        .ok_or_else(|| anyhow!("USERNAME is not set; cannot determine the account to run as"))?;
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => Ok(format!("{domain}\\{user}")),
        _ => Ok(user),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_schtasks_status_reads_the_status_line() {
        let stdout = "\
Folder: \\
HostName:                             DESKTOP-1
TaskName:                             \\com.benediktms.repo-link
Next Run Time:                        N/A
Status:                               Running
Logon Mode:                           Interactive only
";
        assert_eq!(parse_schtasks_status(stdout), Some("Running"));
    }

    #[test]
    fn parse_schtasks_status_returns_none_without_a_status_line() {
        assert_eq!(parse_schtasks_status("TaskName: \\other\n"), None);
    }

    /// Only a live instance counts as loaded — `Ready` is registered-but-not-
    /// running, which on the sibling platforms reports `unit_loaded: false`.
    #[test]
    fn only_the_running_state_counts_as_loaded() {
        for resting in ["Ready", "Queued", "Disabled", "Could not start"] {
            assert_ne!(resting, RUNNING);
        }
        assert_eq!(
            parse_schtasks_status("Status:   Running\n"),
            Some(RUNNING),
            "the parsed state must compare equal to the loaded sentinel"
        );
    }

    #[test]
    fn missing_task_failures_become_not_found_but_other_failures_do_not() {
        let missing = LaunchOutcome::Failed {
            code: 1,
            stderr: "ERROR: The system cannot find the file specified.".into(),
        };
        assert_eq!(tolerate_missing_task(missing), LaunchOutcome::NotFound);

        let denied = LaunchOutcome::Failed {
            code: 1,
            stderr: "ERROR: Access is denied.".into(),
        };
        assert_eq!(tolerate_missing_task(denied.clone()), denied);
    }

    #[test]
    fn render_task_xml_substitutes_all_placeholders() {
        let rendered = render_task_xml(
            std::path::Path::new(r"C:\Users\dev\.local\bin\rld.exe"),
            r"DESKTOP-1\dev",
        );
        assert!(rendered.contains(r"<Command>C:\Users\dev\.local\bin\rld.exe</Command>"));
        assert!(rendered.contains("<Arguments>--log-format=json</Arguments>"));
        assert!(rendered.contains(r"<UserId>DESKTOP-1\dev</UserId>"));
        assert!(rendered.contains("<URI>\\com.benediktms.repo-link</URI>"));
        assert!(!rendered.contains("{{"));
    }

    #[test]
    fn render_task_xml_escapes_markup_in_substituted_values() {
        let rendered = render_task_xml(std::path::Path::new(r"C:\R&D\rld.exe"), r"R&D\dev");
        assert!(rendered.contains(r"C:\R&amp;D\rld.exe"));
        assert!(rendered.contains(r"<UserId>R&amp;D\dev</UserId>"));
    }

    /// `<Command>` reaches CreateProcess without a shell, so a path holding a
    /// space, a `%` pair that names a real variable, or a cmd metacharacter
    /// must land in the XML byte-for-byte (bar XML escaping).
    #[test]
    fn render_task_xml_keeps_shell_metacharacters_literal() {
        let rendered = render_task_xml(
            std::path::Path::new(r"C:\Program Files\100%USERNAME%^&\rld.exe"),
            r"DESKTOP-1\dev",
        );
        assert!(
            rendered.contains(r"<Command>C:\Program Files\100%USERNAME%^&amp;\rld.exe</Command>"),
            "metacharacters were altered: {rendered}"
        );
    }

    /// `schtasks /Create /XML` rejects a definition whose bytes disagree with
    /// its declared encoding, so neither the template nor an interpolated
    /// path may put a non-ASCII byte in the file.
    #[test]
    fn render_task_xml_is_ascii_only_even_for_a_non_ascii_path() {
        let rendered = render_task_xml(
            std::path::Path::new(r"C:\Users\Jörg\.local\bin\rld.exe"),
            r"DESKTOP-1\Jörg",
        );
        assert!(
            rendered.is_ascii(),
            "non-ASCII byte in the rendered task XML: {rendered}"
        );
        assert!(rendered.contains(r"C:\Users\J&#xF6;rg\.local\bin\rld.exe"));
    }
}
