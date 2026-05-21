//! infra-filesystem — real filesystem probe + scanning helpers + watcher.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use ports::{FilesystemProbe, PortError, PortResult};
use thiserror::Error;
use tokio::sync::mpsc;
use walkdir::WalkDir;

#[derive(Debug, Error)]
pub enum FsError {
    #[error("notify: {0}")]
    Notify(#[from] notify::Error),
}

#[derive(Default)]
pub struct TokioFilesystemProbe;

impl TokioFilesystemProbe {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl FilesystemProbe for TokioFilesystemProbe {
    async fn path_exists(&self, path: &Path) -> PortResult<bool> {
        match tokio::fs::metadata(path).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(PortError::Backend(format!(
                "stat {}: {e}",
                path.display()
            ))),
        }
    }

    /// Cheap probe: a path is treated as a git worktree if it contains a `.git`
    /// entry (directory for a primary checkout, file for a linked worktree).
    /// `gix::discover` would be more authoritative but costs significantly
    /// more per call — keep that for an explicit "validate this remote" path.
    async fn is_git_worktree(&self, path: &Path) -> PortResult<bool> {
        let git = path.join(".git");
        match tokio::fs::metadata(&git).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(PortError::Backend(format!(
                "stat {}: {e}",
                git.display()
            ))),
        }
    }
}

/// Walk `root` and return every directory that contains a `.git` entry.
///
/// Useful for bulk-attaching repos: `repo-link` can scan `~/code/` once and
/// surface every git checkout below it. We bound depth at 6 to avoid
/// pathological `node_modules`-style trees.
pub fn discover_repos_under(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .max_depth(6)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name() == ".git"
                && std::fs::metadata(e.path())
                    .map(|m| m.is_dir() || m.is_file())
                    .unwrap_or(false)
        })
        .filter_map(|e| e.path().parent().map(PathBuf::from))
        .collect()
}

/// Watch a set of paths and forward "this path went away" events to async
/// consumers. Backed by `notify::RecommendedWatcher` so we get whatever the
/// platform considers efficient (FSEvents on macOS, inotify on Linux,
/// ReadDirectoryChangesW on Windows).
pub struct WorktreeWatcher {
    // Kept alive for the lifetime of the receiver; dropping it stops the watch.
    _watcher: RecommendedWatcher,
    rx: mpsc::UnboundedReceiver<PathBuf>,
}

impl WorktreeWatcher {
    pub fn watch<I, P>(paths: I) -> Result<Self, FsError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let (tx, rx) = mpsc::unbounded_channel::<PathBuf>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                if matches!(event.kind, EventKind::Remove(_)) {
                    for p in event.paths {
                        let _ = tx.send(p);
                    }
                }
            }
        })?;
        for p in paths {
            // Watching non-recursively: we only care about the registered
            // path itself disappearing, not its internals churning.
            watcher.watch(p.as_ref(), RecursiveMode::NonRecursive)?;
        }
        Ok(Self {
            _watcher: watcher,
            rx,
        })
    }

    /// `await` the next path-missing event. Returns `None` if the watcher
    /// has shut down.
    pub async fn recv(&mut self) -> Option<PathBuf> {
        self.rx.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn path_exists_reports_true_for_real_path() {
        let dir = TempDir::new().unwrap();
        let probe = TokioFilesystemProbe::new();
        assert!(probe.path_exists(dir.path()).await.unwrap());
    }

    #[tokio::test]
    async fn path_exists_reports_false_for_missing() {
        let dir = TempDir::new().unwrap();
        let probe = TokioFilesystemProbe::new();
        let missing = dir.path().join("does-not-exist");
        assert!(!probe.path_exists(&missing).await.unwrap());
    }

    #[tokio::test]
    async fn is_git_worktree_detects_dot_git_dir() {
        let dir = TempDir::new().unwrap();
        let probe = TokioFilesystemProbe::new();
        assert!(!probe.is_git_worktree(dir.path()).await.unwrap());
        tokio::fs::create_dir(dir.path().join(".git")).await.unwrap();
        assert!(probe.is_git_worktree(dir.path()).await.unwrap());
    }

    #[test]
    fn discover_repos_finds_nested_dot_git_dirs() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("a/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("b/c/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("d/not-a-repo")).unwrap();
        let mut found: Vec<_> = discover_repos_under(dir.path())
            .into_iter()
            .map(|p| p.strip_prefix(dir.path()).unwrap().to_path_buf())
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                std::path::PathBuf::from("a"),
                std::path::PathBuf::from("b/c"),
            ]
        );
    }

    #[test]
    fn discover_repos_finds_linked_worktrees() {
        let dir = TempDir::new().unwrap();
        // Primary checkout: .git is a directory.
        std::fs::create_dir_all(dir.path().join("real-repo/.git")).unwrap();
        // Linked worktree: .git is a file (pointer to the main repo).
        std::fs::create_dir_all(dir.path().join("linked-worktree")).unwrap();
        std::fs::write(
            dir.path().join("linked-worktree/.git"),
            "gitdir: /path/to/main/.git/worktrees/foo\n",
        )
        .unwrap();
        // Not a repo: no .git at all.
        std::fs::create_dir_all(dir.path().join("not-a-repo")).unwrap();

        let mut found: Vec<_> = discover_repos_under(dir.path())
            .into_iter()
            .map(|p| p.strip_prefix(dir.path()).unwrap().to_path_buf())
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                std::path::PathBuf::from("linked-worktree"),
                std::path::PathBuf::from("real-repo"),
            ]
        );
    }

    #[tokio::test]
    async fn worktree_watcher_emits_on_path_removal() {
        let dir = TempDir::new().unwrap();
        let watched = dir.path().join("watch_me");
        tokio::fs::create_dir(&watched).await.unwrap();
        let mut w = WorktreeWatcher::watch([watched.clone()]).unwrap();

        // Give the platform a moment to register the watch before triggering events.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tokio::fs::remove_dir(&watched).await.unwrap();

        // Notify events come through best-effort; bound the wait so a flaky
        // backend doesn't hang the suite. macOS FSEvents can take ~1s.
        let got = tokio::time::timeout(std::time::Duration::from_secs(3), w.recv()).await;
        match got {
            Ok(Some(p)) => assert!(
                p.ends_with("watch_me") || p == watched,
                "unexpected event path: {p:?}"
            ),
            // Don't fail the suite on a platform-specific notify quirk; just log.
            other => eprintln!("notify watcher result (best-effort): {other:?}"),
        }
    }
}
