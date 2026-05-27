//! domain-task — Task aggregate, lifecycle + sync state machines, relations.
//!
//! Lifecycle and sync are **orthogonal**: a task can be `Open + DirtyLocal`
//! or `Blocked + Synced`. The two enums live side-by-side on `Task` so the
//! sync engine can reconcile without first asking whether a task is alive,
//! and the planning UI can filter blockers without caring about remote
//! drift.

use domain_core::{Aggregate, DomainError, RepoId, Result, TaskId, Timestamp, WorkspaceId};
use serde::{Deserialize, Serialize};

/// Minimum minted hash length. Minting always starts here and only
/// grows on repeated collisions.
pub const MIN_HASH_LEN: usize = 3;

/// Maximum minted hash length. Both mint paths (runtime `task create`
/// and the `open_db` backfill) cap growth here, and [`is_valid_hash`]
/// accepts up to this length — keeping the validator and the minters
/// in lockstep so a grown hash can never become unresolvable. 16 chars
/// of base32 is 32^16 ≈ 10^24 values; reaching this length is
/// effectively impossible in practice. A single UUID yields up to 25
/// base32 chars of entropy, so one draw covers it.
pub const MAX_HASH_LEN: usize = 16;

/// `^[a-z2-7]{MIN..=MAX}$` — the shape of a minted hash. Used by the
/// resolver to reject obviously-malformed input (wrong case, illegal
/// chars, a truncated UUID's trailing group) with a clear "bad id"
/// error rather than a misleading "task hash not found". The bounds
/// match the minters so any hash the system can persist also resolves.
pub fn is_valid_hash(s: &str) -> bool {
    let len = s.chars().count();
    (MIN_HASH_LEN..=MAX_HASH_LEN).contains(&len)
        && s.chars().all(|c| matches!(c, 'a'..='z' | '2'..='7'))
}

/// Generate a random lowercase RFC 4648 base32 string of the given
/// length. Backs the friendly task ID minting: the persistence layer
/// retries with new randomness on `UNIQUE` index collisions and grows
/// the requested length once the failure rate at a given length climbs.
///
/// Uses a fresh UUID's bytes as the entropy source — keeps the
/// dependency tree small (no extra `rand` crate) and reuses the
/// randomness primitive that already mints `TaskId`s. One UUID supplies
/// up to 25 base32 chars of entropy (16 bytes × 8 / 5), which covers
/// the full `MIN_HASH_LEN..=MAX_HASH_LEN` range callers consume.
pub fn random_lowercase_base32(length: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(length);
    let mut acc: u64 = 0;
    let mut bits: u32 = 0;
    // One UUID yields 128 bits ≈ 25 base32 chars. Draw additional UUIDs
    // as needed so the function always returns exactly `length` chars,
    // even past 25 — otherwise it would silently underfill and break
    // the length-growth collision strategy.
    while out.len() < length {
        for &b in uuid::Uuid::new_v4().as_bytes() {
            acc = (acc << 8) | (b as u64);
            bits += 8;
            while bits >= 5 && out.len() < length {
                bits -= 5;
                let idx = ((acc >> bits) & 0b11111) as usize;
                out.push(ALPHABET[idx] as char);
            }
            if out.len() >= length {
                break;
            }
        }
    }
    out
}

/// Where the task is in the human workflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Created but no one has started it.
    Open,
    /// Actively being worked on.
    InProgress,
    /// Stuck on an external dependency.
    Blocked,
    /// Work is complete. Distinct from `Archived` — done tasks stay
    /// visible in dashboards; archived ones are out of sight.
    Done,
    /// Terminal — dropped, deferred indefinitely, or post-done cleanup.
    Archived,
}

/// How the local copy of the task relates to its remote counterpart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// Never pushed; lives only in the local SQLite store.
    LocalOnly,
    /// Marked for sync, not yet pushed.
    Staged,
    /// Local matches the last known remote snapshot.
    Synced,
    /// Local has uncommitted edits since the last successful sync.
    DirtyLocal,
    /// Remote has changed since the last successful sync.
    DirtyRemote,
    /// Both sides diverged — needs human resolution.
    Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    P0,
    P1,
    P2,
    P3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    BlockedBy,
    Blocks,
    DependsOn,
    Duplicates,
    ParentOf,
    ChildOf,
    RelatedTo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRelation {
    pub kind: RelationKind,
    pub other: TaskId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRef {
    pub provider: String,
    pub remote_id: String,
}

/// A comment mirrored from (or destined for) the remote issue. `remote_id`
/// is `None` for a comment authored locally that hasn't been pushed yet —
/// the outbound path (a follow-up) sets it once the remote create succeeds.
/// Comments are append-only and orthogonal to the snapshot/dirty machinery:
/// they're never part of [`TaskSnapshot`], so remote-side comment activity
/// doesn't perturb dirty detection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskComment {
    pub remote_id: Option<String>,
    pub author: String,
    pub body: String,
    pub created_at: Timestamp,
}

