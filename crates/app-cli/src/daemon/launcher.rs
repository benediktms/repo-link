//! Abstraction over the platform process-control tool — `launchctl` on
//! macOS, `systemctl --user` on Linux. The [`Launcher`] trait exists so the
//! public CLI integration tests can drive `rl daemon install` without
//! actually touching the user agent on the test runner: tests set
//! `REPO_LINK_LAUNCHER=fake`, which routes every call into [`FakeLauncher`]
//! and records the argv into `$REPO_LINK_LAUNCHER_LOG` for replay assertion.

use std::path::PathBuf;
use std::process::Command;

/// Result of a single `launchctl` / `systemctl` invocation. We separate
/// `NotFound` from `Failed` so idempotent bootout/disable can keep going
/// on a fresh checkout where the unit was never registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchOutcome {
    /// Carries stdout so `status`'s callers can parse out the PID / active
    /// state from `launchctl print` / `systemctl show`.
    Success { stdout: String },
    NotFound,
    Failed { code: i32, stderr: String },
}

pub trait Launcher: Send + Sync {
    fn run(&self, argv: &[&str]) -> std::io::Result<LaunchOutcome>;
}

pub struct RealLauncher;

impl Launcher for RealLauncher {
    fn run(&self, argv: &[&str]) -> std::io::Result<LaunchOutcome> {
        let (prog, args) = argv
            .split_first()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty argv"))?;
        let out = Command::new(prog).args(args).output()?;
        if out.status.success() {
            return Ok(LaunchOutcome::Success {
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            });
        }
        let code = out.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

        // launchctl exits 113 with "Could not find service" when bootout/print
        // run against a label that isn't loaded. systemctl exits 5 (or 4 in
        // older versions) for an unknown unit. Both are "nothing to do, keep
        // going" from an idempotent-uninstall perspective.
        let not_found = code == 113
            || code == 5
            || code == 4
            || stderr.contains("Could not find service")
            || stderr.contains("not loaded")
            || stderr.contains("not found")
            || stderr.contains("does not exist");
        if not_found {
            return Ok(LaunchOutcome::NotFound);
        }
        Ok(LaunchOutcome::Failed { code, stderr })
    }
}

/// Records each argv into `$REPO_LINK_LAUNCHER_LOG` as a single JSON line,
/// then reports the configured `response`. Tests `cat` the log to assert
/// the exact sequence the platform helpers issued (bootout → bootstrap →
/// enable, etc.). Activated via `REPO_LINK_LAUNCHER` (see [`current_launcher`]).
pub struct FakeLauncher {
    log_path: PathBuf,
    response: LaunchOutcome,
}

impl FakeLauncher {
    pub fn new(response: LaunchOutcome) -> Self {
        let log_path = std::env::var("REPO_LINK_LAUNCHER_LOG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("repo-link-launcher.log"));
        Self { log_path, response }
    }
}

impl Launcher for FakeLauncher {
    fn run(&self, argv: &[&str]) -> std::io::Result<LaunchOutcome> {
        use std::io::Write as _;

        let line = serde_json::to_string(argv).map_err(std::io::Error::other)?;
        if let Some(parent) = self.log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        writeln!(f, "{line}")?;
        Ok(self.response.clone())
    }
}

/// Pick the launcher implementation based on `REPO_LINK_LAUNCHER`. The two
/// fake modes let tests drive both the "unit is loaded" and "unit is not
/// loaded" status branches without invoking real `launchctl`/`systemctl`.
///
/// - unset / anything else → [`RealLauncher`] (production)
/// - `fake` → [`FakeLauncher`] returning `Success { stdout: "" }`
/// - `fake_not_found` → [`FakeLauncher`] returning `NotFound`
///
/// All of these are undocumented on purpose; they live in the integration
/// test harness only.
pub fn current_launcher() -> Box<dyn Launcher> {
    match std::env::var("REPO_LINK_LAUNCHER").as_deref() {
        Ok("fake") => Box::new(FakeLauncher::new(LaunchOutcome::Success {
            // Carries both macOS (`pid = N`) and Linux (`MainPID=N` +
            // `ActiveState=active`) patterns so the cross-platform status
            // tests see a consistent "loaded with pid 12345" view.
            stdout: "MainPID=12345\nActiveState=active\npid = 12345\n".to_string(),
        })),
        Ok("fake_not_found") => Box::new(FakeLauncher::new(LaunchOutcome::NotFound)),
        _ => Box::new(RealLauncher),
    }
}
