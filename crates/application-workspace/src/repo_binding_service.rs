//! [`RepoBindingService`] — repo binding lifecycle, resolution, and
//! worktree reconciliation.

use std::path::PathBuf;
use std::sync::Arc;

use domain_core::{RepoId, WorkspaceId};
use domain_repo::RepoBinding;
use domain_workspace::WorkspaceStatus;
use dto_shared::{
    AttachRepoCmd, FindRepoMatchDto, FindRepoResponseDto, LinkWorktreeCmd, RepoAttachOutcomeDto,
    RepoBindingDto, RepoMembershipDto, UnlinkWorktreeCmd,
};
use ports::{FilesystemProbe, PortError, RepoBindingRepository, WorkspaceRepository};
use serde::{Deserialize, Serialize};

use crate::error::{AmbiguousCandidate, Result, ServiceError};
use crate::mapping::{binding_to_dto, map_prefix_conflict, workspace_to_dto};

pub struct RepoBindingService {
    workspaces: Arc<dyn WorkspaceRepository>,
    bindings: Arc<dyn RepoBindingRepository>,
}

impl RepoBindingService {
    pub fn new(
        workspaces: Arc<dyn WorkspaceRepository>,
        bindings: Arc<dyn RepoBindingRepository>,
    ) -> Self {
        Self {
            workspaces,
            bindings,
        }
    }

    /// Idempotent: if a binding for `canonical_url` already exists in the
    /// workspace, mutate that one and report `merged: true`. Otherwise
    /// create a fresh binding. In either case, when `link_path` is set we
    /// register it as a worktree on the resulting binding before saving.
    ///
    /// The service does NOT verify that `link_path` actually corresponds
    /// to a checkout of `canonical_url`; that's the CLI's responsibility
    /// (so tests and programmatic callers can wire whatever they like
    /// without git running).
    pub async fn attach(&self, cmd: AttachRepoCmd) -> Result<RepoAttachOutcomeDto> {
        let workspace_id: WorkspaceId = cmd.workspace_id.parse()?;
        // Confirm the workspace exists; bubbles up as PortError::NotFound otherwise.
        let _ = self.workspaces.get(workspace_id).await?;

        let (mut binding, merged) = match self
            .bindings
            .find_by_canonical_url(workspace_id, &cmd.canonical_url)
            .await?
        {
            Some(existing) => (existing, true),
            None => {
                let mut b = RepoBinding::new(workspace_id, cmd.remote_url, cmd.canonical_url)?;
                b.tracked_branch = cmd.tracked_branch;
                (b, false)
            }
        };

        // Explicit `--prefix` always wins over the derived value, even
        // on merge — interpreted as "set this binding's prefix to X if
        // it wasn't already". `set_prefix` validates the shape and
        // bumps `updated_at` only when the value actually changes.
        let explicit_prefix = cmd.prefix.is_some();
        if let Some(requested) = cmd.prefix {
            binding.set_prefix(requested)?;
        }

        let worktree_added = cmd.link_path.inspect(|path| {
            binding.link_worktree(PathBuf::from(path), cmd.link_branch);
        });

        if explicit_prefix {
            // Explicit prefix → surface conflicts as a human-readable
            // "pick a different one" rather than silent suffix-bumping
            // (`myprefix` → `myprefix1`) or a raw SQL error.
            self.bindings
                .save(&binding)
                .await
                .map_err(|e| map_prefix_conflict(e, &binding.prefix))?;
        } else {
            // Derived prefix → break collisions automatically.
            self.save_with_unique_prefix(&mut binding).await?;
        }
        Ok(RepoAttachOutcomeDto {
            binding: binding_to_dto(&binding),
            merged,
            worktree_added,
        })
    }