/// Why a snapshot was captured. Only events that confirm remote alignment
/// (`Promote` / `Push` / `Pull` / `ConflictResolve`) count toward the diff
/// baseline used by dirty detection. `LocalEdit`, `PrePull`, and
/// `Rollback` write rows into the history but don't reset the baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotSource {
    /// v1 of a freshly-created task. Distinct from `LocalEdit` so the
    /// snapshot history tells you when a task came into existence vs. when
    /// it was later revised. (Previously creations also wrote `LocalEdit`,
    /// which made `version == 1` the only way to identify the creation
    /// row — fragile once flows like `sync import` start landing v1 with
    /// source `Pull`.)
    Created,
    /// A local mutation: title/body/status/etc. edit driven by the user.
    LocalEdit,
    /// First successful remote create (`promote_to_remote`).
    Promote,
    /// Successful push of a `DirtyLocal` task.
    Push,
    /// Local state captured *before* a pull overwrites it — the undo
    /// target if the user wants to revert the pull.
    PrePull,
    /// Local state after a successful pull from remote.
    Pull,
    /// Local state after a manual merge resolution.
    ConflictResolve,
    /// Local state after a rollback applied a historical snapshot.
    Rollback,
}

impl SnapshotSource {
    /// Snapshots tagged with these sources represent a moment of remote
    /// alignment and act as the diff baseline for dirty detection.
    pub fn is_baseline(self) -> bool {
        matches!(
            self,
            SnapshotSource::Promote
                | SnapshotSource::Push
                | SnapshotSource::Pull
                | SnapshotSource::ConflictResolve
        )
    }
}

/// A point-in-time copy of a task's remote-observable state plus the
/// reason it was captured. Append-only — the sequence of snapshots for a
/// task is its full edit history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSnapshot {
    pub task_id: TaskId,
    pub version: u64,
    pub title: String,
    pub body: String,
    pub status: TaskStatus,
    pub sync_state: SyncState,
    pub priority: Priority,
    pub assignees: Vec<String>,
    pub remote: Option<RemoteRef>,
    pub source: SnapshotSource,
    pub captured_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub workspace_id: WorkspaceId,
    pub repo_id: Option<RepoId>,
    pub title: String,
    pub body: String,
    pub status: TaskStatus,
    pub sync: SyncState,
    pub priority: Priority,
    pub assignees: Vec<String>,
    pub remote: Option<RemoteRef>,
    pub relations: Vec<TaskRelation>,
    /// Comments mirrored from the remote issue (and, in a follow-up, pending
    /// outbound ones). Append-only; populated by the repository on load and
    /// reconciled by `sync pull`. Deliberately excluded from `snapshot_view`
    /// so comment activity never marks the task dirty.
    pub comments: Vec<TaskComment>,
    /// Short globally-unique hash used to assemble the friendly composite
    /// ID (`{repo.prefix}-{hash}`). Lowercase RFC 4648 base32, length 3+
    /// (grown by the persistence layer on collision). Empty string is the
    /// "not yet minted" sentinel pre-backfill; minted tasks always carry
    /// a non-empty value.
    pub hash: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The latest baseline-eligible [`TaskSnapshot`] (one of `Promote` /
    /// `Push` / `Pull` / `ConflictResolve`). Populated by the repository
    /// on load; mutated by `set_synced_baseline` after a successful
    /// sync. Drives [`Task::reconcile_dirty_against_baseline`] —
    /// idempotent edits and reverts don't gratuitously mark the task
    /// dirty.
    pub synced_baseline: Option<TaskSnapshot>,
}

impl Task {
    /// New local-only task in `Open` status. Sync state starts at `LocalOnly`.
    pub fn new_draft(
        workspace_id: WorkspaceId,
        repo_id: Option<RepoId>,
        title: String,
    ) -> Result<Self> {
        if title.trim().is_empty() {
            return Err(DomainError::validation("task title is empty"));
        }
        let now = Timestamp::now();
        Ok(Self {
            id: TaskId::new(),
            workspace_id,
            repo_id,
            title,
            body: String::new(),
            status: TaskStatus::Open,
            sync: SyncState::LocalOnly,
            priority: Priority::P3,
            assignees: Vec::new(),
            remote: None,
            relations: Vec::new(),
            comments: Vec::new(),
            // Empty until the persistence layer mints a unique base32
            // hash on first save. Domain stays agnostic to randomness
            // and DB-backed uniqueness retries.
            hash: String::new(),
            created_at: now,
            updated_at: now,
            synced_baseline: None,
        })
    }

