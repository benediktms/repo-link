//! Self-documenting AGENTS.md writer for `rl agents docs`.
//!
//! Renders a hand-curated intro plus a per-repo workspace block into a
//! marker-fenced region. The block is always rewritten on every run — no
//! hash comparison, no stale detection. See issue #6 for the scope decision.
//!
//! The command reference is deliberately not auto-generated from the clap
//! tree: we accept the small drift risk in exchange for a tailored,
//! workflow-oriented document that teaches agents how to *use* `rl` rather
//! than listing every flag. `--help` on each subcommand remains the
//! authoritative flag reference.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

const START_MARKER: &str = "<!-- rl:doc:start -->";
const END_MARKER: &str = "<!-- rl:doc:end -->";
const INTRO: &str = include_str!("agents_intro.md");

/// Wrap `body` in the start / end markers. Used by every branch of
/// [`write_agents_md`] that emits a fresh fenced block.
fn marker_block(body: &str) -> String {
    format!("{START_MARKER}\n{body}\n{END_MARKER}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Created,
    Appended,
    Updated,
}

#[derive(Debug, Serialize)]
pub struct WriteOutcome {
    pub file: PathBuf,
    pub action: Action,
    pub bytes_written: usize,
}

/// One row of "this checkout belongs to workspace X under binding Y".
/// A repo can be bound to multiple workspaces; the renderer emits one
/// entry per membership.
#[derive(Debug, Clone, Serialize)]
pub struct DocRepoMembership {
    pub workspace_id: String,
    pub workspace_name: String,
    pub binding_name: String,
    pub aliases: Vec<String>,
    /// Globally-unique short prefix for this binding (`rpl`, `bgt`, …).
    /// Visible so agents reading the footer can use the prefix as a
    /// repo locator (`--repo rpl`) or as the prefix half of friendly
    /// task IDs (`rpl-ev6`) without first running `repo show`.
    pub prefix: String,
}

/// Assemble the inner body of the fenced block: the curated intro
/// followed by the per-repo info section. Markers are added by
/// [`write_agents_md`].
pub fn render_block(repo_info: &str) -> String {
    let mut out = String::new();
    out.push_str(INTRO.trim_end());
    out.push_str("\n\n");
    out.push_str(repo_info.trim_end());
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// Render the "## This repo" section. When `memberships` is empty, emit
/// an `unbound` notice that points the agent at `rl repo attach`.
/// Otherwise emit a single fenced JSON array with one entry per
/// membership.
pub fn render_repo_info(memberships: &[DocRepoMembership], canonical_url: Option<&str>) -> String {
    let mut out = String::from("## This repo\n\n");
    if memberships.is_empty() {
        let canonical = canonical_url.unwrap_or("<not a git repo, or no `origin` remote>");
        out.push_str(&format!(
            "```\nstatus: unbound\ncanonical_url: {canonical}\nhint: run `rl repo attach --workspace <id> --url <git-remote> --canonical <canonical-url>` to bind this checkout to a workspace.\n```\n"
        ));
    } else {
        let json =
            serde_json::to_string_pretty(memberships).unwrap_or_else(|_| "[]".to_string());
        out.push_str(&format!("```json\n{json}\n```\n"));
    }
    out
}

/// Splice `body` into the marker-fenced block in `path`, creating the file
/// (or appending the block) as needed. See issue #6 for the three modes.
pub fn write_agents_md(path: &Path, body: &str) -> Result<WriteOutcome> {
    let block = marker_block(body);
    let (final_text, action) = if !path.exists() {
        (format!("# AGENTS\n\n{block}\n"), Action::Created)
    } else {
        let existing =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        match (existing.find(START_MARKER), existing.find(END_MARKER)) {
            (Some(start), Some(end)) => {
                if end <= start {
                    bail!("{}: end marker appears before start marker", path.display());
                }
                let after_end = end + END_MARKER.len();
                let mut text = String::with_capacity(existing.len() + block.len());
                text.push_str(&existing[..start]);
                text.push_str(&block);
                text.push_str(&existing[after_end..]);
                (text, Action::Updated)
            }
            (Some(_), None) => {
                bail!(
                    "{}: partial marker state detected - start marker present but end marker missing",
                    path.display()
                );
            }
            (None, Some(_)) => {
                bail!(
                    "{}: partial marker state detected - end marker present but start marker missing",
                    path.display()
                );
            }
            (None, None) => {
                let mut text = existing;
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&format!("\n## Using `rl`\n\n{block}\n"));
                (text, Action::Appended)
            }
        }
    };

    fs::write(path, &final_text).with_context(|| format!("writing {}", path.display()))?;
    Ok(WriteOutcome {
        file: path.to_path_buf(),
        action,
        bytes_written: final_text.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn creates_file_with_header_and_markers() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("AGENTS.md");
        let outcome = write_agents_md(&path, "hello").unwrap();
        assert_eq!(outcome.action, Action::Created);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# AGENTS\n"));
        assert!(text.contains(START_MARKER));
        assert!(text.contains(END_MARKER));
        assert!(text.contains("hello"));
        assert_eq!(outcome.bytes_written, text.len());
    }

    #[test]
    fn appends_block_when_file_present_without_markers() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("AGENTS.md");
        fs::write(&path, "# Existing\n\nsome notes\n").unwrap();
        let outcome = write_agents_md(&path, "body").unwrap();
        assert_eq!(outcome.action, Action::Appended);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Existing\n\nsome notes\n"));
        assert!(text.contains("## Using `rl`"));
        assert!(text.contains("body"));
    }

    #[test]
    fn updates_only_between_markers() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("AGENTS.md");
        let before = format!(
            "# Existing\n\npreamble\n\n{START_MARKER}\nold content\n{END_MARKER}\n\nepilogue\n"
        );
        fs::write(&path, &before).unwrap();
        let outcome = write_agents_md(&path, "fresh body").unwrap();
        assert_eq!(outcome.action, Action::Updated);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Existing\n\npreamble\n\n"));
        assert!(text.ends_with("\nepilogue\n"));
        assert!(text.contains("fresh body"));
        assert!(!text.contains("old content"));
    }

    #[test]
    fn second_run_is_stable() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("AGENTS.md");
        let first = write_agents_md(&path, "body").unwrap();
        assert_eq!(first.action, Action::Created);
        let after_first = fs::read_to_string(&path).unwrap();
        let second = write_agents_md(&path, "body").unwrap();
        assert_eq!(second.action, Action::Updated);
        let after_second = fs::read_to_string(&path).unwrap();
        assert_eq!(after_first, after_second);
    }

    #[test]
    fn render_repo_info_unbound_with_canonical() {
        let out = render_repo_info(&[], Some("github.com/foo/bar"));
        assert!(out.starts_with("## This repo\n\n```\n"));
        assert!(out.contains("status: unbound"));
        assert!(out.contains("canonical_url: github.com/foo/bar"));
        assert!(out.contains("rl repo attach"));
        assert!(out.trim_end().ends_with("```"));
    }

    #[test]
    fn render_repo_info_unbound_without_canonical() {
        let out = render_repo_info(&[], None);
        assert!(out.contains("status: unbound"));
        assert!(out.contains("not a git repo"));
    }

    #[test]
    fn render_repo_info_single_membership() {
        let memberships = vec![DocRepoMembership {
            workspace_id: "ws-1".to_string(),
            workspace_name: "repo-link-dev".to_string(),
            binding_name: "repo-link".to_string(),
            aliases: vec!["rl".to_string()],
            prefix: "tst".to_string(),
        }];
        let out = render_repo_info(&memberships, Some("github.com/foo/bar"));
        assert!(out.starts_with("## This repo\n\n```json\n"));
        assert!(out.contains("\"workspace_id\": \"ws-1\""));
        assert!(out.contains("\"workspace_name\": \"repo-link-dev\""));
        assert!(out.contains("\"binding_name\": \"repo-link\""));
        assert!(out.contains("\"rl\""));
        assert!(!out.contains("status: unbound"));
    }

    #[test]
    fn render_repo_info_multiple_memberships() {
        let memberships = vec![
            DocRepoMembership {
                workspace_id: "ws-1".to_string(),
                workspace_name: "alpha".to_string(),
                binding_name: "shared-repo".to_string(),
                aliases: vec![],
                prefix: "tst".to_string(),
            },
            DocRepoMembership {
                workspace_id: "ws-2".to_string(),
                workspace_name: "beta".to_string(),
                binding_name: "shared-repo".to_string(),
                aliases: vec!["sr".to_string()],
                prefix: "tst".to_string(),
            },
        ];
        let out = render_repo_info(&memberships, None);
        assert!(out.contains("\"workspace_name\": \"alpha\""));
        assert!(out.contains("\"workspace_name\": \"beta\""));
        let alpha_idx = out.find("\"alpha\"").unwrap();
        let beta_idx = out.find("\"beta\"").unwrap();
        assert!(alpha_idx < beta_idx);
    }

    #[test]
    fn render_block_includes_intro_and_repo_section() {
        let repo = render_repo_info(&[], None);
        let block = render_block(&repo);
        assert!(block.contains("`rl` (repo-link) is a local-first workspace"));
        assert!(block.contains("### Finding work"));
        assert!(block.contains("### Before you start: check drift"));
        assert!(block.contains("### Before you stop: sync your work"));
        assert!(block.contains("## This repo"));
        assert!(block.contains("status: unbound"));
        assert!(!block.contains("## Command reference"));
    }
}
