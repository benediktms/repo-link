//! Auto-derivation of the local-status → project-option mapping, run once
//! at `rl project link` time (RFC 0001 §3 D1).
//!
//! GitHub's Status options are free-text and per-project, so we seed a
//! best-effort mapping by matching option *names* against the well-known
//! vocabularies below and let the user refine the result via
//! `rl project map`.

use crate::status::{StatusMapping, StatusOption};
use domain_task::TaskStatus;

/// Normalize an option name for vocabulary matching: lowercase and drop
/// whitespace plus `-`/`_` separators.
///
/// This is a *separator-insensitive exact match*, intentionally close to —
/// but not identical to — the RFC's anchored regexes (`^in.progress$`,
/// `^on.hold$`), avoiding a `regex` dependency for four fixed vocabularies.
/// The two differ at the edges:
/// - More lenient on separators: "In progress", "in-progress", "in_progress"
///   and even "inprogress"/"in  progress" all match, where the regex `.`
///   requires exactly one character.
/// - Stricter on non-separator gaps: "in.progress"/"inXprogress" do NOT match
///   (the regex `.` would). Such names are vanishingly rare and the user can
///   fix them with `rl project map`.
///
/// All three live board shapes (RFC §2.2) map identically under both. Note:
/// swapping in the `regex` crate later is NOT a behavior-preserving change.
fn normalize(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// The first option whose normalized name is one of `vocab`.
fn first_matching<'a>(options: &'a [StatusOption], vocab: &[&str]) -> Option<&'a StatusOption> {
    options
        .iter()
        .find(|o| vocab.contains(&normalize(&o.name).as_str()))
}

fn push_mapping(out: &mut Vec<StatusMapping>, status: TaskStatus, opt: Option<&StatusOption>) {
    if let Some(o) = opt {
        out.push(StatusMapping {
            status,
            option_id: o.option_id.clone(),
        });
    }
}

