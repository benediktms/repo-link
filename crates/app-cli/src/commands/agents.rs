//! `rl agents` dispatch — drives the [`crate::docs`] AGENTS.md writer.

use anyhow::{Result, anyhow};

use crate::cli::AgentsCmd;
use crate::commands::discover_canonical_or_none;
use crate::docs;
use crate::services::Services;

pub(crate) async fn agents_dispatch(cmd: AgentsCmd, svc: &Services) -> Result<()> {
    match cmd {
        AgentsCmd::Docs => {
            let cwd = std::env::current_dir()
                .map_err(|e| anyhow!("failed to read current directory: {e}"))?;
            let abs = dunce::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());

            let canonical_url = discover_canonical_or_none(&abs)?;

            let bound = match canonical_url.as_deref() {
                Some(c) => !svc
                    .bindings
                    .memberships_for_canonical_url(c, false)
                    .await?
                    .is_empty(),
                None => false,
            };

            let repo_info = docs::render_repo_info(bound, canonical_url.as_deref());
            let body = docs::render_block(&repo_info);
            let path = abs.join("AGENTS.md");
            let outcome = docs::write_agents_md(&path, &body)?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(())
        }
    }
}
