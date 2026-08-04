//! In-memory [`RemoteProjectProvider`] stub for `application-sync` drainer
//! tests. Records every call and lets a test inject the node IDs returned by
//! the write paths plus a one-shot "fail the next N calls" knob to exercise
//! the drainer's retry / dead-letter policy without standing up wiremock.

use std::sync::Mutex;

use async_trait::async_trait;
use ports::{
    ItemStatusPage, PollPage, PortError, PortResult, RemoteProjectItem, RemoteProjectProvider,
    RemoteProjectSnapshot,
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
    /// One `set_single_select_option` invocation (RFC 0006 D4). Field-agnostic
    /// on the wire — `field_id` is whichever single-select was targeted
    /// (Status or Priority); tests distinguish by comparing `field_id`
    /// against the project's known Status/Priority field ids.
    SetSingleSelectOption {
        project_node_id: String,
        item_node_id: String,
        field_id: String,
        /// `None` records a clear (`clearProjectV2ItemFieldValue`) — the
        /// custom-Type clear path (#238); `Some` records a set.
        option_id: Option<String>,
    },
    /// One `poll_project_items` invocation. The poller's tests assert the
    /// project node id, status field id, and query the daemon passes through
    /// (empty — delta-only `updated:>{since}`), plus that polling happened.
    Poll {
        project_node_id: String,
        status_field_id: String,
        query: String,
    },
    /// One `fetch_item_statuses` invocation — the live drift read. Tests
    /// assert which item ids were batched and against which Status field.
    FetchItemStatuses {
        item_node_ids: Vec<String>,
        status_field_id: String,
    },
    /// One `set_issue_type` invocation (RFC 0006 §0 A1 / #228) — the
    /// off-axis GraphQL `updateIssue` projection, distinct from
    /// `SetSingleSelectOption` (which only ever addresses a project-item
    /// field, never the issue itself). `issue_type_id: None` is a clear.
    SetIssueType {
        issue_node_id: String,
        issue_type_id: Option<String>,
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
    /// Applied `option_id` `set_single_select_option` reads back. `None`
    /// (default) echoes the sent option id (the success case GitHub
    /// returns); set it to a DIFFERENT id to drive the drainer's
    /// status-conflict tripwire.
    set_single_select_option_returns: Mutex<Option<String>>,
    /// Fail the next N mutation calls with a transient backend error before
    /// any succeed. Each failing call decrements the counter. Lets a test
    /// drive "Err under cap → reschedule" and "Err at cap → dead-letter".
    fail_next: Mutex<u32>,
    /// Items the next `poll_project_items` returns, keyed by project node id.
    /// The poller's reconcile tests inject a canned `RemoteProjectItem` list
    /// per project; an absent key polls to an empty page.
    poll_items: Mutex<std::collections::HashMap<String, Vec<RemoteProjectItem>>>,
    /// `truncated` flag the next `poll_project_items` reports, keyed by project
    /// node id. Lets the poller's tests drive a truncated page directly rather
    /// than synthesising a giant item vec. Absent key → `false` (complete).
    poll_truncated: Mutex<std::collections::HashMap<String, bool>>,
    /// Statuses `fetch_item_statuses` reports, keyed by item node id. An id the
    /// caller asks for that is absent here is left out of the returned page —
    /// the "item did not resolve" case that makes drift fall back to cache.
    item_statuses: Mutex<std::collections::HashMap<String, Option<String>>>,
}

impl InMemoryRemoteProjectProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the status `fetch_item_statuses` reports for one item. Leaving an
    /// item unseeded is how a test drives the "did not resolve" path.
    pub fn set_item_status(&self, item_node_id: &str, option_id: Option<&str>) {
        self.item_statuses
            .lock()
            .unwrap()
            .insert(item_node_id.to_string(), option_id.map(str::to_string));
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

    /// Override the `option_id` `set_single_select_option` reads back,
    /// simulating a remote that applied a DIFFERENT option than requested.
    /// Drives the drainer's `SetProjectStatus` → `Conflict` tripwire (Priority
    /// deliberately has no such tripwire — see the drainer's
    /// `SetProjectPriority` arm).
    pub fn set_single_select_option_returns(&self, option_id: &str) {
        *self.set_single_select_option_returns.lock().unwrap() = Some(option_id.to_string());
    }

    /// Make the next `n` mutation calls fail with a transient error.
    pub fn fail_next(&self, n: u32) {
        *self.fail_next.lock().unwrap() = n;
    }

    /// Inject the items a `poll_project_items(project_node_id, ..)` call
    /// returns. Used by the poller's reconcile tests.
    pub fn set_poll_items(&self, project_node_id: &str, items: Vec<RemoteProjectItem>) {
        self.poll_items
            .lock()
            .unwrap()
            .insert(project_node_id.to_string(), items);
    }

    /// Inject the `truncated` flag a `poll_project_items(project_node_id, ..)`
    /// call reports. Lets the poller's partial-page test assert the watermark
    /// is NOT advanced on a truncated read without a giant item vec.
    pub fn set_poll_truncated(&self, project_node_id: &str, truncated: bool) {
        self.poll_truncated
            .lock()
            .unwrap()
            .insert(project_node_id.to_string(), truncated);
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

    async fn set_single_select_option(
        &self,
        project_node_id: &str,
        item_node_id: &str,
        field_id: &str,
        option_id: Option<&str>,
    ) -> PortResult<Option<String>> {
        if self.should_fail() {
            return Err(PortError::Backend(
                "stub: set_single_select_option transient".into(),
            ));
        }
        self.calls
            .lock()
            .unwrap()
            .push(ProjectCall::SetSingleSelectOption {
                project_node_id: project_node_id.to_string(),
                item_node_id: item_node_id.to_string(),
                field_id: field_id.to_string(),
                option_id: option_id.map(str::to_string),
            });
        // A clear (`None`) has no option to read back — return `Ok(None)`,
        // mirroring the adapter's `clearProjectV2ItemFieldValue` path.
        let Some(option_id) = option_id else {
            return Ok(None);
        };
        // Default: echo the requested option (the remote applied it). A test
        // can override via `set_single_select_option_returns` to force a
        // mismatch.
        Ok(Some(
            self.set_single_select_option_returns
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| option_id.to_string()),
        ))
    }

    async fn fetch_item_statuses(
        &self,
        item_node_ids: &[String],
        status_field_id: &str,
    ) -> PortResult<ItemStatusPage> {
        if self.should_fail() {
            return Err(PortError::Backend("stub: item statuses transient".into()));
        }
        self.calls
            .lock()
            .unwrap()
            .push(ProjectCall::FetchItemStatuses {
                item_node_ids: item_node_ids.to_vec(),
                status_field_id: status_field_id.to_string(),
            });
        let seeded = self.item_statuses.lock().unwrap();
        let statuses = item_node_ids
            .iter()
            .filter_map(|id| seeded.get(id).map(|s| (id.clone(), s.clone())))
            .collect();
        Ok(ItemStatusPage {
            statuses,
            truncated: false,
        })
    }

    async fn poll_project_items(
        &self,
        project_node_id: &str,
        status_field_id: &str,
        query: &str,
    ) -> PortResult<PollPage> {
        if self.should_fail() {
            return Err(PortError::Backend("stub: poll transient".into()));
        }
        self.calls.lock().unwrap().push(ProjectCall::Poll {
            project_node_id: project_node_id.to_string(),
            status_field_id: status_field_id.to_string(),
            query: query.to_string(),
        });
        let items = self
            .poll_items
            .lock()
            .unwrap()
            .get(project_node_id)
            .cloned()
            .unwrap_or_default();
        let truncated = self
            .poll_truncated
            .lock()
            .unwrap()
            .get(project_node_id)
            .copied()
            .unwrap_or(false);
        Ok(PollPage { items, truncated })
    }

    async fn set_issue_type(
        &self,
        issue_node_id: &str,
        issue_type_id: Option<&str>,
    ) -> PortResult<()> {
        if self.should_fail() {
            return Err(PortError::Backend("stub: set_issue_type transient".into()));
        }
        self.calls.lock().unwrap().push(ProjectCall::SetIssueType {
            issue_node_id: issue_node_id.to_string(),
            issue_type_id: issue_type_id.map(str::to_owned),
        });
        Ok(())
    }
}
