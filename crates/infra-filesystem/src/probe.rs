use std::path::Path;

use async_trait::async_trait;
use ports::{FilesystemProbe, PortError, PortResult};

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
            Err(e) => Err(PortError::Backend(format!("stat {}: {e}", path.display()))),
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
            Err(e) => Err(PortError::Backend(format!("stat {}: {e}", git.display()))),
        }
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
        tokio::fs::create_dir(dir.path().join(".git"))
            .await
            .unwrap();
        assert!(probe.is_git_worktree(dir.path()).await.unwrap());
    }
}
