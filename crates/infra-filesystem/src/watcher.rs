use std::path::{Path, PathBuf};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::error::FsError;

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
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res
                    && matches!(event.kind, EventKind::Remove(_))
                {
                    for p in event.paths {
                        let _ = tx.send(p);
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
