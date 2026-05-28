//! Output helpers — the CLI always emits JSON so agents and shells get a
//! predictable, parseable shape. Human reading is via `jq` / `fx` / etc.

use application_query::{
    AssignedTaskRow, BlockedTaskRow, ContributorRow, DriftRow, ReadyTaskRow, StaleWorktreeRow,
    UnsyncedTaskRow, WorkspaceOverview,
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