    /// Construct a task that is already a mirror of an existing remote issue
    /// — the one-step factory `sync import` needs. Unlike the
    /// `new_draft → stage → promote` path (which *creates* the remote), this
    /// records a remote that already exists: `sync = Synced`, `remote` set,
    /// and the diff baseline captured from a `Pull` view (the remote is the
    /// source of truth from the first version). `status` mirrors the remote's
    /// open/closed bit; lifecycle and sync stay orthogonal thereafter. `hash`
    /// is left empty for the persistence layer to mint, as with `new_draft`.
    pub fn import_mirror(
        workspace_id: WorkspaceId,
        repo_id: Option<RepoId>,
        remote: RemoteRef,
        title: String,
        body: String,
        assignees: Vec<String>,
        closed: bool,
    ) -> Result<Self> {
        if title.trim().is_empty() {
            return Err(DomainError::validation("task title is empty"));
        }
        let now = Timestamp::now();
        let mut task = Self {
            id: TaskId::new(),
            workspace_id,
            repo_id,
            title,
            body,
            status: if closed {
                TaskStatus::Done
            } else {
                TaskStatus::Open
            },
            sync: SyncState::Synced,
            priority: Priority::P3,
            assignees,
            remote: Some(remote),
            relations: Vec::new(),
            comments: Vec::new(),
            hash: String::new(),
            created_at: now,
            updated_at: now,
            synced_baseline: None,
        };
        task.synced_baseline = Some(task.snapshot_view(SnapshotSource::Pull));
        Ok(task)
    }

    /// Refresh the diff baseline after a successful remote-aligning sync
    /// event (promote / push / pull / conflict resolve). The snapshot's
    /// `version` is assigned by the repository; the application layer
    /// constructs a candidate via [`Task::snapshot_view`] and the
    /// adapter persists it.
    pub fn set_synced_baseline(&mut self, baseline: TaskSnapshot) {
        self.synced_baseline = Some(baseline);
        self.reconcile_dirty_against_baseline();
    }

    /// Build a snapshot value of the task as it currently is.
    ///
    /// `version` is left as `0` — the repository is responsible for
    /// assigning the next monotonic version when persisting. The
    /// in-memory representation never needs the version to do diff
    /// comparisons; equality compares everything *except* version, so a
    /// fresh `snapshot_view` and the stored baseline still compare
    /// cleanly for dirty detection.
    pub fn snapshot_view(&self, source: SnapshotSource) -> TaskSnapshot {
        TaskSnapshot {
            task_id: self.id,
            version: 0,
            title: self.title.clone(),
            body: self.body.clone(),
            status: self.status,
            sync_state: self.sync,
            priority: self.priority,
            assignees: self.assignees.clone(),
            remote: self.remote.clone(),
            source,
            captured_at: Timestamp::now(),
        }
    }

    // --- Sync transitions ------------------------------------------------
    //
    // None of these change `status` — lifecycle and sync are orthogonal.
    // Archived tasks are still permitted to transition sync (e.g. a final
    // pull after archiving) so the daemon doesn't have to special-case
    // them; callers that want to skip Archived tasks can filter upstream.

