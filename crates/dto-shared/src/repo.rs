use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::WorkspaceDto;

// ---------- Repo binding --------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeLinkDto {
    pub path: String,
    pub branch: Option<String>,
    pub status: String,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoBindingDto {
    pub id: String,
    pub workspace_id: String,
    /// The shared-identity origin id (RFC 0005). The `id` field is the
    /// per-workspace instance id; `origin_id` is the cross-workspace identity key.
    pub origin_id: String,
    pub remote_url: String,
    pub canonical_url: String,
    pub tracked_branch: Option<String>,
    pub name: String,
    pub aliases: Vec<String>,
    /// Globally-unique short handle used both as the human-typeable
    /// piece of friendly task IDs (`prefix-hash`) and as a stand-alone
    /// repo locator anywhere a binding ID is taken.
    pub prefix: String,
    pub worktrees: Vec<WorktreeLinkDto>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindRepoMatchDto {
    pub binding: RepoBindingDto,
    pub workspace_id: String,
    /// Which field matched: "name" | "alias" | "canonical_url" | "name_substring".
    pub matched_by: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindRepoResponseDto {
    pub query: String,
    pub matches: Vec<FindRepoMatchDto>,
    pub ambiguous: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachRepoCmd {
    pub workspace_id: String,
    pub remote_url: String,
    pub canonical_url: String,
    pub tracked_branch: Option<String>,
    /// Optional checkout path to register as a worktree on the binding.
    /// The CLI is responsible for verifying that the path's git origin
    /// canonicalises to `canonical_url`; the service trusts what it's
    /// handed and just records the link.
    pub link_path: Option<String>,
    pub link_branch: Option<String>,
    /// Explicit prefix override. When `None`, the service derives one
    /// from the repo name via [`domain_repo::derive_prefix`] and breaks
    /// collisions with a numeric suffix. When `Some`, the supplied
    /// value is validated against `^[a-z][a-z0-9]{1,19}$` and used
    /// verbatim — collisions surface as a `Conflict` error so the
    /// user is forced to pick a different prefix (rather than getting
    /// `myprefix1` silently).
    pub prefix: Option<String>,
}

/// Returned by `attach`: carries the resulting binding plus whether the
/// call merged into an existing one (same `canonical_url`) and which
/// path, if any, was newly linked. Lets agents distinguish "I created"
/// from "I joined" without comparing IDs out-of-band.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoAttachOutcomeDto {
    pub binding: RepoBindingDto,
    pub merged: bool,
    pub worktree_added: Option<String>,
}

/// One workspace-binding pair: this canonical URL is bound under `binding`
/// inside `workspace`. A repo can be a member of multiple workspaces, so
/// callers receive a `Vec<RepoMembershipDto>`. Used by `repo locate` and
/// `agents docs` to report cross-workspace membership.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMembershipDto {
    pub workspace: WorkspaceDto,
    pub binding: RepoBindingDto,
}

/// Display projection of a task's filing repo, surfaced as the additive
/// `filing_repo` block by `rl task show` (RFC 0002 D5 / #122). Post-RFC 0005
/// (§D4) the filing axis is **origin-level**, so `id` is the repo *origin* id —
/// the cross-workspace identity that matches the value stored on
/// `tasks.filing_repo_id`, not a per-workspace instance id. `name` /
/// `canonical_url` are origin-intrinsic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilingRepoRefDto {
    pub id: String,
    pub name: String,
    pub canonical_url: String,
}

/// Full result of a `repo locate` lookup. `canonical_url` is `None` when
/// the queried path isn't a git repo with an origin remote; `matches` is
/// empty when no binding references the discovered remote.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocateResponseDto {
    pub query_path: String,
    pub canonical_url: Option<String>,
    pub matches: Vec<RepoMembershipDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkWorktreeCmd {
    pub repo_id: String,
    pub path: String,
    pub branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnlinkWorktreeCmd {
    pub repo_id: String,
    pub path: String,
}
