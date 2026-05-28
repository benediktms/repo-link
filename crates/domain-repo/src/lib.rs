//! domain-repo — Repository binding + worktree links.

mod binding;
mod link;
mod naming;

pub use binding::RepoBinding;
pub use link::{LinkStatus, WorktreeLink};
pub use naming::{derive_name, derive_prefix, is_valid_prefix};
