use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TickReport {
    pub workspaces: usize,
    pub repos_checked: usize,
    pub worktrees_checked: usize,
    pub marked_missing: usize,
    pub pruned: usize,
    pub pushed: usize,
    pub push_failures: Vec<String>,
}

/// Heartbeat artefact written atomically at the end of every `tick_once`.
/// Consumed by `rl daemon status` to surface "running but wedged" — a
/// daemon whose unit is loaded but whose `tick_at` is older than expected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastTick {
    pub tick_at: chrono::DateTime<chrono::Utc>,
    pub interval_secs: u64,
    pub report: TickReport,
}

/// Atomic write: serialise to a temp file in the destination directory,
/// then `rename` over the target. Same-directory rename is atomic on every
/// POSIX filesystem, so readers never see a half-written heartbeat.
pub(crate) fn write_last_tick_atomic(
    path: &std::path::Path,
    interval_secs: u64,
    report: &TickReport,
) -> std::io::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let last_tick = LastTick {
        tick_at: chrono::Utc::now(),
        interval_secs,
        report: report.clone(),
    };
    let bytes =
        serde_json::to_vec_pretty(&last_tick).map_err(|e| std::io::Error::other(e.to_string()))?;
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(&bytes)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