    pub fn stage_for_sync(&mut self) -> Result<()> {
        match self.sync {
            SyncState::LocalOnly | SyncState::DirtyLocal => {
                self.sync = SyncState::Staged;
                self.touch();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot stage from sync={other:?}"
            ))),
        }
    }

    pub fn promote_to_remote(&mut self, remote: RemoteRef) -> Result<()> {
        if self.sync != SyncState::Staged {
            return Err(DomainError::transition(format!(
                "cannot promote from sync={:?}",
                self.sync
            )));
        }
        self.remote = Some(remote);
        self.sync = SyncState::Synced;
        self.touch();
        // Promote IS a remote-alignment event — capture the current state
        // as the initial baseline so subsequent edits diff correctly.
        self.synced_baseline = Some(self.snapshot_view(SnapshotSource::Promote));
        Ok(())
    }

    /// Confirm that a sync event has aligned local with remote: flips
    /// `sync` to `Synced` AND refreshes the diff baseline. The `source`
    /// records *why* this alignment happened (push / pull / conflict
    /// resolve). Must be a baseline-eligible source — `LocalEdit`,
    /// `PrePull`, and `Rollback` aren't alignment events.
    pub fn confirm_synced(&mut self, source: SnapshotSource) -> Result<()> {
        if !source.is_baseline() {
            return Err(DomainError::validation(format!(
                "confirm_synced source must be Promote/Push/Pull/ConflictResolve, got {source:?}"
            )));
        }
        match self.sync {
            SyncState::Staged | SyncState::DirtyLocal | SyncState::DirtyRemote => {
                self.sync = SyncState::Synced;
                self.touch();
                self.synced_baseline = Some(self.snapshot_view(source));
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot confirm_synced from sync={other:?}"
            ))),
        }
    }

    pub fn mark_dirty_local(&mut self) -> Result<()> {
        match self.sync {
            SyncState::Synced => {
                self.sync = SyncState::DirtyLocal;
                self.touch();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot mark dirty_local from sync={other:?}"
            ))),
        }
    }

    pub fn mark_dirty_remote(&mut self) -> Result<()> {
        match self.sync {
            SyncState::Synced => {
                self.sync = SyncState::DirtyRemote;
                self.touch();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot mark dirty_remote from sync={other:?}"
            ))),
        }
    }

    pub fn mark_conflicted(&mut self) -> Result<()> {
        match self.sync {
            SyncState::DirtyLocal | SyncState::DirtyRemote | SyncState::Synced => {
                self.sync = SyncState::Conflict;
                self.touch();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot mark conflict from sync={other:?}"
            ))),
        }
    }

    // --- Lifecycle transitions ------------------------------------------
    //
    // All mutations that change remote-observable state (lifecycle status,
    // title, body, assignees) call `reconcile_dirty_against_baseline`
    // after touching. The helper diffs against [`synced_baseline`] (the
    // last known remote state) — idempotent edits stay `Synced`, real
    // edits flip `Synced → DirtyLocal`, edits while remote is dirty
    // escalate to `Conflict`, and reverts unwind `DirtyLocal → Synced`.
    // The CLI command returns as soon as the local store is updated;
    // the daemon picks up `DirtyLocal` tasks on its next tick.

    /// Move `Open` or `Blocked` into `InProgress` (signal start of work).
    pub fn start(&mut self) -> Result<()> {
        match self.status {
            TaskStatus::Open | TaskStatus::Blocked => {
                self.status = TaskStatus::InProgress;
                self.touch();
                self.reconcile_dirty_against_baseline();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot start from status={other:?}"
            ))),
        }
    }

    /// Move into `Blocked` from `Open` or `InProgress`.
    pub fn mark_blocked(&mut self) -> Result<()> {
        match self.status {
            TaskStatus::Open | TaskStatus::InProgress => {
                self.status = TaskStatus::Blocked;
                self.touch();
                self.reconcile_dirty_against_baseline();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot block from status={other:?}"
            ))),
        }
    }

    /// Move `Blocked` back to `Open`. Caller can `start()` again explicitly.
    pub fn unblock(&mut self) -> Result<()> {
        match self.status {
            TaskStatus::Blocked => {
                self.status = TaskStatus::Open;
                self.touch();
                self.reconcile_dirty_against_baseline();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot unblock from status={other:?}"
            ))),
        }
    }

    /// Mark `InProgress` task as `Done`.
    pub fn complete(&mut self) -> Result<()> {
        match self.status {
            TaskStatus::InProgress => {
                self.status = TaskStatus::Done;
                self.touch();
                self.reconcile_dirty_against_baseline();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot complete from status={other:?}"
            ))),
        }
    }

    /// Move a `Done` task back to `Open` — used when work was marked done
    /// prematurely and needs to be reopened.
    pub fn reopen(&mut self) -> Result<()> {
        match self.status {
            TaskStatus::Done => {
                self.status = TaskStatus::Open;
                self.touch();
                self.reconcile_dirty_against_baseline();
                Ok(())
            }
            other => Err(DomainError::transition(format!(
                "cannot reopen from status={other:?}"
            ))),
        }
    }

    pub fn archive(&mut self) -> Result<()> {
        if self.status == TaskStatus::Archived {
            return Err(DomainError::transition("already archived"));
        }
        self.status = TaskStatus::Archived;
        self.touch();
        self.reconcile_dirty_against_baseline();
        Ok(())
    }

    pub fn add_relation(&mut self, kind: RelationKind, other: TaskId) {
        if !self
            .relations
            .iter()
            .any(|r| r.kind == kind && r.other == other)
        {
            self.relations.push(TaskRelation { kind, other });
            self.touch();
        }
    }

    /// Priority is **local-only metadata**: GitHub doesn't model it, so a
    /// priority change does NOT flip sync state.
    pub fn set_priority(&mut self, priority: Priority) {
        if self.priority != priority {
            self.priority = priority;
            self.touch();
        }
    }

    /// Reassign the owning repo binding. Permitted only while the task is
    /// not yet remote-backed: once promoted, the remote issue lives in a
    /// specific GitHub repo, so moving the local task to a different
    /// binding would orphan that issue. Repo ownership is local metadata
    /// (it selects the promote target), so — like `priority` — changing
    /// it does NOT flip sync state.
    pub fn set_repo_id(&mut self, repo_id: Option<RepoId>) -> Result<()> {
        // Idempotent no-op: setting the same value is always fine, even on
        // a remote-backed task — only an actual *change* is rejected.
        if self.repo_id == repo_id {
            return Ok(());
        }
        if self.remote.is_some() {
            return Err(DomainError::validation(
                "cannot reassign the repo of a task already synced to a remote issue",
            ));
        }
        self.repo_id = repo_id;
        self.touch();
        Ok(())
    }

    /// Body is mirrored to the remote issue, so editing it marks the task
    /// `DirtyLocal` (when remote-backed and currently `Synced`).
    pub fn set_body(&mut self, body: String) {
        self.body = body;
        self.touch();
        self.reconcile_dirty_against_baseline();
    }

    /// Title round-trips to the remote issue's title.
    pub fn set_title(&mut self, title: String) -> Result<()> {
        if title.trim().is_empty() {
            return Err(DomainError::validation("task title is empty"));
        }
        self.title = title;
        self.touch();
        self.reconcile_dirty_against_baseline();
        Ok(())
    }

    /// Assignees round-trip to the remote issue's assignees.
    pub fn set_assignees(&mut self, assignees: Vec<String>) {
        self.assignees = assignees;
        self.touch();
        self.reconcile_dirty_against_baseline();
    }

    pub fn is_remote_backed(&self) -> bool {
        self.remote.is_some()
    }

    /// Diff the current task against [`Task::synced_baseline`] and
    /// reconcile the sync state. Called by every mutation that touches
    /// remote-observable state.
    ///
    /// Only fields GitHub mirrors (title, body, status, assignees) count
    /// toward the diff — `priority` is local metadata, `relations` live
    /// in their own table.
    ///
    /// Transition table (assuming a baseline exists):
    ///
    /// | Before  | Differs from baseline | After |
    /// |---|---|---|
    /// | `Synced` | yes | `DirtyLocal` (standard edit) |
    /// | `Synced` | no | `Synced` (idempotent edit; no-op) |
    /// | `DirtyLocal` | no | `Synced` (revert to baseline value) |
    /// | `DirtyLocal` | yes | `DirtyLocal` |
    /// | `DirtyRemote` | yes | `Conflict` (both sides diverged) |
    /// | `Conflict` | * | `Conflict` (already worst-case) |
    /// | `Staged` | * | `Staged` (the in-flight push will read latest fields) |
    /// | `LocalOnly` | * | `LocalOnly` (no remote ⇒ baseline is None ⇒ early return) |
    ///
    /// Without a baseline (local-only task, never promoted) this is a
    /// no-op.
    pub fn reconcile_dirty_against_baseline(&mut self) {
        let Some(baseline) = &self.synced_baseline else {
            return;
        };
        let differs = self.title != baseline.title
            || self.body != baseline.body
            || self.status != baseline.status
            || !assignees_equal(&self.assignees, &baseline.assignees);
        self.sync = match (self.sync, differs) {
            (SyncState::Synced, true) => SyncState::DirtyLocal,
            (SyncState::DirtyLocal, false) => SyncState::Synced,
            (SyncState::DirtyRemote, true) => SyncState::Conflict,
            (other, _) => other,
        };
    }

    fn touch(&mut self) {
        self.updated_at = Timestamp::now();
    }
}