/// Seed the local-status → option mapping from a freshly-fetched option
/// catalog, following the RFC 0001 §3 table:
///
/// | local status | name match | fallback |
/// |---|---|---|
/// | `Open` | backlog / todo / open / new | first option |
/// | `InProgress` | in progress / doing / wip | — (left unmapped) |
/// | `Blocked` | blocked / on hold / waiting | — (left unmapped; the app resolves Blocked to the Open option at lookup time) |
/// | `Done` | done / complete / closed / shipped | last option |
///
/// `Archived` is never mapped — it leaves the sync surface entirely. An
/// empty option list yields no mappings (and `Project::new` accepts that).
///
/// The result satisfies `Project`'s invariants directly: every entry
/// references an option in `options`, and each status appears at most once.
/// Two statuses *may* share one option (e.g. a single-option board maps
/// both `Open`→first and `Done`→last to the same row) — that many-to-one
/// shape is valid by design.
pub fn derive_status_mappings(options: &[StatusOption]) -> Vec<StatusMapping> {
    if options.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();

    push_mapping(
        &mut out,
        TaskStatus::Open,
        first_matching(options, &["backlog", "todo", "open", "new"]).or_else(|| options.first()),
    );
    push_mapping(
        &mut out,
        TaskStatus::InProgress,
        first_matching(options, &["inprogress", "doing", "wip"]),
    );
    push_mapping(
        &mut out,
        TaskStatus::Blocked,
        first_matching(options, &["blocked", "onhold", "waiting"]),
    );
    push_mapping(
        &mut out,
        TaskStatus::Done,
        first_matching(options, &["done", "complete", "closed", "shipped"])
            .or_else(|| options.last()),
    );

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(names: &[&str]) -> Vec<StatusOption> {
        names
            .iter()
            .enumerate()
            .map(|(i, name)| StatusOption {
                option_id: format!("o{i}"),
                name: (*name).to_string(),
                ordinal: u32::try_from(i).unwrap(),
            })
            .collect()
    }

    /// Helper: the option_id mapped to `status`, if any.
    fn mapped(m: &[StatusMapping], status: TaskStatus) -> Option<&str> {
        m.iter()
            .find(|x| x.status == status)
            .map(|x| x.option_id.as_str())
    }

    #[test]
    fn repo_link_shape_maps_by_name() {
        // The live `repo-link` board (RFC §2.2): no "Blocked"-like option.
        let o = opts(&["Backlog", "Ready", "In progress", "In review", "Done"]);
        let m = derive_status_mappings(&o);
        assert_eq!(mapped(&m, TaskStatus::Open), Some("o0")); // Backlog
        assert_eq!(mapped(&m, TaskStatus::InProgress), Some("o2")); // In progress
        assert_eq!(mapped(&m, TaskStatus::Done), Some("o4")); // Done
        // No option matches the Blocked vocabulary → deliberately unmapped.
        assert_eq!(mapped(&m, TaskStatus::Blocked), None);
    }

    #[test]
    fn github_default_shape_maps_by_name() {
        // GitHub's default board (RFC §2.2): Todo → In Progress → Done.
        let o = opts(&["Todo", "In Progress", "Done"]);
        let m = derive_status_mappings(&o);
        assert_eq!(mapped(&m, TaskStatus::Open), Some("o0")); // Todo
        assert_eq!(mapped(&m, TaskStatus::InProgress), Some("o1")); // In Progress
        assert_eq!(mapped(&m, TaskStatus::Done), Some("o2")); // Done
        assert_eq!(mapped(&m, TaskStatus::Blocked), None);
    }

    #[test]
    fn fully_custom_shape_matches_all_four() {
        // A board that happens to name every vocabulary, incl. Blocked.
        let o = opts(&["New", "Doing", "Blocked", "Shipped"]);
        let m = derive_status_mappings(&o);
        assert_eq!(mapped(&m, TaskStatus::Open), Some("o0")); // New
        assert_eq!(mapped(&m, TaskStatus::InProgress), Some("o1")); // Doing
        assert_eq!(mapped(&m, TaskStatus::Blocked), Some("o2")); // Blocked
        assert_eq!(mapped(&m, TaskStatus::Done), Some("o3")); // Shipped
    }

    #[test]
    fn separator_and_case_variants_normalize() {
        // "in-progress", "ON_HOLD" etc. must hit the same vocabulary as the
        // spaced forms — this is the regex `.` separator equivalence.
        let o = opts(&["open", "in-progress", "ON_HOLD", "Done"]);
        let m = derive_status_mappings(&o);
        assert_eq!(mapped(&m, TaskStatus::Open), Some("o0"));
        assert_eq!(mapped(&m, TaskStatus::InProgress), Some("o1"));
        assert_eq!(mapped(&m, TaskStatus::Blocked), Some("o2"));
        assert_eq!(mapped(&m, TaskStatus::Done), Some("o3"));
    }

    #[test]
    fn normalize_divergence_from_regex_is_locked_in() {
        // "Inprogress" (no separator) matches — MORE lenient than the regex `.`.
        let m = derive_status_mappings(&opts(&["Backlog", "Inprogress"]));
        assert_eq!(mapped(&m, TaskStatus::InProgress), Some("o1"));

        // "in.progress" (a non-separator gap char) does NOT match — STRICTER
        // than the regex `.`. InProgress is left unmapped; Done falls back to
        // the last option.
        let m2 = derive_status_mappings(&opts(&["Backlog", "in.progress"]));
        assert_eq!(mapped(&m2, TaskStatus::InProgress), None);
        assert_eq!(mapped(&m2, TaskStatus::Done), Some("o1"));
    }

    #[test]
    fn unrecognised_names_fall_back_to_first_and_last() {
        // No name matches any vocabulary → Open=first, Done=last, the two
        // middle statuses stay unmapped.
        let o = opts(&["Alpha", "Beta", "Gamma"]);
        let m = derive_status_mappings(&o);
        assert_eq!(mapped(&m, TaskStatus::Open), Some("o0")); // first
        assert_eq!(mapped(&m, TaskStatus::Done), Some("o2")); // last
        assert_eq!(mapped(&m, TaskStatus::InProgress), None);
        assert_eq!(mapped(&m, TaskStatus::Blocked), None);
    }

    #[test]
    fn single_option_maps_open_and_done_to_it() {
        // Many-to-one fallback: one column gets both Open and Done.
        let o = opts(&["Only"]);
        let m = derive_status_mappings(&o);
        assert_eq!(mapped(&m, TaskStatus::Open), Some("o0"));
        assert_eq!(mapped(&m, TaskStatus::Done), Some("o0"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn empty_options_yield_no_mappings() {
        assert!(derive_status_mappings(&[]).is_empty());
    }

    #[test]
    fn anchored_match_rejects_substrings() {
        // "Not done" must NOT satisfy the Done vocabulary (anchored/full-name
        // match), so Done falls back to the last option instead.
        let o = opts(&["Open", "Not done"]);
        let m = derive_status_mappings(&o);
        assert_eq!(mapped(&m, TaskStatus::Open), Some("o0"));
        // "Not done" normalizes to "notdone" ∉ vocab → Done falls back to last.
        assert_eq!(mapped(&m, TaskStatus::Done), Some("o1"));
        assert_eq!(mapped(&m, TaskStatus::InProgress), None);
    }
}
