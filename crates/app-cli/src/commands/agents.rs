//! `rl agents` dispatch — drives the [`crate::docs`] AGENTS.md writer.

use anyhow::{Result, anyhow};
use infra_git::discover_canonical;

use crate::cli::AgentsCmd;
use crate::docs;
use crate::services::Services;

pub(crate) async fn agents_dispatch(cmd: AgentsCmd, svc: &Services) -> Result<()> {
    match cmd {
        AgentsCmd::Docs => {
            let cwd = std::env::current_dir()
                .map_err(|e| anyhow!("failed to read current directory: {e}"))?;
            let abs = std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());

            // Only "not a git repo" / "no origin" maps to None — other
            // errors (missing git, permission denied, etc.) surface as
            // hard failures so the agent sees the real cause.
            let canonical_url = match discover_canonical(&abs) {
                Err(infra_git::GitError::NotARepo(_)) | Ok(None) => None,
                Err(e) => return Err(anyhow!("{e}")),
                Ok(Some(c)) => Some(c),
            };

            let memberships = match canonical_url.as_deref() {
                Some(c) => svc
                    .bindings
                    .memberships_for_canonical_url(c)
                    .await?
                    .into_iter()
                    .map(|m| docs::DocRepoMembership {
                        workspace_id: m.workspace.id,
                        workspace_name: m.workspace.name,
                        binding_name: m.binding.name,
                        aliases: m.binding.aliases,
                        prefix: m.binding.prefix,
                    })
                    .collect(),
                None => Vec::new(),
            };

            let repo_info = docs::render_repo_info(&memberships, canonical_url.as_deref());
            let body = docs::render_block(&repo_info);
            let path = abs.join("AGENTS.md");
            let outcome = docs::write_agents_md(&path, &body)?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(())
        }
    }
}
