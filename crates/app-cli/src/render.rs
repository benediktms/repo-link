//! Output renderers: comfy-table for humans, serde_json for machines.

use application_query::{
    AssignedTaskRow, BlockedTaskRow, ContributorRow, DriftRow, ReadyTaskRow, StaleWorktreeRow,
    UnsyncedTaskRow, WorkspaceOverview,
};
use application_workspace::ReconcileSummary;
use comfy_table::{ContentArrangement, Table};
use dto_shared::{RepoBindingDto, SyncSummaryDto, TaskDto, WorkspaceDto};

fn base() -> Table {
    let mut t = Table::new();
    t.set_content_arrangement(ContentArrangement::Dynamic);
    t
}

fn short(id: &str) -> String {
    id.split('-').next().unwrap_or(id).to_string()
}

fn fmt_dt(dt: &chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

// ---------- Workspace ----------------------------------------------------

pub fn workspace(dto: &WorkspaceDto, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(dto).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["id", "name", "status", "local_only", "created_at"]);
    t.add_row(vec![
        dto.id.clone(),
        dto.name.clone(),
        dto.status.clone(),
        dto.local_only.to_string(),
        fmt_dt(&dto.created_at),
    ]);
    println!("{t}");
}

pub fn workspaces(rows: &[WorkspaceDto], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(rows).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["id", "name", "status", "local_only", "created_at"]);
    for w in rows {
        t.add_row(vec![
            short(&w.id),
            w.name.clone(),
            w.status.clone(),
            w.local_only.to_string(),
            fmt_dt(&w.created_at),
        ]);
    }
    println!("{t}");
}

// ---------- Repo binding -------------------------------------------------

pub fn repo(dto: &RepoBindingDto, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(dto).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["field", "value"]);
    t.add_row(vec!["id", &dto.id]);
    t.add_row(vec!["workspace_id", &dto.workspace_id]);
    t.add_row(vec!["remote_url", &dto.remote_url]);
    t.add_row(vec!["canonical_url", &dto.canonical_url]);
    t.add_row(vec![
        "tracked_branch",
        dto.tracked_branch.as_deref().unwrap_or("-"),
    ]);
    t.add_row(vec!["worktrees", &dto.worktrees.len().to_string()]);
    println!("{t}");

    if !dto.worktrees.is_empty() {
        let mut w = base();
        w.set_header(vec!["path", "branch", "status", "last_seen_at"]);
        for link in &dto.worktrees {
            w.add_row(vec![
                link.path.clone(),
                link.branch.clone().unwrap_or_else(|| "-".into()),
                link.status.clone(),
                fmt_dt(&link.last_seen_at),
            ]);
        }
        println!("{w}");
    }
}

pub fn discovered(rows: &[crate::DiscoveredRepo], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(rows).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["path", "canonical"]);
    for r in rows {
        t.add_row(vec![
            r.path.clone(),
            r.canonical.clone().unwrap_or_else(|| "-".into()),
        ]);
    }
    println!("{t}");
}

pub fn repos(rows: &[RepoBindingDto], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(rows).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["id", "canonical_url", "branch", "worktrees"]);
    for r in rows {
        t.add_row(vec![
            short(&r.id),
            r.canonical_url.clone(),
            r.tracked_branch.clone().unwrap_or_else(|| "-".into()),
            r.worktrees.len().to_string(),
        ]);
    }
    println!("{t}");
}

// ---------- Task ---------------------------------------------------------

pub fn task(dto: &TaskDto, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(dto).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["field", "value"]);
    t.add_row(vec!["id", &dto.id]);
    t.add_row(vec!["title", &dto.title]);
    t.add_row(vec!["state", &dto.state]);
    t.add_row(vec!["priority", &dto.priority]);
    t.add_row(vec!["workspace_id", &dto.workspace_id]);
    t.add_row(vec![
        "repo_id",
        dto.repo_id.as_deref().unwrap_or("-"),
    ]);
    t.add_row(vec![
        "remote",
        &dto.remote
            .as_ref()
            .map(|r| format!("{}#{}", r.provider, r.remote_id))
            .unwrap_or_else(|| "-".into()),
    ]);
    t.add_row(vec!["assignees", &dto.assignees.join(", ")]);
    t.add_row(vec!["relations", &dto.relations.len().to_string()]);
    t.add_row(vec!["updated_at", &fmt_dt(&dto.updated_at)]);
    println!("{t}");
}

pub fn tasks(rows: &[TaskDto], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(rows).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["id", "title", "state", "priority", "updated"]);
    for x in rows {
        t.add_row(vec![
            short(&x.id),
            x.title.clone(),
            x.state.clone(),
            x.priority.clone(),
            fmt_dt(&x.updated_at),
        ]);
    }
    println!("{t}");
}

