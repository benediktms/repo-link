//! domain-repo — Repo identity (origin), per-workspace membership (instance),
//! and worktree links. The pre-RFC-0005 single `RepoBinding` aggregate is split
//! into [`RepoOrigin`] (shared identity) and [`RepoInstance`] (membership).

mod instance;
mod link;
mod naming;
mod origin;
mod view;

pub use instance::RepoInstance;
pub use link::{LinkStatus, WorktreeLink};
pub use naming::{derive_name, derive_prefix, is_superseded_prefix, is_valid_prefix};
pub use origin::RepoOrigin;
pub use view::RepoBindingView;
