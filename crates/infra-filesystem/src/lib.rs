//! infra-filesystem — real filesystem probe + scanning helpers + watcher.

mod discover;
mod error;
mod probe;
mod watcher;

pub use discover::discover_repos_under;
pub use error::FsError;
pub use probe::TokioFilesystemProbe;
pub use watcher::WorktreeWatcher;