// ---------- Query views --------------------------------------------------

pub fn overview(v: &WorkspaceOverview, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(v).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["field", "value"]);
    t.add_row(vec!["workspace", &v.workspace_name]);
    t.add_row(vec!["status", &v.status]);
    t.add_row(vec!["repos", &v.repo_count.to_string()]);
    t.add_row(vec!["worktrees", &v.worktree_count.to_string()]);
    t.add_row(vec!["stale_worktrees", &v.stale_worktree_count.to_string()]);
    t.add_row(vec!["unsynced_tasks", &v.unsynced_task_count.to_string()]);
    println!("{t}");
    if !v.task_states.is_empty() {
        let mut s = base();
        s.set_header(vec!["state", "count"]);
        for (k, n) in &v.task_states {
            s.add_row(vec![k.clone(), n.to_string()]);
        }
        println!("{s}");
    }
}

pub fn blocked(rows: &[BlockedTaskRow], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(rows).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["task_id", "title", "priority", "blocked_by"]);
    for r in rows {
        t.add_row(vec![
            short(&r.task_id),
            r.title.clone(),
            r.priority.clone(),
            r.blocked_by.iter().map(|s| short(s)).collect::<Vec<_>>().join(","),
        ]);
    }
    println!("{t}");
}

pub fn stale(rows: &[StaleWorktreeRow], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(rows).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["repo_id", "canonical_url", "path", "status"]);
    for r in rows {
        t.add_row(vec![
            short(&r.repo_id),
            r.canonical_url.clone(),
            r.path.clone(),
            r.status.clone(),
        ]);
    }
    println!("{t}");
}

pub fn sync(summary: &SyncSummaryDto, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(summary).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["field", "value"]);
    t.add_row(vec!["task_id", &summary.task_id]);
    t.add_row(vec!["previous_state", &summary.previous_state]);
    t.add_row(vec!["new_state", &summary.new_state]);
    t.add_row(vec!["decision", &summary.decision]);
    if let Some(r) = &summary.remote {
        t.add_row(vec!["remote", &format!("{}#{}", r.provider, r.remote_id)]);
    }
    println!("{t}");
}

pub fn reconcile(summary: &ReconcileSummary, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(summary).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["metric", "value"]);
    t.add_row(vec!["repos_checked", &summary.repos_checked.to_string()]);
    t.add_row(vec!["worktrees_checked", &summary.worktrees_checked.to_string()]);
    t.add_row(vec!["marked_missing", &summary.marked_missing.to_string()]);
    t.add_row(vec!["pruned", &summary.pruned.to_string()]);
    println!("{t}");
}

pub fn ready(rows: &[ReadyTaskRow], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(rows).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["task_id", "priority", "state", "assignees", "title"]);
    for r in rows {
        t.add_row(vec![
            short(&r.task_id),
            r.priority.clone(),
            r.state.clone(),
            if r.assignees.is_empty() {
                "-".into()
            } else {
                r.assignees.join(",")
            },
            r.title.clone(),
        ]);
    }
    println!("{t}");
}

pub fn assigned(rows: &[AssignedTaskRow], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(rows).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["task_id", "priority", "state", "blocked", "remote", "title"]);
    for r in rows {
        t.add_row(vec![
            short(&r.task_id),
            r.priority.clone(),
            r.state.clone(),
            if r.blocked { "yes".into() } else { "-".into() },
            r.remote_id.clone().unwrap_or_else(|| "-".into()),
            r.title.clone(),
        ]);
    }
    println!("{t}");
}

pub fn contributors(rows: &[ContributorRow], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(rows).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["assignee", "total", "by_state"]);
    for r in rows {
        let by_state = r
            .by_state
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        t.add_row(vec![r.assignee.clone(), r.total.to_string(), by_state]);
    }
    println!("{t}");
}

pub fn drift(rows: &[DriftRow], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(rows).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["task_id", "title", "state", "remote_id"]);
    for r in rows {
        t.add_row(vec![
            short(&r.task_id),
            r.title.clone(),
            r.state.clone(),
            r.remote_id.clone().unwrap_or_else(|| "-".into()),
        ]);
    }
    println!("{t}");
}

pub fn unsynced(rows: &[UnsyncedTaskRow], json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(rows).unwrap());
        return;
    }
    let mut t = base();
    t.set_header(vec!["task_id", "title", "state"]);
    for r in rows {
        t.add_row(vec![short(&r.task_id), r.title.clone(), r.state.clone()]);
    }
    println!("{t}");
}
