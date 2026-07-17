//! Auto-derivation of the local-status → project-option mapping, run once
//! at `rl project link` time (RFC 0001 §3 D1).
//!
//! GitHub's Status options are free-text and per-project, so we seed a
//! best-effort mapping by matching option *names* against the well-known
//! vocabularies below and let the user refine the result via
//! `rl project map`.

use crate::field::FieldOption;
use crate::priority::PriorityMapping;
use crate::status::StatusMapping;
use domain_task::Priority;

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
fn first_matching<'a>(options: &'a [FieldOption], vocab: &[&str]) -> Option<&'a FieldOption> {
    options
        .iter()
        .find(|o| vocab.contains(&normalize(&o.name).as_str()))
}

fn push_mapping(out: &mut Vec<StatusMapping>, is_open: bool, opt: Option<&FieldOption>) {
    if let Some(o) = opt {
        out.push(StatusMapping {
            is_open,
            option_id: o.option_id.clone(),
        });
    }
}

/// Seed the local-lifecycle → option mapping from a freshly-fetched option
/// catalog, keyed on the open/closed bit (RFC 0004 D1):
///
/// | lifecycle | name match | fallback |
/// |---|---|---|
/// | open (`is_open = true`) | backlog / todo / open / new — else in progress / doing / wip / ready | first option |
/// | closed (`is_open = false`) | done / complete / closed / shipped | last option |
///
/// The open bucket prefers a backlog/todo column over an in-progress one (and
/// only falls back to in-progress when the board has no backlog/todo/open/new
/// column), so a freshly-created task lands in the backlog regardless of the
/// board's column order.
///
/// An empty option list yields no mappings (and `Project::new` accepts that).
///
/// The result satisfies `Project`'s invariants directly: every entry
/// references an option in `options`, and each open/closed bit appears at most
/// once. The two buckets *may* share one option (e.g. a single-option board
/// maps both open→first and closed→last to the same row) — that many-to-one
/// shape is valid by design.
pub fn derive_status_mappings(options: &[FieldOption]) -> Vec<StatusMapping> {
    if options.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();

    push_mapping(
        &mut out,
        true,
        // Prefer a true "backlog/open" column; only fall back to an
        // in-progress-style column when the board has none. `first_matching`
        // keys on board *order*, so a single fused vocab would map open tasks
        // to whichever of "Todo"/"In Progress" sits first on the board —
        // placing freshly-created tasks mid-flow on boards ordered
        // ["In Progress", "Todo", …]. Splitting the lookup makes the choice
        // order-independent: a backlog/todo column wins wherever it sits.
        first_matching(options, &["backlog", "todo", "open", "new"])
            .or_else(|| first_matching(options, &["inprogress", "doing", "wip", "ready"]))
            .or_else(|| options.first()),
    );
    push_mapping(
        &mut out,
        false,
        first_matching(options, &["done", "complete", "closed", "shipped"])
            .or_else(|| options.last()),
    );

    out
}

/// The local priorities in ordinal order. The index in this array is `k` in the
/// RFC 0006 D3 clamp formula `Pk → option[min(k, N-1)]`.
const PRIORITIES_BY_ORDINAL: [Priority; 4] =
    [Priority::P0, Priority::P1, Priority::P2, Priority::P3];