/// Order-insensitive assignee comparison. GitHub does not preserve the order
/// of assignees in its REST responses, so `["alice","bob"]` and `["bob","alice"]`
/// must be treated as equal to avoid spurious `DirtyLocal` transitions.
fn assignees_equal(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let a_set: std::collections::HashSet<&String> = a.iter().collect();
    let b_set: std::collections::HashSet<&String> = b.iter().collect();
    a_set == b_set
}

impl Aggregate for Task {
    type Id = TaskId;

    fn id(&self) -> Self::Id {
        self.id
    }

    fn created_at(&self) -> Timestamp {
        self.created_at
    }

    fn updated_at(&self) -> Timestamp {
        self.updated_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Task {
        Task::new_draft(WorkspaceId::new(), None, "do the thing".into()).unwrap()
    }

    fn remote_ref() -> RemoteRef {
        RemoteRef {
            provider: "github".into(),
            remote_id: "org/repo#1".into(),
        }
    }

    #[test]
    fn rejects_empty_title() {
        assert!(Task::new_draft(WorkspaceId::new(), None, "  ".into()).is_err());
    }

    #[test]
    fn happy_path_local_only_to_synced() {
        let mut t = draft();
        assert_eq!(t.status, TaskStatus::Open);
        assert_eq!(t.sync, SyncState::LocalOnly);
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        // promote_to_remote lands directly on Synced — see its doc comment.
        assert_eq!(t.sync, SyncState::Synced);
        assert!(t.is_remote_backed());
    }

    #[test]
    fn promote_requires_staged() {
        let mut t = draft();
        assert!(t.promote_to_remote(remote_ref()).is_err());
    }

    #[test]
    fn import_mirror_lands_synced_with_pull_baseline() {
        let t = Task::import_mirror(
            WorkspaceId::new(),
            None,
            remote_ref(),
            "imported".into(),
            "from remote".into(),
            vec!["alice".into()],
            true,
        )
        .unwrap();
        assert_eq!(t.sync, SyncState::Synced);
        assert_eq!(t.status, TaskStatus::Done); // closed → Done
        assert!(t.is_remote_backed());
        assert_eq!(t.assignees, vec!["alice".to_string()]);
        // Baseline captured from the remote, so a fresh import is NOT dirty.
        let baseline = t.synced_baseline.as_ref().expect("baseline set");
        assert_eq!(baseline.source, SnapshotSource::Pull);
    }

    #[test]
    fn comments_do_not_dirty_a_synced_task() {
        let mut t = draft();
        assert!(t.comments.is_empty());
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        assert_eq!(t.sync, SyncState::Synced);

        // Mirroring a comment must not perturb sync state: comments are
        // excluded from the snapshot baseline, so a subsequent reconcile
        // (here via a no-op body set) leaves the task Synced, not DirtyLocal.
        t.comments.push(TaskComment {
            remote_id: Some("7".into()),
            author: "octocat".into(),
            body: "looks good".into(),
            created_at: Timestamp::now(),
        });
        let body = t.body.clone();
        t.set_body(body);
        assert_eq!(
            t.sync,
            SyncState::Synced,
            "comment activity must not dirty a synced task"
        );
        assert_eq!(t.comments.len(), 1);
    }

    #[test]
    fn import_mirror_open_issue_maps_to_open() {
        let t = Task::import_mirror(
            WorkspaceId::new(),
            None,
            remote_ref(),
            "open one".into(),
            String::new(),
            vec![],
            false,
        )
        .unwrap();
        assert_eq!(t.status, TaskStatus::Open);
        assert_eq!(t.sync, SyncState::Synced);
    }

    #[test]
    fn import_mirror_rejects_empty_title() {
        assert!(
            Task::import_mirror(
                WorkspaceId::new(),
                None,
                remote_ref(),
                "   ".into(),
                String::new(),
                vec![],
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn dirty_local_then_resync() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        t.mark_dirty_local().unwrap();
        t.stage_for_sync().unwrap();
        assert_eq!(t.sync, SyncState::Staged);
    }

    #[test]
    fn lifecycle_and_sync_are_independent() {
        // A task can be Blocked + DirtyLocal at the same time. Blocking
        // doesn't roll back sync; staging doesn't unblock.
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        t.mark_dirty_local().unwrap();
        t.mark_blocked().unwrap();
        assert_eq!(t.status, TaskStatus::Blocked);
        assert_eq!(t.sync, SyncState::DirtyLocal);
    }

    #[test]
    fn status_transitions_open_inprogress_blocked() {
        let mut t = draft();
        t.start().unwrap();
        assert_eq!(t.status, TaskStatus::InProgress);
        t.mark_blocked().unwrap();
        assert_eq!(t.status, TaskStatus::Blocked);
        t.start().unwrap();
        assert_eq!(t.status, TaskStatus::InProgress);
        t.mark_blocked().unwrap();
        t.unblock().unwrap();
        assert_eq!(t.status, TaskStatus::Open);
    }

    #[test]
    fn cannot_start_archived_task() {
        let mut t = draft();
        t.archive().unwrap();
        assert!(t.start().is_err());
    }

    #[test]
    fn complete_requires_in_progress_and_reopen_returns_to_open() {
        let mut t = draft();
        // Can't complete an Open task — must start it first.
        assert!(t.complete().is_err());
        t.start().unwrap();
        t.complete().unwrap();
        assert_eq!(t.status, TaskStatus::Done);
        // Done is not Archived: still visible, still mutable.
        t.reopen().unwrap();
        assert_eq!(t.status, TaskStatus::Open);
    }

    #[test]
    fn done_tasks_can_still_be_archived() {
        let mut t = draft();
        t.start().unwrap();
        t.complete().unwrap();
        t.archive().unwrap();
        assert_eq!(t.status, TaskStatus::Archived);
    }

    /// The whole point of the auto-dirty behavior: a lifecycle transition
    /// on a remote-backed Synced task should enqueue a remote update via
    /// the DirtyLocal flag, so the daemon picks it up on its next tick.
    /// The CLI command itself stays synchronous (no network in the hot
    /// path).
    #[test]
    fn lifecycle_mutations_on_synced_remote_task_flip_to_dirty_local() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        assert_eq!(t.sync, SyncState::Synced);

        // Each lifecycle hop should leave the task DirtyLocal so the
        // daemon picks it up on its next push tick.
        t.start().unwrap();
        assert_eq!(t.sync, SyncState::DirtyLocal);
        // mark_synced re-syncs so we can exercise the next transition.
        t.confirm_synced(SnapshotSource::Push).unwrap();

        t.complete().unwrap();
        assert_eq!(t.sync, SyncState::DirtyLocal);
        t.confirm_synced(SnapshotSource::Push).unwrap();

        t.reopen().unwrap();
        assert_eq!(t.sync, SyncState::DirtyLocal);
        t.confirm_synced(SnapshotSource::Push).unwrap();

        t.mark_blocked().unwrap();
        assert_eq!(t.sync, SyncState::DirtyLocal);
        t.confirm_synced(SnapshotSource::Push).unwrap();

        t.unblock().unwrap();
        assert_eq!(t.sync, SyncState::DirtyLocal);
    }

    #[test]
    fn local_only_task_lifecycle_does_not_flip_sync() {
        // Without a remote, lifecycle changes leave sync at LocalOnly —
        // there's nothing to push.
        let mut t = draft();
        assert_eq!(t.sync, SyncState::LocalOnly);
        t.start().unwrap();
        t.complete().unwrap();
        assert_eq!(t.sync, SyncState::LocalOnly);
    }

    #[test]
    fn body_and_title_edits_also_flip_to_dirty_local() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();

        t.set_body("revised".into());
        assert_eq!(t.sync, SyncState::DirtyLocal);
        t.confirm_synced(SnapshotSource::Push).unwrap();

        t.set_title("new title".into()).unwrap();
        assert_eq!(t.sync, SyncState::DirtyLocal);
        t.confirm_synced(SnapshotSource::Push).unwrap();

        t.set_assignees(vec!["alice".into()]);
        assert_eq!(t.sync, SyncState::DirtyLocal);
    }

    #[test]
    fn priority_is_local_only_and_does_not_dirty() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        assert_eq!(t.sync, SyncState::Synced);
        t.set_priority(Priority::P0);
        // Priority isn't a remote field — no spurious DirtyLocal flip.
        assert_eq!(t.sync, SyncState::Synced);
    }

