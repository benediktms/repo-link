//! In-memory [`RemoteProjectProvider`] stub for `application-sync` drainer
//! tests. Records every call and lets a test inject the node IDs returned by
//! the write paths plus a one-shot "fail the next N calls" knob to exercise
//! the drainer's retry / dead-letter policy without standing up wiremock.

use std::sync::Mutex;

use async_trait::async_trait;
use domain_core::Timestamp;
use ports::{
    PortError, PortResult, RemoteProjectItem, RemoteProjectProvider, RemoteProjectSnapshot,
};

/// One recorded mutation call, in the order it was applied. The drainer's
/// ordering tests assert against this log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectCall {
    AddItem {
        project_node_id: String,
        issue_node_id: String,
    },
    CreateDraftIssue {
        project_node_id: String,
        title: String,
        body: String,
    },
    UpdateDraftIssue {
        item_node_id: String,
        title: Option<String>,
        body: Option<String>,
    },
    ConvertDraftToIssue {
        item_node_id: String,
        repo_node_id: String,
    },
    SetStatus {
        project_node_id: String,
        item_node_id: String,
        status_field_id: String,
        option_id: String,
    },
}

#[derive(Default)]
pub struct InMemoryRemoteProjectProvider {
    calls: Mutex<Vec<ProjectCall>>,
    /// Node id `add_item` returns (the new PVTI_…). Defaults to a fixed stub.
    add_item_returns: Mutex<Option<String>>,
    /// Node id `create_draft_issue` returns.
    create_draft_returns: Mutex<Option<String>>,
    /// Issue node id + REST `number` `convert_draft_to_issue` returns.
    convert_returns: Mutex<Option<(String, u64)>>,
    /// Fail the next N mutation calls with a transient backend error before
    /// any succeed. Each failing call decrements the counter. Lets a test
    /// drive "Err under cap → reschedule" and "Err at cap → dead-letter".
    fail_next: Mutex<u32>,
}

impl InMemoryRemoteProjectProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_add_item_returns(&self, id: &str) {
        *self.add_item_returns.lock().unwrap() = Some(id.to_string());
    }

    pub fn set_create_draft_returns(&self, id: &str) {
        *self.create_draft_returns.lock().unwrap() = Some(id.to_string());
    }

    /// Set the issue node id `convert_draft_to_issue` returns; the REST
    /// `number` defaults to `1`. Use [`set_convert_returns_with_number`] when
    /// a test needs to assert the written-back `remote_id`.
    pub fn set_convert_returns(&self, id: &str) {
        *self.convert_returns.lock().unwrap() = Some((id.to_string(), 1));
    }

    /// Set both the issue node id and the REST `number`
    /// `convert_draft_to_issue` returns.
    pub fn set_convert_returns_with_number(&self, id: &str, number: u64) {
        *self.convert_returns.lock().unwrap() = Some((id.to_string(), number));
    }

    /// Make the next `n` mutation calls fail with a transient error.
    pub fn fail_next(&self, n: u32) {
        *self.fail_next.lock().unwrap() = n;
    }

    pub fn calls(&self) -> Vec<ProjectCall> {
        self.calls.lock().unwrap().clone()
    }

    /// `true` (and decrements) if this call should fail.
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
impl RemoteProjectProvider for InMemoryRemoteProjectProvider {
    async fn fetch_project(&self, _owner: &str, _number: u64) -> PortResult<RemoteProjectSnapshot> {
        Err(PortError::NotFound("fetch_project not stubbed".into()))
    }

    async fn add_item(&self, project_node_id: &str, issue_node_id: &str) -> PortResult<String> {
        if self.should_fail() {
            return Err(PortError::Backend("stub: add_item transient".into()));
        }
        self.calls.lock().unwrap().push(ProjectCall::AddItem {
            project_node_id: project_node_id.to_string(),
            issue_node_id: issue_node_id.to_string(),
        });
        Ok(self
            .add_item_returns
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "PVTI_added".to_string()))
    }

    async fn create_draft_issue(
        &self,
        project_node_id: &str,
        title: &str,
        body: &str,
    ) -> PortResult<String> {
        if self.should_fail() {
            return Err(PortError::Backend("stub: create_draft transient".into()));
        }
        self.calls
            .lock()
            .unwrap()
            .push(ProjectCall::CreateDraftIssue {
                project_node_id: project_node_id.to_string(),
                title: title.to_string(),
                body: body.to_string(),
            });
        Ok(self
            .create_draft_returns
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "PVTI_draft".to_string()))
    }

    async fn update_draft_issue(
        &self,
        item_node_id: &str,
        title: Option<&str>,
        body: Option<&str>,
    ) -> PortResult<()> {
        if self.should_fail() {
            return Err(PortError::Backend("stub: update_draft transient".into()));
        }
        self.calls
            .lock()
            .unwrap()
            .push(ProjectCall::UpdateDraftIssue {
                item_node_id: item_node_id.to_string(),
                title: title.map(str::to_owned),
                body: body.map(str::to_owned),
            });
        Ok(())
    }

    async fn convert_draft_to_issue(
        &self,
        item_node_id: &str,
        repo_node_id: &str,
    ) -> PortResult<(String, u64)> {
        if self.should_fail() {
            return Err(PortError::Backend("stub: convert transient".into()));
        }
        self.calls
            .lock()
            .unwrap()
            .push(ProjectCall::ConvertDraftToIssue {
                item_node_id: item_node_id.to_string(),
                repo_node_id: repo_node_id.to_string(),
            });
        Ok(self
            .convert_returns
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| ("I_converted".to_string(), 1)))
    }

    async fn set_status(
        &self,
        project_node_id: &str,
        item_node_id: &str,
        status_field_id: &str,
        option_id: &str,
    ) -> PortResult<()> {
        if self.should_fail() {
            return Err(PortError::Backend("stub: set_status transient".into()));
        }
        self.calls.lock().unwrap().push(ProjectCall::SetStatus {
            project_node_id: project_node_id.to_string(),
            item_node_id: item_node_id.to_string(),
            status_field_id: status_field_id.to_string(),
            option_id: option_id.to_string(),
        });
        Ok(())
    }

    async fn poll_project_items(
        &self,
        _project_node_id: &str,
        _status_field_id: &str,
        _since: Timestamp,
        _query: &str,
    ) -> PortResult<Vec<RemoteProjectItem>> {
        Ok(Vec::new())
    }
}
