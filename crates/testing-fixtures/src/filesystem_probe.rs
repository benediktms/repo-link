use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use ports::{FilesystemProbe, PortResult};

// ---------- Filesystem probe ----------------------------------------------

#[derive(Default)]
pub struct StubFilesystemProbe {
    existing: Mutex<Vec<PathBuf>>,
    worktrees: Mutex<Vec<PathBuf>>,
}

impl StubFilesystemProbe {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_path(self, path: impl Into<PathBuf>) -> Self {
        self.existing.lock().unwrap().push(path.into());
        self
    }

    pub fn with_worktree(self, path: impl Into<PathBuf>) -> Self {
        let p = path.into();
        self.existing.lock().unwrap().push(p.clone());
        self.worktrees.lock().unwrap().push(p);
        self
    }

    pub fn remove(&self, path: &Path) {
        self.existing.lock().unwrap().retain(|p| p != path);
        self.worktrees.lock().unwrap().retain(|p| p != path);
    }
}

#[async_trait]
impl FilesystemProbe for StubFilesystemProbe {
    async fn path_exists(&self, path: &Path) -> PortResult<bool> {
        Ok(self.existing.lock().unwrap().iter().any(|p| p == path))
    }

    async fn is_git_worktree(&self, path: &Path) -> PortResult<bool> {
        Ok(self.worktrees.lock().unwrap().iter().any(|p| p == path))
    }
}