    #[test]
    fn set_repo_id_assigns_when_not_remote_backed() {
        let mut t = draft();
        assert_eq!(t.repo_id, None);
        let repo = RepoId::new();
        t.set_repo_id(Some(repo)).unwrap();
        assert_eq!(t.repo_id, Some(repo));
        // Repo ownership selects the promote target; it's local metadata,
        // so assigning it on a local-only task doesn't flip sync state.
        assert_eq!(t.sync, SyncState::LocalOnly);
    }

    #[test]
    fn set_repo_id_rejected_once_remote_backed() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        // The remote issue lives in a specific repo; reassigning would
        // orphan it.
        let err = t.set_repo_id(Some(RepoId::new())).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn set_repo_id_idempotent_no_op() {
        let mut t = draft();
        let repo = RepoId::new();
        t.set_repo_id(Some(repo)).unwrap();
        let before = t.updated_at;
        t.set_repo_id(Some(repo)).unwrap();
        assert_eq!(t.updated_at, before);
    }

    #[test]
    fn set_repo_id_noop_allowed_even_when_remote_backed() {
        // A no-op (same value) must succeed even after promotion — only an
        // actual *change* of repo on a remote-backed task is rejected.
        let mut t = draft();
        let repo = RepoId::new();
        t.set_repo_id(Some(repo)).unwrap();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        let before = t.updated_at;
        t.set_repo_id(Some(repo)).unwrap(); // same value → Ok, no touch
        assert_eq!(t.updated_at, before);
    }

