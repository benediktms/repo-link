//! Domain event payload structs.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCreated {
    pub workspace_id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceArchived {
    pub workspace_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoAttached {
    pub repo_id: String,
    pub workspace_id: String,
    pub remote_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDetached {
    pub repo_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRegistered {
    pub repo_id: String,
    pub path: String,
    pub branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeMissing {
    pub repo_id: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreePruned {
    pub repo_id: String,
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCreated {
    pub task_id: String,
    pub workspace_id: String,
    pub repo_id: Option<String>,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStaged {
    pub task_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPromoted {
    pub task_id: String,
    pub provider: String,
    pub remote_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSynced {
    pub task_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDirtyLocal {
    pub task_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDirtyRemote {
    pub task_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskConflicted {
    pub task_id: String,
    pub conflict_kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBlocked {
    pub task_id: String,
    pub blocked_by: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskArchived {
    pub task_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 0002 D5 / #119 (OUT OF SCOPE for *adding* the field; cheap negative
    /// guard only): the filing repo is an INTERNAL axis and must NEVER reach
    /// the domain-event stream. The event stream stays on the LOGICAL axis —
    /// `TaskCreated` carries `repo_id` (logical) and that is the only repo
    /// identity any payload exposes. A contributor leaking `filing_repo_id`
    /// onto an event "for symmetry" trips this guard.
    #[test]
    fn task_event_payloads_omit_filing_repo_id() {
        let payloads: Vec<(&str, serde_json::Value)> = vec![
            (
                "TaskCreated",
                serde_json::to_value(TaskCreated {
                    task_id: "rpl-1".into(),
                    workspace_id: "ws-1".into(),
                    repo_id: Some("repo-1".into()),
                    title: "t".into(),
                })
                .unwrap(),
            ),
            (
                "TaskStaged",
                serde_json::to_value(TaskStaged {
                    task_id: "rpl-1".into(),
                })
                .unwrap(),
            ),
            (
                "TaskPromoted",
                serde_json::to_value(TaskPromoted {
                    task_id: "rpl-1".into(),
                    provider: "github".into(),
                    remote_id: "1".into(),
                })
                .unwrap(),
            ),
            (
                "TaskSynced",
                serde_json::to_value(TaskSynced {
                    task_id: "rpl-1".into(),
                })
                .unwrap(),
            ),
            (
                "TaskDirtyLocal",
                serde_json::to_value(TaskDirtyLocal {
                    task_id: "rpl-1".into(),
                })
                .unwrap(),
            ),
            (
                "TaskDirtyRemote",
                serde_json::to_value(TaskDirtyRemote {
                    task_id: "rpl-1".into(),
                })
                .unwrap(),
            ),
            (
                "TaskConflicted",
                serde_json::to_value(TaskConflicted {
                    task_id: "rpl-1".into(),
                    conflict_kind: "both".into(),
                })
                .unwrap(),
            ),
            (
                "TaskBlocked",
                serde_json::to_value(TaskBlocked {
                    task_id: "rpl-1".into(),
                    blocked_by: vec![],
                })
                .unwrap(),
            ),
            (
                "TaskArchived",
                serde_json::to_value(TaskArchived {
                    task_id: "rpl-1".into(),
                })
                .unwrap(),
            ),
        ];

        for (name, v) in payloads {
            let obj = v.as_object().expect("event payload is a JSON object");
            assert!(
                !obj.contains_key("filing_repo_id"),
                "{name} event payload must NOT carry the internal filing_repo_id axis \
                 — the event stream stays on the logical axis (RFC 0002 D5, #119)"
            );
        }
    }
}
