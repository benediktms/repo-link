//! Combined read view for a workspace's repo binding.

use crate::{RepoInstance, RepoOrigin};

/// Combined read view: the shared identity ([`RepoOrigin`]) plus the
/// per-workspace membership ([`RepoInstance`]). Returned by the read
/// methods of [`ports::RepoBindingRepository`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoBindingView {
    pub origin: RepoOrigin,
    pub instance: RepoInstance,
}