    /// Mutating a `DirtyRemote` task is the textbook conflict case: remote
    /// had changes we hadn't pulled, and now we're piling local changes on
    /// top. Sync state must escalate to `Conflict` so the daemon (and the
    /// `query drift` view) surfaces it correctly — silently letting it
    /// look like a clean DirtyLocal would erase the "remote was dirty"
    /// signal.
    #[test]
    fn mutation_on_dirty_remote_escalates_to_conflict() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        t.mark_dirty_remote().unwrap();
        assert_eq!(t.sync, SyncState::DirtyRemote);
        t.set_body("local edit".into());
        assert_eq!(t.sync, SyncState::Conflict);
    }

    #[test]
    fn lifecycle_mutation_on_dirty_remote_also_escalates() {
        // Same rule for non-body mutations — lifecycle changes are
        // remote-observable, so they count.
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        t.mark_dirty_remote().unwrap();
        t.start().unwrap();
        assert_eq!(t.sync, SyncState::Conflict);
    }

    #[test]
    fn mutation_on_conflict_stays_conflict() {
        // Once conflicted, edits don't make things worse — but they also
        // don't accidentally "resolve" the conflict by flipping to DirtyLocal.
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        t.mark_conflicted().unwrap();
        assert_eq!(t.sync, SyncState::Conflict);
        t.set_body("trying anyway".into());
        t.start().unwrap();
        t.set_assignees(vec!["alice".into()]);
        assert_eq!(t.sync, SyncState::Conflict);
    }

    /// Reordering assignees (without any other change) must NOT produce a
    /// false-positive `DirtyLocal`. GitHub's REST API returns assignees in
    /// arbitrary order, so order has no semantic meaning.
    #[test]
    fn assignee_reorder_does_not_dirty() {
        let mut t = draft();
        t.stage_for_sync().unwrap();
        t.promote_to_remote(remote_ref()).unwrap();
        t.set_assignees(vec!["alice".into(), "bob".into()]);
        t.confirm_synced(SnapshotSource::Push).unwrap();
        assert_eq!(t.sync, SyncState::Synced);

        // Reorder only — no other field changes.
        t.set_assignees(vec!["bob".into(), "alice".into()]);
        assert_eq!(
            t.sync,
            SyncState::Synced,
            "reordering assignees must not produce DirtyLocal"
        );
    }

    #[test]
    fn relations_are_deduplicated() {
        let mut t = draft();
        let other = TaskId::new();
        t.add_relation(RelationKind::BlockedBy, other);
        t.add_relation(RelationKind::BlockedBy, other);
        assert_eq!(t.relations.len(), 1);
    }

    #[test]
    fn archive_is_terminal() {
        let mut t = draft();
        t.archive().unwrap();
        assert!(t.archive().is_err());
    }

    #[test]
    fn is_valid_hash_accepts_minted_shapes_rejects_junk() {
        assert!(is_valid_hash("ev6"));
        assert!(is_valid_hash("ak7"));
        // Grown hashes (up to MAX_HASH_LEN) must still validate, or a
        // collision-grown hash would become unresolvable.
        assert!(is_valid_hash("abcdefgh")); // 8
        assert!(is_valid_hash("abcdefghijklmnop")); // exactly MAX_HASH_LEN (16)
        // Wrong case, illegal base32 digits (0/1/8/9), wrong length.
        assert!(!is_valid_hash("EV6"));
        assert!(!is_valid_hash("ab")); // below MIN_HASH_LEN
        assert!(!is_valid_hash("abcdefghijklmnopq")); // 17, over MAX_HASH_LEN
        assert!(!is_valid_hash("ev0")); // 0 not in RFC 4648 base32
        assert!(!is_valid_hash("ev1")); // 1 not in RFC 4648 base32
        assert!(!is_valid_hash("ev-")); // hyphen
        assert!(!is_valid_hash(""));
    }

    #[test]
    fn minted_hashes_are_always_valid_hash_shaped() {
        for &length in &[3usize, 4, 5, 8] {
            let s = random_lowercase_base32(length);
            assert!(is_valid_hash(&s), "minted {s:?} failed is_valid_hash");
        }
    }

    #[test]
    fn random_lowercase_base32_fills_past_single_uuid_entropy() {
        // 30 > the ~25 chars a single UUID supplies — the function must
        // draw more entropy rather than underfilling.
        let s = random_lowercase_base32(30);
        assert_eq!(s.chars().count(), 30);
        assert!(s.chars().all(|c| matches!(c, 'a'..='z' | '2'..='7')));
    }

    #[test]
    fn random_lowercase_base32_length_and_alphabet() {
        for &length in &[3usize, 4, 5, 7] {
            let s = random_lowercase_base32(length);
            assert_eq!(
                s.chars().count(),
                length,
                "expected {length} chars, got {s:?}"
            );
            for c in s.chars() {
                assert!(
                    matches!(c, 'a'..='z' | '2'..='7'),
                    "char {c:?} is outside RFC 4648 lowercase base32"
                );
            }
        }
    }

    /// Smoke test: ten draws at length 3 produce more than one distinct
    /// value. (3-char base32 has 32^3 = 32 768 possible values; collisions
    /// across 10 draws would be astronomical bad luck.)
    #[test]
    fn random_lowercase_base32_is_actually_random() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10 {
            seen.insert(random_lowercase_base32(3));
        }
        assert!(seen.len() > 1, "10 length-3 draws produced one value");
    }
}
