//! infra-git — git remote URL parsing + minimal repo discovery.

mod discover;
mod error;
mod url;

pub use discover::{discover_canonical, discover_origin_url, is_inside_git_worktree};
pub use error::{GitError, Result};
pub use url::parse_canonical;
