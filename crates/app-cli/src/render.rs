//! Output helpers — the CLI always emits JSON so agents and shells get a
//! predictable, parseable shape. Human reading is via `jq` / `fx` / etc.

use application_query::{
    AssignedTaskRow, BlockedTaskRow, ChildrenRollup, ContributorRow, DriftRow, ReadyTaskRow,
    StaleWorktreeRow, UnsyncedTaskRow, WorkspaceOverview,
};
use application_workspace::ReconcileSummary;
use domain_task::TaskSnapshot;
use dto_shared::{
    FindRepoResponseDto, LocateResponseDto, RepoAttachOutcomeDto, RepoBindingDto, SyncSummaryDto,
    TaskDto, WorkspaceDto,
};
use serde::Serialize;

fn print_json<T: Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: failed to serialize output: {e}"),
    }
}

// ---------- Workspace ----------------------------------------------------

pub fn workspace(dto: &WorkspaceDto) {
    print_json(dto);
}

pub fn workspaces(rows: &[WorkspaceDto]) {
    print_json(&rows);
}

// ---------- Repo binding -------------------------------------------------

pub fn repo(dto: &RepoBindingDto) {
    print_json(dto);
}

pub fn repos(rows: &[RepoBindingDto]) {
    print_json(&rows);
}

pub fn discovered(rows: &[crate::DiscoveredRepo]) {
    print_json(&rows);
}

pub fn attach_outcome(dto: &RepoAttachOutcomeDto) {
    print_json(dto);
}

pub fn locate(dto: &LocateResponseDto) {
    print_json(dto);
}

pub fn find(dto: &FindRepoResponseDto) {
    print_json(dto);
}

// ---------- Task ---------------------------------------------------------

pub fn task(dto: &TaskDto) {
    print_json(dto);
}

pub fn tasks(rows: &[TaskDto]) {
    print_json(&rows);
}

/// Show-specific display helper (RFC 0002 D5 / #122). Serializes the base
/// `TaskDto` as usual, then injects an additive `filing_repo` key that
/// surfaces the resolved filing-repo binding (id / name / canonical_url).
/// `filing_repo` is `null` when no filing repo has been recorded yet (the
/// task was never promoted or was created before RFC 0002).
///
/// `task()` and `tasks()` (list / query) are unchanged — this path is used
/// ONLY by `rl task show`, keeping the shared `TaskDto` contract byte-
/// identical for all other consumers.
///
/// `refresh_failed` carries the `--refresh` non-fatal annotation (RFC 0004 D4):
/// `Some({at, error})` when a `--refresh` fetch failed, injected as an additive
/// `last_refresh_failed` key so the user sees the cached value WAS shown and why
/// it isn't fresher. `None` (the default `show` path, or a successful refresh)
/// omits the key entirely.
pub fn task_show(
    dto: &TaskDto,
    filing_repo: serde_json::Value,
    refresh_failed: Option<serde_json::Value>,
) {
    let mut obj = match serde_json::to_value(dto) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: failed to serialize task: {e}");
            return;
        }
    };
    if let Some(map) = obj.as_object_mut() {
        map.insert("filing_repo".to_string(), filing_repo);
        if let Some(rf) = refresh_failed {
            map.insert("last_refresh_failed".to_string(), rf);
        }
    }
    match serde_json::to_string_pretty(&obj) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: failed to serialize output: {e}"),
    }
}

pub fn snapshots(snaps: &[TaskSnapshot]) {
    print_json(&snaps);
}

// ---------- Query views --------------------------------------------------

pub fn overview(v: &WorkspaceOverview) {
    print_json(v);
}

pub fn blocked(rows: &[BlockedTaskRow]) {
    print_json(&rows);
}

pub fn stale(rows: &[StaleWorktreeRow]) {
    print_json(&rows);
}

pub fn unsynced(rows: &[UnsyncedTaskRow]) {
    print_json(&rows);
}

pub fn contributors(rows: &[ContributorRow]) {
    print_json(&rows);
}

pub fn drift(rows: &[DriftRow]) {
    print_json(&rows);
}

pub fn ready(rows: &[ReadyTaskRow]) {
    print_json(&rows);
}

pub fn assigned(rows: &[AssignedTaskRow]) {
    print_json(&rows);
}

pub fn children(rollup: &ChildrenRollup) {
    print_json(rollup);
}

// ---------- Sync / reconcile --------------------------------------------

pub fn sync(summary: &SyncSummaryDto) {
    // Caveats land on stderr so the JSON on stdout stays scriptable.
    if let Some(note) = &summary.note {
        eprintln!("note: {note}");
    }
    print_json(summary);
}

pub fn reconcile(summary: &ReconcileSummary) {
    print_json(summary);
}
