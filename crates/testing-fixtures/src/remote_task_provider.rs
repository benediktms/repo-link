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
    pub remote_id: String,
    pub title: Option<String>,
    pub body: Option<String>,
    pub closed: Option<bool>,
    pub state_reason: Option<RemoteStateReason>,
}

#[derive(Default)]
pub struct InMemoryRemoteTaskProvider {
    updates: Mutex<Vec<RecordedUpdate>>,
    fail_next: Mutex<u32>,
}

impl InMemoryRemoteTaskProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn fail_next(&self, n: u32) {
        *self.fail_next.lock().unwrap() = n;
    }

    pub fn updates(&self) -> Vec<RecordedUpdate> {
        self.updates.lock().unwrap().clone()
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
            remote_id: cmd.remote_id.into(),
            title: cmd.title.map(str::to_owned),
            body: cmd.body.map(str::to_owned),
            closed: cmd.closed,
            state_reason: cmd.state_reason,
        });
        Ok(RemoteTaskSnapshot {
            remote_id: cmd.remote_id.into(),
            title: cmd.title.unwrap_or("").into(),
            body: cmd.body.unwrap_or("").into(),
            closed: cmd.closed.unwrap_or(false),
            updated_at: Timestamp::now(),
            assignees: vec![],
            labels: vec![],
        })
    }

    async fn fetch_remote(&self, _: &str, _: &str) -> PortResult<RemoteTaskSnapshot> {
        Err(PortError::NotFound("fetch_remote not stubbed".into()))
    }

    async fn create_comment(&self, _: &str, _: &str, _: &str) -> PortResult<RemoteComment> {
        Err(PortError::NotFound("create_comment not stubbed".into()))
    }
}
