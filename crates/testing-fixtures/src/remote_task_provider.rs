//! In-memory [`RemoteTaskProvider`] stub for drainer tests. Records the
//! `(closed, state_reason)` of every `update_remote` call so a test can assert
//! the drainer re-derives lifecycle correctly, with a one-shot failure knob.

use std::sync::Mutex;

use async_trait::async_trait;
use domain_core::Timestamp;
use ports::{
    PortError, PortResult, RemoteComment, RemoteStateReason, RemoteTaskCreate, RemoteTaskProvider,
    RemoteTaskSnapshot, RemoteTaskUpdate,
};

/// One recorded `update_remote`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedUpdate {
    pub canonical_repo: String,
    pub remote_id: String,
    pub title: Option<String>,
    pub body: Option<String>,
    pub closed: Option<bool>,
    pub state_reason: Option<RemoteStateReason>,
    /// Assignees forwarded to the remote. `None` ⇒ the PATCH omitted the
    /// field (leave remote unchanged); `Some(vec![])` ⇒ clear;
    /// `Some(non-empty)` ⇒ the set to apply. Mirrors
    /// [`RemoteTaskUpdate::assignees`] so drainer tests can assert the
    /// field-level PATCH shape end-to-end (rpl-x2v).
    pub assignees: Option<Vec<String>>,
}

/// One recorded relation-sync call (`add`/`remove` × sub-issue/dependency).
/// `addressed_*` is the issue the REST endpoint targets (parent / blocked);
/// `related_*` is the far end whose db id the real adapter resolves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedRelationCall {
    pub op: &'static str,
    pub addressed_canonical: String,
    pub addressed_remote_id: String,
    pub related_canonical: String,
    pub related_remote_id: String,
}

#[derive(Default)]
pub struct InMemoryRemoteTaskProvider {
    updates: Mutex<Vec<RecordedUpdate>>,
    relation_calls: Mutex<Vec<RecordedRelationCall>>,
    fail_next: Mutex<u32>,
    /// Assignees the `update_remote` response snapshot reports back. `None`
    /// (default) echoes whatever the PATCH sent (the confirming case GitHub
    /// returns); set it to a DIFFERENT list to drive the drainer's
    /// assignee-conflict tripwire (a remote that didn't apply the assignees).
    update_assignees_returns: Mutex<Option<Vec<String>>>,
}

impl InMemoryRemoteTaskProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn fail_next(&self, n: u32) {
        *self.fail_next.lock().unwrap() = n;
    }

    /// Override the assignees the `update_remote` response reports, simulating a
    /// remote that did NOT apply the PATCH's assignees (e.g. returns `[]` for a
    /// non-empty set). Drives the drainer's `UpdateRemote` → `Conflict`
    /// tripwire.
    pub fn set_update_assignees_returns(&self, assignees: Vec<String>) {
        *self.update_assignees_returns.lock().unwrap() = Some(assignees);
    }

    pub fn updates(&self) -> Vec<RecordedUpdate> {
        self.updates.lock().unwrap().clone()
    }

    pub fn relation_calls(&self) -> Vec<RecordedRelationCall> {
        self.relation_calls.lock().unwrap().clone()
    }

    fn record_relation(
        &self,
        op: &'static str,
        addressed_canonical: &str,
        addressed_remote_id: &str,
        related_canonical: &str,
        related_remote_id: &str,
    ) {
        self.relation_calls
            .lock()
            .unwrap()
            .push(RecordedRelationCall {
                op,
                addressed_canonical: addressed_canonical.into(),
                addressed_remote_id: addressed_remote_id.into(),
                related_canonical: related_canonical.into(),
                related_remote_id: related_remote_id.into(),
            });
    }

    fn should_fail(&self) -> bool {
        let mut g = self.fail_next.lock().unwrap();
        if *g > 0 {
            *g -= 1;
            true
        } else {
            false
        }
    }
}

