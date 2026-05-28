//! Filesystem probe port.

use std::path::Path;

use async_trait::async_trait;

use crate::error::PortResult;

// ---------- Filesystem probe ---------------------------------------------

#[async_trait]
pub trait FilesystemProbe: Send + Sync {
    async fn path_exists(&self, path: &Path) -> PortResult<bool>;
    async fn is_git_worktree(&self, path: &Path) -> PortResult<bool>;
}