/// Seed the local-priority → option mapping from a freshly-fetched `Priority`
/// option catalog, keyed on the local `P0..P3` enum (RFC 0006 D3).
///
/// The mapping is **positional ("incremental"), never by name** — `Pk` lands on
/// the option at ordinal `k`, so it works on any board regardless of what the
/// options are called (`High/Med/Low`, `Urgent/High/Med/Low`, `P0..P3`, …). The
/// catalog is ordered by each option's declared `ordinal` first, so the result
/// follows the board's own severity order rather than whatever order the fetch
/// happened to return.
///
/// **Count mismatch** — `P0..P3` is four buckets; boards have `N`. The default
/// is to clamp: `Pk → option[min(k, N-1)]` (mirroring how `derive_status_mappings`
/// falls back to first/last). When `N < 4` this collapses two or more
/// priorities onto the last option; the caller detects that collapse (two
/// distinct priorities sharing one `option_id`) and surfaces an advisory. When
/// `N > 4` the trailing options are simply unused.
///
/// An empty option list yields no mappings (and `Project::from_fields` accepts
/// that). The result satisfies `Project`'s invariants directly: every entry
/// references an option in `options`, and each `Priority` appears exactly once.
pub fn derive_priority_mappings(options: &[FieldOption]) -> Vec<PriorityMapping> {
    if options.is_empty() {
        return Vec::new();
    }
    let mut ordered: Vec<&FieldOption> = options.iter().collect();
    ordered.sort_by_key(|o| o.ordinal);
    let last = ordered.len() - 1;
    PRIORITIES_BY_ORDINAL
        .iter()
        .enumerate()
        .map(|(k, &priority)| PriorityMapping {
            priority,
            option_id: ordered[k.min(last)].option_id.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(names: &[&str]) -> Vec<FieldOption> {
        names
            .iter()
            .enumerate()
            .map(|(i, name)| FieldOption {
                option_id: format!("o{i}"),
                name: (*name).to_string(),
                ordinal: u32::try_from(i).unwrap(),
            })
            .collect()
    }

    /// Helper: the option_id mapped to the open/closed bit, if any.
    fn mapped(m: &[StatusMapping], is_open: bool) -> Option<&str> {
        m.iter()
            .find(|x| x.is_open == is_open)
            .map(|x| x.option_id.as_str())
    }

    #[test]
    fn repo_link_shape_maps_by_name() {
        // The live `repo-link` board (RFC §2.2).
        let o = opts(&["Backlog", "Ready", "In progress", "In review", "Done"]);
        let m = derive_status_mappings(&o);
        assert_eq!(mapped(&m, true), Some("o0")); // Backlog
        assert_eq!(mapped(&m, false), Some("o4")); // Done
    }

    #[test]
    fn github_default_shape_maps_by_name() {
        // GitHub's default board (RFC §2.2): Todo → In Progress → Done.
        let o = opts(&["Todo", "In Progress", "Done"]);
        let m = derive_status_mappings(&o);
        assert_eq!(mapped(&m, true), Some("o0")); // Todo
        assert_eq!(mapped(&m, false), Some("o2")); // Done
    }

    #[test]
    fn open_prefers_backlog_over_an_earlier_in_progress_column() {
        // Board whose in-progress column sits *before* the backlog column.
        // The open bucket must still land on Backlog, not In Progress — the
        // choice is by vocabulary preference, not board order (regression: a
        // single fused open vocab + order-sensitive `first_matching` mapped
        // freshly-created open tasks onto the In Progress column here).
        let o = opts(&["In Progress", "Backlog", "Done"]);
        let m = derive_status_mappings(&o);
        assert_eq!(mapped(&m, true), Some("o1")); // Backlog, not In Progress (o0)
        assert_eq!(mapped(&m, false), Some("o2")); // Done
    }

    #[test]
    fn open_falls_back_to_in_progress_when_no_backlog_column() {
        // No backlog/todo/open/new column → the open bucket falls back to an
        // in-progress-style column before the first-option fallback.
        let o = opts(&["In Progress", "In Review", "Done"]);
        let m = derive_status_mappings(&o);
        assert_eq!(mapped(&m, true), Some("o0")); // In Progress (fallback vocab)
        assert_eq!(mapped(&m, false), Some("o2")); // Done
    }

    #[test]
    fn closed_matches_done_vocabulary() {
        // A board where the open bucket lands on the first option and the
        // closed bucket matches a Done-vocabulary name.
        let o = opts(&["New", "Doing", "Shipped"]);
        let m = derive_status_mappings(&o);
        assert_eq!(mapped(&m, true), Some("o0")); // New
        assert_eq!(mapped(&m, false), Some("o2")); // Shipped
    }

    #[test]
    fn separator_and_case_variants_normalize() {
        // "in-progress" must hit the open vocabulary as the spaced form would —
        // this is the regex `.` separator equivalence.
        let o = opts(&["in-progress", "Done"]);
        let m = derive_status_mappings(&o);
        assert_eq!(mapped(&m, true), Some("o0")); // in-progress (open vocab)
        assert_eq!(mapped(&m, false), Some("o1")); // Done
    }

    #[test]
    fn normalize_divergence_from_regex_is_locked_in() {
        // "Inprogress" (no separator) matches the open vocab — MORE lenient
        // than the regex `.`.
        let m = derive_status_mappings(&opts(&["Inprogress", "Done"]));
        assert_eq!(mapped(&m, true), Some("o0"));

        // "in.progress" (a non-separator gap char) does NOT match — STRICTER
        // than the regex `.`. The open bucket falls back to the first option.
        let m2 = derive_status_mappings(&opts(&["in.progress", "Done"]));
        assert_eq!(mapped(&m2, true), Some("o0")); // first-option fallback
        assert_eq!(mapped(&m2, false), Some("o1"));
    }

    #[test]
    fn unrecognised_names_fall_back_to_first_and_last() {
        // No name matches any vocabulary → open=first, closed=last.
        let o = opts(&["Alpha", "Beta", "Gamma"]);
        let m = derive_status_mappings(&o);
        assert_eq!(mapped(&m, true), Some("o0")); // first
        assert_eq!(mapped(&m, false), Some("o2")); // last
    }

    #[test]
    fn single_option_maps_open_and_closed_to_it() {
        // Many-to-one fallback: one column gets both open and closed.
        let o = opts(&["Only"]);
        let m = derive_status_mappings(&o);
        assert_eq!(mapped(&m, true), Some("o0"));
        assert_eq!(mapped(&m, false), Some("o0"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn empty_options_yield_no_mappings() {
        assert!(derive_status_mappings(&[]).is_empty());
    }

    #[test]
    fn anchored_match_rejects_substrings() {
        // "Not done" must NOT satisfy the Done vocabulary (anchored/full-name
        // match), so the closed bucket falls back to the last option instead.
        let o = opts(&["Open", "Not done"]);
        let m = derive_status_mappings(&o);
        assert_eq!(mapped(&m, true), Some("o0")); // Open (open vocab)
        // "Not done" normalizes to "notdone" ∉ vocab → closed falls back to last.
        assert_eq!(mapped(&m, false), Some("o1"));
    }

    // ---------- derive_priority_mappings (RFC 0006 D3) ----------------------

    /// Helper: the option_id mapped to a given priority, if any.
    fn prio_opt(m: &[PriorityMapping], p: Priority) -> Option<&str> {
        m.iter()
            .find(|x| x.priority == p)
            .map(|x| x.option_id.as_str())
    }

    #[test]
    fn priority_four_option_board_maps_one_to_one() {
        // Exact fit: P0..P3 land on options 0..3 by ordinal, name-agnostically.
        let o = opts(&["Urgent", "High", "Medium", "Low"]);
        let m = derive_priority_mappings(&o);
        assert_eq!(m.len(), 4);
        assert_eq!(prio_opt(&m, Priority::P0), Some("o0"));
        assert_eq!(prio_opt(&m, Priority::P1), Some("o1"));
        assert_eq!(prio_opt(&m, Priority::P2), Some("o2"));
        assert_eq!(prio_opt(&m, Priority::P3), Some("o3"));
    }

    #[test]
    fn priority_three_option_board_clamps_the_tail() {
        // N=3 < 4: P0→0, P1→1, P2→2, and P3 clamps onto the last option (2),
        // collapsing P2 and P3 onto the same board option.
        let o = opts(&["High", "Medium", "Low"]);
        let m = derive_priority_mappings(&o);
        assert_eq!(m.len(), 4);
        assert_eq!(prio_opt(&m, Priority::P0), Some("o0"));
        assert_eq!(prio_opt(&m, Priority::P1), Some("o1"));
        assert_eq!(prio_opt(&m, Priority::P2), Some("o2"));
        assert_eq!(prio_opt(&m, Priority::P3), Some("o2")); // clamp: P3 shares P2's option
    }

    #[test]
    fn priority_five_option_board_leaves_the_trailing_option_unused() {
        // N=5 > 4: P0..P3 map exactly to options 0..3; option 4 is never a
        // derived target.
        let o = opts(&["P0", "P1", "P2", "P3", "P4-ish"]);
        let m = derive_priority_mappings(&o);
        assert_eq!(m.len(), 4);
        assert_eq!(prio_opt(&m, Priority::P0), Some("o0"));
        assert_eq!(prio_opt(&m, Priority::P1), Some("o1"));
        assert_eq!(prio_opt(&m, Priority::P2), Some("o2"));
        assert_eq!(prio_opt(&m, Priority::P3), Some("o3"));
        assert!(
            !m.iter().any(|x| x.option_id == "o4"),
            "the trailing 5th option must stay unmapped"
        );
    }

    #[test]
    fn priority_maps_by_ordinal_not_slice_order() {
        // A board whose options arrive out of order must still map by the
        // declared `ordinal`, not by the order the fetch returned them.
        let o = vec![
            FieldOption {
                option_id: "low".into(),
                name: "Low".into(),
                ordinal: 3,
            },
            FieldOption {
                option_id: "urgent".into(),
                name: "Urgent".into(),
                ordinal: 0,
            },
            FieldOption {
                option_id: "med".into(),
                name: "Medium".into(),
                ordinal: 2,
            },
            FieldOption {
                option_id: "high".into(),
                name: "High".into(),
                ordinal: 1,
            },
        ];
        let m = derive_priority_mappings(&o);
        assert_eq!(prio_opt(&m, Priority::P0), Some("urgent")); // ordinal 0
        assert_eq!(prio_opt(&m, Priority::P1), Some("high")); // ordinal 1
        assert_eq!(prio_opt(&m, Priority::P2), Some("med")); // ordinal 2
        assert_eq!(prio_opt(&m, Priority::P3), Some("low")); // ordinal 3
    }

    #[test]
    fn priority_single_option_board_collapses_everything() {
        // Degenerate N=1: all four priorities clamp onto the one option.
        let o = opts(&["Only"]);
        let m = derive_priority_mappings(&o);
        assert_eq!(m.len(), 4);
        assert!(m.iter().all(|x| x.option_id == "o0"));
    }

    #[test]
    fn priority_empty_options_yield_no_mappings() {
        assert!(derive_priority_mappings(&[]).is_empty());
    }
}
