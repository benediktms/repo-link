//! Self-documenting AGENTS.md writer for `rl agents docs`.
//!
//! Renders a curated intro plus a clap-introspected command tree into a
//! marker-fenced block. The block is always rewritten on every run — no hash
//! comparison, no stale detection. See issue #6 for the scope decision.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Arg, ArgAction, Command};
use serde::Serialize;

const START_MARKER: &str = "<!-- rl:doc:start -->";
const END_MARKER: &str = "<!-- rl:doc:end -->";
const INTRO: &str = include_str!("agents_intro.md");

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

/// Render the curated intro followed by a recursive walk of the clap
/// subcommand tree. Returns the *inner* body of the fenced block (markers
/// are added by [`write_agents_md`]).
pub fn render_block(cmd: &Command) -> String {
    let mut out = String::new();
    out.push_str(INTRO.trim_end());
    out.push_str("\n\n## Command reference\n");
    render_subcommands(cmd, &[], &mut out);
    // Trim a trailing blank line for cleanliness; the marker writer adds one back.
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

fn render_subcommands(cmd: &Command, path: &[&str], out: &mut String) {
    for sub in cmd.get_subcommands() {
        let name = sub.get_name();
        // clap auto-generates a `help` subcommand; skip to avoid noise.
        if name == "help" {
            continue;
        }
        let mut new_path: Vec<&str> = path.to_vec();
        new_path.push(name);
        let full = new_path.join(" ");
        out.push_str(&format!("\n### `rl {full}`\n\n"));
        if let Some(about) = sub.get_about() {
            out.push_str(&format!("{about}\n\n"));
        }
        let args: Vec<&Arg> = sub
            .get_arguments()
            .filter(|a| a.get_id() != "help")
            .collect();
        if !args.is_empty() {
            out.push_str("Arguments:\n\n");
            for arg in args {
                out.push_str(&format!("- `{}`", arg_label(arg)));
                if let Some(help) = arg.get_help() {
                    out.push_str(&format!(" — {help}"));
                }
                out.push('\n');
            }
            out.push('\n');
        }
        render_subcommands(sub, &new_path, out);
    }
}

fn arg_label(arg: &Arg) -> String {
    if arg.is_positional() {
        return format!("<{}>", arg.get_id());
    }
    let mut flags: Vec<String> = Vec::new();
    if let Some(c) = arg.get_short() {
        flags.push(format!("-{c}"));
    }
    if let Some(l) = arg.get_long() {
        flags.push(format!("--{l}"));
    }
    let head = if flags.is_empty() {
        arg.get_id().to_string()
    } else {
        flags.join(", ")
    };
    let value_names: Vec<String> = if takes_value(arg) {
        arg.get_value_names()
            .map(|v| v.iter().map(|n| format!("<{n}>")).collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if value_names.is_empty() {
        head
    } else {
        format!("{head} {}", value_names.join(" "))
    }
}

/// Whether the arg's action consumes a value. Bool flags and counters do not,
/// even though `get_value_names()` returns a default placeholder for them.
fn takes_value(arg: &Arg) -> bool {
    matches!(arg.get_action(), ArgAction::Set | ArgAction::Append)
}

/// Splice `body` into the marker-fenced block in `path`, creating the file
/// (or appending the block) as needed. See issue #6 for the three modes.
pub fn write_agents_md(path: &Path, body: &str) -> Result<WriteOutcome> {
    let (final_text, action) = if !path.exists() {
        let text = format!("# AGENTS\n\n{START_MARKER}\n{body}\n{END_MARKER}\n");
        (text, Action::Created)
    } else {
        let existing =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        match (existing.find(START_MARKER), existing.find(END_MARKER)) {
            (Some(start), Some(end)) => {
                if end <= start {
                    bail!("{}: end marker appears before start marker", path.display());
                }
                let after_end = end + END_MARKER.len();
                let mut text = String::with_capacity(existing.len() + body.len());
                text.push_str(&existing[..start]);
                text.push_str(START_MARKER);
                text.push('\n');
                text.push_str(body);
                text.push('\n');
                text.push_str(END_MARKER);
                text.push_str(&existing[after_end..]);
                (text, Action::Updated)
            }
            _ => {
                let mut text = existing;
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&format!(
                    "\n## Using `rl`\n\n{START_MARKER}\n{body}\n{END_MARKER}\n"
                ));
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
}
