//! Output helpers — the CLI always emits JSON so agents and shells get a
//! predictable, parseable shape. Human reading is via `jq` / `fx` / etc.

use application_query::{
    AssignedTaskRow, BlockedTaskRow, ChildrenRollup, ContributorRow, DriftReport, QueryNoticeDto,
    ReadyView, StaleWorktreeRow, UnsyncedTaskRow, WorkspaceOverview,
};
use application_workspace::ReconcileSummary;
use domain_task::TaskSnapshot;
use dto_shared::{
    FindRepoResponseDto, HereResponseDto, LocateResponseDto, RelationChange, RepoAttachOutcomeDto,
    RepoBindingDto, SearchIndexMaintenanceDto, SearchIndexStatusDto, SearchModelStatusDto,
    SyncNoticeDto, SyncSummaryDto, TaskDto, TaskSearchResponseDto, WorkspaceDto,
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

pub fn here(dto: &HereResponseDto) {
    print_json(dto);
}

// ---------- Task ---------------------------------------------------------

pub fn task(dto: &TaskDto) {
    print_json(dto);
}

// ---------- Task search (RFC 0007) --------------------------------------

pub fn search(dto: &TaskSearchResponseDto) {
    print_json(dto);
}

pub fn search_index_status(dto: &SearchIndexStatusDto) {
    print_json(dto);
}

pub fn search_index_maintenance(dto: &SearchIndexMaintenanceDto) {
    print_json(dto);
}

pub fn search_model_status(dto: &SearchModelStatusDto) {
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

pub fn drift(report: &DriftReport) {
    notices(&report.messages);
    print_json(report);
}

/// `rl query ready` — a nested ready frontier. This is a breaking shape change
/// from the earlier flat `[{ReadyTaskRow}]` array: output is now
/// `{workspaces: [{workspace_id, workspace_name, tree: [ReadyNode]}]}` with
/// ready tasks nested recursively under their parent task. Consumers that
/// relied on a flat row list should flatten with
/// `[.workspaces[].tree[] | .. | objects | select(has("task_id"))]` before
/// applying prior per-row logic.
pub fn ready(v: &ReadyView) {
    print_json(v);
}

pub fn assigned(rows: &[AssignedTaskRow]) {
    print_json(&rows);
}

pub fn children(rollup: &ChildrenRollup) {
    print_json(rollup);
}

// ---------- Sync / reconcile --------------------------------------------

pub fn sync(summary: &SyncSummaryDto) {
    // Caveats and notices land on stderr so the JSON on stdout stays scriptable.
    if let Some(note) = &summary.note {
        eprintln!("note: {note}");
    }
    notices(&summary.messages);
    print_json(summary);
}

/// One line of prose for a structured notice, for stderr.
///
/// The structured form stays in the stdout JSON for scripts; this is the human
/// half. It lives here rather than on the DTOs so the wire types carry no
/// prose — one source of truth per message, in the layer that renders.
/// Implemented per notice enum so a view emitting a new kind of notice gets
/// [`notices`] for free.
pub(crate) trait NoticeLine {
    fn line(&self) -> String;
}

/// Print every notice as a `note:` line on stderr, leaving stdout to the JSON.
fn notices<N: NoticeLine>(items: &[N]) {
    for n in items {
        eprintln!("note: {}", n.line());
    }
}

impl NoticeLine for QueryNoticeDto {
    fn line(&self) -> String {
        match self {
            QueryNoticeDto::DriftPartiallyLive(n) => format!(
                "{} row(s) read live from the board; {} fell back to the cached status",
                n.live_count, n.cached_count,
            ),
            QueryNoticeDto::DriftLiveUnavailable(n) => format!(
                "reporting cached board status — the live read was unavailable ({})",
                n.reason,
            ),
            QueryNoticeDto::DriftCacheNotRefreshed(n) => format!(
                "these rows are live, but the status cache could not be refreshed ({})",
                n.reason,
            ),
        }
    }
}

impl NoticeLine for SyncNoticeDto {
    fn line(&self) -> String {
        sync_notice_line(self)
    }
}

/// Format a structured sync notice into a one-line human message for stderr.
/// The structured form stays in the stdout JSON for scripts; this is the prose.
fn sync_notice_line(notice: &SyncNoticeDto) -> String {
    match notice {
        SyncNoticeDto::RelationReconciled(n) => {
            let (verb, prep) = match n.change {
                RelationChange::Added => ("added", "now"),
                RelationChange::Removed => ("removed", "was"),
            };
            format!(
                "{} edge {verb} ({prep} {}) — reconciled from the remote",
                n.relation_kind, n.other_task_id,
            )
        }
        SyncNoticeDto::RelationTargetUntracked(n) => format!(
            "remote {} target {}#{} is not tracked locally — run \
             `rl sync import https://{}/issues/{}` to replicate it",
            n.relation_kind, n.canonical_repo, n.remote_id, n.canonical_repo, n.remote_id,
        ),
    }
}

pub fn reconcile(summary: &ReconcileSummary) {
    print_json(summary);
}