    /// Save a binding, retrying with a numeric suffix on `repos.prefix`
    /// UNIQUE violations. Two distinct repos with the same `name` would
    /// otherwise both derive the same prefix and collide globally; the
    /// suffix breaks the tie deterministically (`rpe` → `rpe1` → `rpe2`).
    ///
    /// The retry runs against the database, not a pre-flight cache —
    /// the spec is explicit that uniqueness is the index's job and a
    /// pre-check would race.
    async fn save_with_unique_prefix(&self, binding: &mut RepoBinding) -> Result<()> {
        // No low ceiling: automatic collision-breaking must scale to any
        // number of same-named repos (`rpe100` is just as valid as
        // `rpe1`). The suffix is bounded by the 20-char prefix cap, and
        // SAFETY_CAP only guards against a logic-bug infinite loop, not
        // legitimate data.
        const SAFETY_CAP: u32 = 1_000_000;
        let base = binding.prefix.clone();
        let mut suffix: u32 = 0;
        loop {
            match self.bindings.save(binding).await {
                Ok(()) => return Ok(()),
                Err(e) if e.conflict_target() == Some("repos.prefix") => {
                    suffix += 1;
                    if suffix > SAFETY_CAP {
                        return Err(ServiceError::Port(PortError::Backend(format!(
                            "could not allocate unique repo prefix from base '{base}' after {SAFETY_CAP} attempts"
                        ))));
                    }
                    let suffix_str = suffix.to_string();
                    let max_base_chars = 20usize.saturating_sub(suffix_str.len());
                    let trimmed: String = base.chars().take(max_base_chars).collect();
                    let candidate = format!("{trimmed}{suffix_str}");
                    binding.set_prefix(candidate)?;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub async fn detach(&self, id: &str) -> Result<()> {
        let id: RepoId = id.parse()?;
        self.bindings.delete(id).await?;
        Ok(())
    }

    pub async fn show(&self, query: &str) -> Result<RepoBindingDto> {
        if let Ok(id) = query.parse::<RepoId>() {
            let b = self.bindings.get(id).await?;
            return Ok(binding_to_dto(&b));
        }
        let binding = self.resolve_by_handle(query).await?;
        Ok(binding_to_dto(&binding))
    }

    /// Resolve a UUID, exact name, or exact alias to a `RepoBinding`.
    async fn resolve(&self, query: &str) -> Result<RepoBinding> {
        if let Ok(id) = query.parse::<RepoId>() {
            return Ok(self.bindings.get(id).await?);
        }
        self.resolve_by_handle(query).await
    }

    /// Resolve `query` to a binding by trying, in order: exact
    /// `prefix` match (globally-unique, single index lookup); then
    /// exact `name` or `alias` match. The prefix takes priority because it
    /// carries an explicit uniqueness guarantee — names and aliases can clash
    /// across workspaces, so they still produce the ambiguity error.
    ///
    /// This resolver backs write commands (`--repo` on `task create`,
    /// `set-filing-repo`, etc.), so a repo whose workspace is archived must
    /// still be addressable by handle: archiving hides a workspace from
    /// listings but does NOT lock it.
    ///
    /// Archived workspaces are therefore searched, but only as a **fallback**:
    /// name/alias matches in ACTIVE workspaces win outright, and archived
    /// bindings are consulted solely when the active set yields nothing. This
    /// keeps archiving from broadening the ambiguity surface — a live handle
    /// that resolved cleanly before still resolves cleanly even when an
    /// archived workspace happens to hold a same-named binding. Ambiguity is
    /// still surfaced WITHIN whichever set ultimately resolves the handle.
    async fn resolve_by_handle(&self, query: &str) -> Result<RepoBinding> {
        if let Some(binding) = self.bindings.find_by_prefix(query).await? {
            return Ok(binding);
        }
        let workspaces = self.workspaces.list(true).await?;
        let mut active: Vec<RepoBinding> = Vec::new();
        let mut archived: Vec<RepoBinding> = Vec::new();
        for ws in &workspaces {
            // `list(true)` returns ALL statuses, Deleted included — a deleted
            // workspace's bindings must never resolve a handle, so drop them
            // before bucketing (else they'd land in `active` and either route a
            // write to a dead workspace or falsely trip AmbiguousHandle).
            if ws.status == WorkspaceStatus::Deleted {
                continue;
            }
            let bindings = self.bindings.list_by_workspace(ws.id).await?;
            for b in bindings {
                if b.name == query || b.aliases.iter().any(|a| a == query) {
                    if ws.status == WorkspaceStatus::Archived {
                        archived.push(b);
                    } else {
                        active.push(b);
                    }
                }
            }
        }
        // Active wins; archived is the fallback consulted only when no active
        // binding matches the handle.
        let mut matches = if active.is_empty() { archived } else { active };
        match matches.len() {
            0 => Err(ServiceError::BindingNotFound(query.to_string())),
            1 => Ok(matches.remove(0)),
            _ => Err(ServiceError::AmbiguousHandle {
                query: query.to_string(),
                candidates: matches
                    .into_iter()
                    .map(|b| AmbiguousCandidate {
                        id: b.id.to_string(),
                        workspace_id: b.workspace_id.to_string(),
                        canonical_url: b.canonical_url.clone(),
                        name: b.name.clone(),
                    })
                    .collect(),
            }),
        }
    }

    pub async fn rename(&self, query: &str, new_name: String) -> Result<RepoBindingDto> {
        let mut binding = self.resolve(query).await?;
        binding.set_name(new_name)?;
        self.bindings.save(&binding).await?;
        Ok(binding_to_dto(&binding))
    }

    /// Replace the binding's prefix with an explicit value. Validates
    /// against `^[a-z][a-z0-9]{1,19}$`; surfaces `Conflict` if another
    /// binding already owns the requested prefix (so the user picks a
    /// different one rather than getting silent suffix-bumping). Every
    /// composite ID a user has already typed against the *old* prefix
    /// goes stale — the bare hash still resolves, but `oldprefix-ak7`
    /// will now error with PrefixMismatch. Document this in CLI help.
    pub async fn set_prefix(&self, query: &str, new_prefix: String) -> Result<RepoBindingDto> {
        let mut binding = self.resolve(query).await?;
        binding.set_prefix(new_prefix)?;
        self.bindings
            .save(&binding)
            .await
            .map_err(|e| map_prefix_conflict(e, &binding.prefix))?;
        Ok(binding_to_dto(&binding))
    }

    pub async fn add_alias(&self, query: &str, alias: String) -> Result<RepoBindingDto> {
        let mut binding = self.resolve(query).await?;
        binding.add_alias(alias)?;
        self.bindings.save(&binding).await?;
        Ok(binding_to_dto(&binding))
    }

    pub async fn remove_alias(&self, query: &str, alias: &str) -> Result<RepoBindingDto> {
        let mut binding = self.resolve(query).await?;
        if !binding.remove_alias(alias) {
            return Err(ServiceError::Domain(domain_core::DomainError::validation(
                format!("alias '{alias}' not found"),
            )));
        }
        self.bindings.save(&binding).await?;
        Ok(binding_to_dto(&binding))
    }

    pub async fn find(&self, query: &str) -> Result<FindRepoResponseDto> {
        let workspaces = self.workspaces.list(false).await?;
        let mut hits: Vec<(u8, RepoBinding, String)> = Vec::new();
        for ws in &workspaces {
            let bindings = self.bindings.list_by_workspace(ws.id).await?;
            for b in bindings {
                if b.name == query {
                    hits.push((0, b, "name".to_string()));
                } else if b.aliases.iter().any(|a| a == query) {
                    hits.push((1, b, "alias".to_string()));
                } else if b.canonical_url.contains(query) {
                    hits.push((2, b, "canonical_url".to_string()));
                } else if b.name.contains(query) {
                    hits.push((3, b, "name_substring".to_string()));
                }
            }
        }
        hits.sort_by_key(|(rank, b, _)| (*rank, b.created_at));
        let matches: Vec<FindRepoMatchDto> = hits
            .into_iter()
            .map(|(_, b, matched_by)| FindRepoMatchDto {
                workspace_id: b.workspace_id.to_string(),
                binding: binding_to_dto(&b),
                matched_by,
            })
            .collect();
        let ambiguous = matches.len() > 1;
        Ok(FindRepoResponseDto {
            query: query.to_string(),
            matches,
            ambiguous,
        })
    }

    pub async fn list(&self, workspace_id: &str) -> Result<Vec<RepoBindingDto>> {
        let workspace_id: WorkspaceId = workspace_id.parse()?;
        let rows = self.bindings.list_by_workspace(workspace_id).await?;
        Ok(rows.iter().map(binding_to_dto).collect())
    }

    /// Return every (workspace, binding) pair whose binding's
    /// `canonical_url` is an exact match. Archived workspaces are excluded
    /// unless `include_archived` is set (the `rl repo locate -a` opt-in).
    /// Direct key lookup, not a search — callers want the full membership set,
    /// not a ranked best hit. See [`find`] for the ranked / fuzzy variant.
    ///
    /// [`find`]: Self::find
    pub async fn memberships_for_canonical_url(
        &self,
        canonical_url: &str,
        include_archived: bool,
    ) -> Result<Vec<RepoMembershipDto>> {
        let workspaces = self.workspaces.list(include_archived).await?;
        let mut out = Vec::new();
        for ws in &workspaces {
            if let Some(binding) = self
                .bindings
                .find_by_canonical_url(ws.id, canonical_url)
                .await?
            {
                out.push(RepoMembershipDto {
                    workspace: workspace_to_dto(ws),
                    binding: binding_to_dto(&binding),
                });
            }
        }
        Ok(out)
    }

    pub async fn link_worktree(&self, cmd: LinkWorktreeCmd) -> Result<RepoBindingDto> {
        let id: RepoId = cmd.repo_id.parse()?;
        let mut binding = self.bindings.get(id).await?;
        binding.link_worktree(PathBuf::from(cmd.path), cmd.branch);
        self.bindings.save(&binding).await?;
        Ok(binding_to_dto(&binding))
    }

    pub async fn unlink_worktree(&self, cmd: UnlinkWorktreeCmd) -> Result<RepoBindingDto> {
        let id: RepoId = cmd.repo_id.parse()?;
        let mut binding = self.bindings.get(id).await?;
        binding.unlink_worktree(std::path::Path::new(&cmd.path))?;
        self.bindings.save(&binding).await?;
        Ok(binding_to_dto(&binding))
    }

    pub async fn prune_missing(&self, id: &str) -> Result<RepoBindingDto> {
        let id: RepoId = id.parse()?;
        let mut binding = self.bindings.get(id).await?;
        binding.prune_missing();
        self.bindings.save(&binding).await?;
        Ok(binding_to_dto(&binding))
    }

    /// Walk every binding in the workspace, ask the probe whether each
    /// recorded worktree path still exists, and persist the resulting
    /// status transitions. Optionally prune entries we just marked missing.
    ///
    /// Idempotent — running it twice produces the same final state.
    pub async fn reconcile_worktrees(
        &self,
        workspace_id: &str,
        probe: &dyn FilesystemProbe,
        prune: bool,
    ) -> Result<ReconcileSummary> {
        let workspace_id: WorkspaceId = workspace_id.parse()?;
        // Confirm the workspace exists; bubbles up as PortError::NotFound otherwise.
        let _ = self.workspaces.get(workspace_id).await?;
        let bindings = self.bindings.list_by_workspace(workspace_id).await?;

        let mut summary = ReconcileSummary::default();
        for mut binding in bindings {
            summary.repos_checked += 1;
            let mut missing_paths = Vec::new();
            for link in &binding.worktrees {
                summary.worktrees_checked += 1;
                let exists = probe.path_exists(&link.path).await?;
                let already_missing = matches!(
                    link.status,
                    domain_repo::LinkStatus::MissingPath | domain_repo::LinkStatus::Detached
                );
                if !exists && !already_missing {
                    missing_paths.push(link.path.clone());
                }
            }

            let mut changed = false;
            for path in &missing_paths {
                binding.mark_path_missing(path)?;
                summary.marked_missing += 1;
                changed = true;
            }
            if prune {
                let pruned = binding.prune_missing();
                if pruned > 0 {
                    summary.pruned += pruned;
                    changed = true;
                }
            }
            if changed {
                self.bindings.save(&binding).await?;
            }
        }
        Ok(summary)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileSummary {
    pub repos_checked: usize,
    pub worktrees_checked: usize,
    pub marked_missing: usize,
    pub pruned: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkspaceService;
    use dto_shared::CreateWorkspaceCmd;
    use testing_fixtures::{InMemoryRepoBindingRepository, InMemoryWorkspaceRepository};

    fn setup() -> (WorkspaceService, RepoBindingService) {
        let workspaces: Arc<dyn WorkspaceRepository> = Arc::new(InMemoryWorkspaceRepository::new());
        let bindings: Arc<dyn RepoBindingRepository> =
            Arc::new(InMemoryRepoBindingRepository::new());
        (
            WorkspaceService::new(workspaces.clone()),
            RepoBindingService::new(workspaces, bindings),
        )
    }

    #[tokio::test]
    async fn attach_repo_requires_workspace() {
        let (_, bsvc) = setup();
        let err = bsvc
            .attach(AttachRepoCmd {
                workspace_id: domain_core::WorkspaceId::new().to_string(),
                remote_url: "git@github.com:o/r.git".into(),
                canonical_url: "github.com/o/r".into(),
                tracked_branch: None,
                link_path: None,
                link_branch: None,
                prefix: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::Port(PortError::NotFound(_))));
    }

    #[tokio::test]
    async fn attach_and_link_worktree_roundtrip() {
        let (ws_svc, bsvc) = setup();
        let ws = ws_svc
            .create(CreateWorkspaceCmd {
                name: "w".into(),
                description: None,
                local_only: true,
                project_spec: None,
            })
            .await
            .unwrap();
        let b = bsvc
            .attach(AttachRepoCmd {
                workspace_id: ws.id.clone(),
                remote_url: "git@github.com:o/r.git".into(),
                canonical_url: "github.com/o/r".into(),
                tracked_branch: Some("main".into()),
                link_path: None,
                link_branch: None,
                prefix: None,
            })
            .await
            .unwrap();
        assert!(!b.merged);
        assert!(b.worktree_added.is_none());
        let linked = bsvc
            .link_worktree(LinkWorktreeCmd {
                repo_id: b.binding.id.clone(),
                path: "/tmp/repo".into(),
                branch: Some("main".into()),
            })
            .await
            .unwrap();
        assert_eq!(linked.worktrees.len(), 1);
        assert_eq!(linked.worktrees[0].status, "linked");
    }

    #[tokio::test]
    async fn reconcile_marks_missing_and_optionally_prunes() {
        use testing_fixtures::StubFilesystemProbe;
        let (ws_svc, bsvc) = setup();
        let ws = ws_svc
            .create(CreateWorkspaceCmd {
                name: "w".into(),
                description: None,
                local_only: true,
                project_spec: None,
            })
            .await
            .unwrap();
        let b = bsvc
            .attach(AttachRepoCmd {
                workspace_id: ws.id.clone(),
                remote_url: "git@github.com:o/r.git".into(),
                canonical_url: "github.com/o/r".into(),
                tracked_branch: None,
                link_path: None,
                link_branch: None,
                prefix: None,
            })
            .await
            .unwrap();
        bsvc.link_worktree(LinkWorktreeCmd {
            repo_id: b.binding.id.clone(),
            path: "/tmp/alive".into(),
            branch: None,
        })
        .await
        .unwrap();
        bsvc.link_worktree(LinkWorktreeCmd {
            repo_id: b.binding.id.clone(),
            path: "/tmp/gone".into(),
            branch: None,
        })
        .await
        .unwrap();

        // Probe sees /tmp/alive but not /tmp/gone.
        let probe = StubFilesystemProbe::new().with_path("/tmp/alive");

        let summary = bsvc
            .reconcile_worktrees(&ws.id, &probe, false)
            .await
            .unwrap();
        assert_eq!(summary.repos_checked, 1);
        assert_eq!(summary.worktrees_checked, 2);
        assert_eq!(summary.marked_missing, 1);
        assert_eq!(summary.pruned, 0);

        // Second pass with prune=true drops the missing path.
        let summary2 = bsvc
            .reconcile_worktrees(&ws.id, &probe, true)
            .await
            .unwrap();
        // /tmp/gone is now MissingPath (already_missing branch) → no new marks,
        // but prune removes it.
        assert_eq!(summary2.marked_missing, 0);
        assert_eq!(summary2.pruned, 1);

        let after = bsvc.show(&b.binding.id).await.unwrap();
        assert_eq!(after.worktrees.len(), 1);
        assert_eq!(after.worktrees[0].path, "/tmp/alive");
    }

    #[tokio::test]
    async fn reconcile_worktrees_unknown_workspace_returns_not_found() {
        use testing_fixtures::StubFilesystemProbe;
        let (_, bsvc) = setup();
        let unknown_id = domain_core::WorkspaceId::new().to_string();
        let probe = StubFilesystemProbe::new();
        let err = bsvc
            .reconcile_worktrees(&unknown_id, &probe, false)
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::Port(PortError::NotFound(_))));
    }

    #[tokio::test]
    async fn attach_with_existing_canonical_merges() {
        let (ws_svc, bsvc) = setup();
        let ws = ws_svc
            .create(CreateWorkspaceCmd {
                name: "w".into(),
                description: None,
                local_only: true,
                project_spec: None,
            })
            .await
            .unwrap();
        let cmd = AttachRepoCmd {
            workspace_id: ws.id.clone(),
            remote_url: "git@github.com:o/r.git".into(),
            canonical_url: "github.com/o/r".into(),
            tracked_branch: None,
            link_path: None,
            link_branch: None,
            prefix: None,
        };
        let first = bsvc.attach(cmd.clone()).await.unwrap();
        assert!(!first.merged);
        let second = bsvc.attach(cmd).await.unwrap();
        assert!(second.merged);
        assert_eq!(second.binding.id, first.binding.id);
    }

    #[tokio::test]
    async fn attach_merges_when_canonical_exists() {
        let (ws_svc, bsvc) = setup();
        let ws = ws_svc
            .create(CreateWorkspaceCmd {
                name: "w2".into(),
                description: None,
                local_only: true,
                project_spec: None,
            })
            .await
            .unwrap();
        let first = bsvc
            .attach(AttachRepoCmd {
                workspace_id: ws.id.clone(),
                remote_url: "git@github.com:o/r.git".into(),
                canonical_url: "github.com/o/r".into(),
                tracked_branch: None,
                link_path: None,
                link_branch: None,
                prefix: None,
            })
            .await
            .unwrap();
        assert!(!first.merged);
        assert!(first.worktree_added.is_none());
        assert_eq!(first.binding.worktrees.len(), 0);

        let second = bsvc
            .attach(AttachRepoCmd {
                workspace_id: ws.id.clone(),
                remote_url: "git@github.com:o/r.git".into(),
                canonical_url: "github.com/o/r".into(),
                tracked_branch: None,
                link_path: Some("/tmp/second".into()),
                link_branch: None,
                prefix: None,
            })
            .await
            .unwrap();
        assert!(second.merged);
        assert_eq!(second.worktree_added, Some("/tmp/second".into()));
        assert_eq!(second.binding.id, first.binding.id);
        assert_eq!(second.binding.worktrees.len(), 1);
    }

    #[tokio::test]
    async fn attach_links_worktree_when_link_path_given() {
        let (ws_svc, bsvc) = setup();
        let ws = ws_svc
            .create(CreateWorkspaceCmd {
                name: "w3".into(),
                description: None,
                local_only: true,
                project_spec: None,
            })
            .await
            .unwrap();
        let outcome = bsvc
            .attach(AttachRepoCmd {
                workspace_id: ws.id.clone(),
                remote_url: "git@github.com:o/r.git".into(),
                canonical_url: "github.com/o/r".into(),
                tracked_branch: None,
                link_path: Some("/tmp/checkout".into()),
                link_branch: Some("main".into()),
                prefix: None,
            })
            .await
            .unwrap();
        assert!(!outcome.merged);
        assert_eq!(outcome.worktree_added, Some("/tmp/checkout".into()));
        assert_eq!(outcome.binding.worktrees.len(), 1);
        assert_eq!(outcome.binding.worktrees[0].path, "/tmp/checkout");
        assert_eq!(outcome.binding.worktrees[0].branch, Some("main".into()));
        assert_eq!(outcome.binding.worktrees[0].status, "linked");
    }

    // ---- Phase B: rename / alias / find / show-resolution ---------------

    async fn seeded(
        ws_svc: &WorkspaceService,
        bsvc: &RepoBindingService,
        ws_name: &str,
        canonical: &str,
    ) -> RepoBindingDto {
        let ws = ws_svc
            .create(CreateWorkspaceCmd {
                name: ws_name.into(),
                description: None,
                local_only: true,
                project_spec: None,
            })
            .await
            .unwrap();
        bsvc.attach(AttachRepoCmd {
            workspace_id: ws.id,
            remote_url: format!("git@example.com:{canonical}.git"),
            canonical_url: canonical.into(),
            tracked_branch: None,
            link_path: None,
            link_branch: None,
            prefix: None,
        })
        .await
        .unwrap()
        .binding
    }

    #[tokio::test]
    async fn rename_persists() {
        let (ws_svc, bsvc) = setup();
        let b = seeded(&ws_svc, &bsvc, "w-rename", "github.com/o/r").await;
        assert_eq!(b.name, "r");
        let renamed = bsvc.rename(&b.id, "gateway".into()).await.unwrap();
        assert_eq!(renamed.name, "gateway");
        // Round-trip via show: the new name is queryable.
        let shown = bsvc.show("gateway").await.unwrap();
        assert_eq!(shown.id, b.id);
    }

    #[tokio::test]
    async fn add_alias_dedup_persists() {
        let (ws_svc, bsvc) = setup();
        let b = seeded(&ws_svc, &bsvc, "w-alias", "github.com/o/r").await;
        bsvc.add_alias(&b.id, "edge".into()).await.unwrap();
        let again = bsvc.add_alias(&b.id, "edge".into()).await.unwrap();
        assert_eq!(again.aliases, vec!["edge".to_string()]);
    }

    #[tokio::test]
    async fn remove_alias_errors_when_absent() {
        let (ws_svc, bsvc) = setup();
        let b = seeded(&ws_svc, &bsvc, "w-rm", "github.com/o/r").await;
        let err = bsvc.remove_alias(&b.id, "no-such").await.unwrap_err();
        assert!(matches!(err, ServiceError::Domain(_)));
    }

    #[tokio::test]
    async fn show_resolves_uuid_passthrough() {
        let (ws_svc, bsvc) = setup();
        let b = seeded(&ws_svc, &bsvc, "w-uuid", "github.com/o/r").await;
        let by_uuid = bsvc.show(&b.id).await.unwrap();
        assert_eq!(by_uuid.id, b.id);
    }

    #[tokio::test]
    async fn show_resolves_by_exact_name() {
        let (ws_svc, bsvc) = setup();
        let b = seeded(&ws_svc, &bsvc, "w-name", "github.com/o/demo-app").await;
        let by_name = bsvc.show("demo-app").await.unwrap();
        assert_eq!(by_name.id, b.id);
    }

    #[tokio::test]
    async fn show_resolves_by_exact_alias() {
        let (ws_svc, bsvc) = setup();
        let b = seeded(&ws_svc, &bsvc, "w-alias-show", "github.com/o/r").await;
        bsvc.add_alias(&b.id, "gw".into()).await.unwrap();
        let hit = bsvc.show("gw").await.unwrap();
        assert_eq!(hit.id, b.id);
    }

    #[tokio::test]
    async fn show_errors_on_ambiguous_handle_with_candidates() {
        let (ws_svc, bsvc) = setup();
        // Two workspaces, two bindings, same alias on both.
        let a = seeded(&ws_svc, &bsvc, "ws-a", "github.com/o/a").await;
        let b = seeded(&ws_svc, &bsvc, "ws-b", "github.com/o/b").await;
        bsvc.add_alias(&a.id, "gw".into()).await.unwrap();
        bsvc.add_alias(&b.id, "gw".into()).await.unwrap();
        let err = bsvc.show("gw").await.unwrap_err();
        match err {
            ServiceError::AmbiguousHandle { query, candidates } => {
                assert_eq!(query, "gw");
                assert_eq!(candidates.len(), 2);
            }
            other => panic!("expected AmbiguousHandle, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_resolution_prefers_active_over_archived_collision() {
        let (ws_svc, bsvc) = setup();
        // Same alias on two workspaces' bindings; archive one. The active
        // binding must still resolve cleanly — archiving must NOT introduce a
        // new ambiguity where a live handle resolved before.
        let active = seeded(&ws_svc, &bsvc, "ws-active", "github.com/o/a").await;
        let stale = seeded(&ws_svc, &bsvc, "ws-stale", "github.com/o/b").await;
        bsvc.add_alias(&active.id, "gw".into()).await.unwrap();
        bsvc.add_alias(&stale.id, "gw".into()).await.unwrap();
        ws_svc.archive(&stale.workspace_id).await.unwrap();
        let hit = bsvc.show("gw").await.unwrap();
        assert_eq!(hit.id, active.id, "active binding wins the handle");
    }

    #[tokio::test]
    async fn handle_resolution_falls_back_to_archived_when_no_active_match() {
        let (ws_svc, bsvc) = setup();
        // A binding whose only workspace is archived stays addressable by
        // handle (archiving hides, never locks) — the archived set is the
        // fallback when no active workspace matches.
        let b = seeded(&ws_svc, &bsvc, "ws-only-archived", "github.com/o/lonely").await;
        bsvc.add_alias(&b.id, "gw".into()).await.unwrap();
        ws_svc.archive(&b.workspace_id).await.unwrap();
        let hit = bsvc.show("gw").await.unwrap();
        assert_eq!(hit.id, b.id);
    }

    #[tokio::test]
    async fn handle_resolution_ignores_deleted_workspace_bindings() {
        use domain_workspace::{Workspace, WorkspaceName, WorkspaceStatus};
        // `list(true)` returns ALL statuses, Deleted included. A deleted
        // workspace's binding must never resolve a handle — neither route a
        // write to a dead workspace nor falsely trip AmbiguousHandle against a
        // live one. Force-seed Deleted directly (no command transitions into
        // it yet) to lock the guard.
        let workspaces: Arc<dyn WorkspaceRepository> = Arc::new(InMemoryWorkspaceRepository::new());
        let bindings: Arc<dyn RepoBindingRepository> =
            Arc::new(InMemoryRepoBindingRepository::new());
        let bsvc = RepoBindingService::new(workspaces.clone(), bindings);

        let mut deleted = Workspace::new(WorkspaceName::new("ws-deleted").unwrap(), None, true);
        deleted.status = WorkspaceStatus::Deleted;
        workspaces.save(&deleted).await.unwrap();
        // Name = canonical's last segment = "widget-service"; query by name so
        // the prefix path (globally-unique, status-agnostic) doesn't shadow it.
        bsvc.attach(AttachRepoCmd {
            workspace_id: deleted.id.to_string(),
            remote_url: "git@example.com:o/widget-service.git".into(),
            canonical_url: "github.com/o/widget-service".into(),
            tracked_branch: None,
            link_path: None,
            link_branch: None,
            prefix: None,
        })
        .await
        .unwrap();

        let err = bsvc.show("widget-service").await.unwrap_err();
        assert!(
            matches!(err, ServiceError::BindingNotFound(_)),
            "deleted workspace's binding must not resolve, got {err:?}"
        );
    }

    #[tokio::test]
    async fn show_errors_when_handle_unknown() {
        let (ws_svc, bsvc) = setup();
        let _ = seeded(&ws_svc, &bsvc, "w-unknown", "github.com/o/r").await;
        let err = bsvc.show("nothing-matches").await.unwrap_err();
        assert!(matches!(err, ServiceError::BindingNotFound(_)));
    }

    #[tokio::test]
    async fn find_ranks_name_over_canonical_substring() {
        let (ws_svc, bsvc) = setup();
        // Binding A: canonical contains "foo" in the owner slot, name = "r".
        let a = seeded(&ws_svc, &bsvc, "ws-a2", "github.com/foo/r").await;
        // Binding B: name is exactly "foo" (canonical's last segment).
        let b = seeded(&ws_svc, &bsvc, "ws-b2", "github.com/owner/foo").await;
        let out = bsvc.find("foo").await.unwrap();
        assert!(out.ambiguous);
        assert_eq!(out.matches.len(), 2);
        // Rank 0 (exact name) must come first.
        assert_eq!(out.matches[0].binding.id, b.id);
        assert_eq!(out.matches[0].matched_by, "name");
        assert_eq!(out.matches[1].binding.id, a.id);
        assert_eq!(out.matches[1].matched_by, "canonical_url");
    }

    #[tokio::test]
    async fn find_marks_ambiguous_when_multi_match() {
        let (ws_svc, bsvc) = setup();
        let a = seeded(&ws_svc, &bsvc, "ws-a3", "github.com/o/a").await;
        let b = seeded(&ws_svc, &bsvc, "ws-b3", "github.com/o/b").await;
        bsvc.add_alias(&a.id, "common".into()).await.unwrap();
        bsvc.add_alias(&b.id, "common".into()).await.unwrap();
        let out = bsvc.find("common").await.unwrap();
        assert!(out.ambiguous);
        assert_eq!(out.matches.len(), 2);
        assert!(out.matches.iter().all(|m| m.matched_by == "alias"));
    }

    // ---- memberships_for_canonical_url --------------------------------

    #[tokio::test]
    async fn memberships_for_canonical_url_returns_empty_when_no_match() {
        let (ws_svc, bsvc) = setup();
        let _ = seeded(&ws_svc, &bsvc, "ws-mempty", "github.com/o/r").await;
        let out = bsvc
            .memberships_for_canonical_url("github.com/o/other", false)
            .await
            .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn memberships_for_canonical_url_returns_single_match() {
        let (ws_svc, bsvc) = setup();
        let binding = seeded(&ws_svc, &bsvc, "ws-msingle", "github.com/o/repo").await;
        let out = bsvc
            .memberships_for_canonical_url("github.com/o/repo", false)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].workspace.name, "ws-msingle");
        assert_eq!(out[0].binding.id, binding.id);
        assert_eq!(out[0].binding.canonical_url, "github.com/o/repo");
    }

    #[tokio::test]
    async fn memberships_for_canonical_url_returns_all_workspace_matches() {
        let (ws_svc, bsvc) = setup();
        let canonical = "github.com/shared/repo";
        let a = seeded(&ws_svc, &bsvc, "ws-alpha", canonical).await;
        let b = seeded(&ws_svc, &bsvc, "ws-beta", canonical).await;
        // Decoy in a third workspace with a different repo.
        let _ = seeded(&ws_svc, &bsvc, "ws-decoy", "github.com/o/unrelated").await;

        let out = bsvc
            .memberships_for_canonical_url(canonical, false)
            .await
            .unwrap();
        assert_eq!(out.len(), 2);
        let workspace_names: Vec<&str> = out.iter().map(|m| m.workspace.name.as_str()).collect();
        assert!(workspace_names.contains(&"ws-alpha"));
        assert!(workspace_names.contains(&"ws-beta"));
        let binding_ids: Vec<&str> = out.iter().map(|m| m.binding.id.as_str()).collect();
        assert!(binding_ids.contains(&a.id.as_str()));
        assert!(binding_ids.contains(&b.id.as_str()));
    }

    #[tokio::test]
    async fn memberships_excludes_archived_by_default_includes_with_flag() {
        let (ws_svc, bsvc) = setup();
        let canonical = "github.com/o/arch";
        let _live = seeded(&ws_svc, &bsvc, "ws-live", canonical).await;
        let arch = seeded(&ws_svc, &bsvc, "ws-arch", canonical).await;
        ws_svc.archive(&arch.workspace_id).await.unwrap();

        let default = bsvc
            .memberships_for_canonical_url(canonical, false)
            .await
            .unwrap();
        assert_eq!(default.len(), 1, "archived workspace hidden by default");
        assert_eq!(default[0].workspace.name, "ws-live");

        let all = bsvc
            .memberships_for_canonical_url(canonical, true)
            .await
            .unwrap();
        assert_eq!(
            all.len(),
            2,
            "-a / include_archived surfaces the archived one"
        );
    }
}