#[async_trait]
impl RemoteTaskProvider for InMemoryRemoteTaskProvider {
    async fn create_remote(&self, cmd: RemoteTaskCreate<'_>) -> PortResult<RemoteTaskSnapshot> {
        Ok(RemoteTaskSnapshot {
            remote_id: "100".into(),
            // Mirror GitHub: a freshly created issue comes back with its
            // GraphQL node id, so promote can persist it onto the RemoteRef.
            node_id: Some("I_kwDOstub100".into()),
            title: cmd.title.into(),
            body: cmd.body.into(),
            closed: false,
            updated_at: Timestamp::now(),
            assignees: cmd.assignees.to_vec(),
            labels: cmd.labels.to_vec(),
        })
    }

    async fn update_remote(&self, cmd: RemoteTaskUpdate<'_>) -> PortResult<RemoteTaskSnapshot> {
        if self.should_fail() {
            return Err(PortError::Backend("stub: update_remote transient".into()));
        }
        self.updates.lock().unwrap().push(RecordedUpdate {
            canonical_repo: cmd.canonical_repo.into(),
            remote_id: cmd.remote_id.into(),
            title: cmd.title.map(str::to_owned),
            body: cmd.body.map(str::to_owned),
            closed: cmd.closed,
            state_reason: cmd.state_reason,
            assignees: cmd.assignees.map(|s| s.to_vec()),
        });
        // Echo the sent assignees by default (the remote applied them), so a
        // normal push confirms. An override forces a divergent return for the
        // conflict tripwire. `None` sent (field omitted) → empty response.
        let assignees = self
            .update_assignees_returns
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| cmd.assignees.map(|s| s.to_vec()).unwrap_or_default());
        Ok(RemoteTaskSnapshot {
            remote_id: cmd.remote_id.into(),
            node_id: None,
            title: cmd.title.unwrap_or("").into(),
            body: cmd.body.unwrap_or("").into(),
            closed: cmd.closed.unwrap_or(false),
            updated_at: Timestamp::now(),
            assignees,
            labels: vec![],
        })
    }

    async fn fetch_remote(&self, _: &str, _: &str) -> PortResult<RemoteTaskSnapshot> {
        Err(PortError::NotFound("fetch_remote not stubbed".into()))
    }

    async fn create_comment(&self, _: &str, _: &str, _: &str) -> PortResult<RemoteComment> {
        Err(PortError::NotFound("create_comment not stubbed".into()))
    }

    async fn add_sub_issue(
        &self,
        parent_canonical: &str,
        parent_remote_id: &str,
        child_canonical: &str,
        child_remote_id: &str,
    ) -> PortResult<()> {
        if self.should_fail() {
            return Err(PortError::Backend("stub: add_sub_issue transient".into()));
        }
        self.record_relation(
            "add_sub_issue",
            parent_canonical,
            parent_remote_id,
            child_canonical,
            child_remote_id,
        );
        Ok(())
    }

    async fn remove_sub_issue(
        &self,
        parent_canonical: &str,
        parent_remote_id: &str,
        child_canonical: &str,
        child_remote_id: &str,
    ) -> PortResult<()> {
        if self.should_fail() {
            return Err(PortError::Backend(
                "stub: remove_sub_issue transient".into(),
            ));
        }
        self.record_relation(
            "remove_sub_issue",
            parent_canonical,
            parent_remote_id,
            child_canonical,
            child_remote_id,
        );
        Ok(())
    }

    async fn add_blocked_by(
        &self,
        blocked_canonical: &str,
        blocked_remote_id: &str,
        blocker_canonical: &str,
        blocker_remote_id: &str,
    ) -> PortResult<()> {
        if self.should_fail() {
            return Err(PortError::Backend("stub: add_blocked_by transient".into()));
        }
        self.record_relation(
            "add_blocked_by",
            blocked_canonical,
            blocked_remote_id,
            blocker_canonical,
            blocker_remote_id,
        );
        Ok(())
    }

    async fn remove_blocked_by(
        &self,
        blocked_canonical: &str,
        blocked_remote_id: &str,
        blocker_canonical: &str,
        blocker_remote_id: &str,
    ) -> PortResult<()> {
        if self.should_fail() {
            return Err(PortError::Backend(
                "stub: remove_blocked_by transient".into(),
            ));
        }
        self.record_relation(
            "remove_blocked_by",
            blocked_canonical,
            blocked_remote_id,
            blocker_canonical,
            blocker_remote_id,
        );
        Ok(())
    }
}
