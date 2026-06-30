//! `rl workspace` dispatch.

use anyhow::{Result, anyhow};
use dto_shared::{CreateWorkspaceCmd, ListWorkspacesQuery, UpdateWorkspaceCmd};

use crate::cli::WorkspaceCmd;
use crate::commands::repo::{resolve_repo_handle_required, resolve_workspace};
use crate::render;
use crate::services::Services;

pub(crate) async fn workspace_dispatch(cmd: WorkspaceCmd, svc: &Services) -> Result<()> {
    match cmd {
        WorkspaceCmd::Create {
            name,
            description,
            local_only,
            project,
        } => {
            let dto = svc
                .workspaces
                .create(CreateWorkspaceCmd {
                    name,
                    description,
                    local_only,
                    project_spec: project,
                })
                .await?;
            render::workspace(&dto);
        }
        WorkspaceCmd::SetProject {
            workspace,
            project,
            none,
        } => {
            if !none && project.is_none() {
                return Err(anyhow!(
                    "rl workspace set-project requires either --project <spec> or --none"
                ));
            }
            let spec = if none { None } else { project.as_deref() };
            let workspace = resolve_workspace(svc, workspace).await?;
            let dto = svc.workspaces.set_project(&workspace, spec).await?;
            render::workspace(&dto);
        }
        WorkspaceCmd::SetFilingRepo {
            workspace,
            repo,
            none,
        } => {
            if !none && repo.is_none() {
                return Err(anyhow!(
                    "rl workspace set-filing-repo requires either --repo <handle> or --none"
                ));
            }
            let workspace = resolve_workspace(svc, workspace).await?;
            let filing_origin: Option<String> = if none {
                None
            } else {
                let instance_id = resolve_repo_handle_required(svc, &repo.unwrap()).await?;
                // The repo-handle resolver searches bindings across ALL
                // workspaces, so scope the result to the target workspace: a
                // workspace filing default must be one of THAT workspace's own
                // bindings, otherwise we would silently record a foreign binding
                // the workspace can't file into.
                let binding = svc.bindings.show(&instance_id).await?;
                if binding.workspace_id != workspace {
                    return Err(anyhow!(
                        "repo {instance_id} belongs to workspace {} — a workspace \
                         filing default must be one of this workspace's own bindings",
                        binding.workspace_id
                    ));
                }
                // RFC 0005 §D4: the filing axis lives in ORIGIN id space — the
                // promote path reads `workspace.filing_repo_id` as an origin id,
                // and the migration backfilled existing values to origin ids.
                // Store the shared ORIGIN id, not the per-workspace instance id
                // the handle resolved to (that one is the *work* axis).
                Some(binding.origin_id)
            };
            let dto = svc
                .workspaces
                .set_filing_repo(&workspace, filing_origin.as_deref())
                .await?;
            render::workspace(&dto);
        }
        WorkspaceCmd::List { include_archived } => {
            let rows = svc
                .workspaces
                .list(ListWorkspacesQuery { include_archived })
                .await?;
            render::workspaces(&rows);
        }
        WorkspaceCmd::Show { id } => render::workspace(&svc.workspaces.show(&id).await?),
        WorkspaceCmd::Edit {
            id,
            name,
            description,
        } => {
            if name.is_none() && description.is_none() {
                return Err(anyhow!(
                    "rl workspace edit requires at least one of --name, --description"
                ));
            }
            let dto = svc
                .workspaces
                .edit(UpdateWorkspaceCmd {
                    workspace_id: id,
                    name,
                    description,
                })
                .await?;
            render::workspace(&dto);
        }
        WorkspaceCmd::Activate { id } => render::workspace(&svc.workspaces.activate(&id).await?),
        WorkspaceCmd::Pause { id } => render::workspace(&svc.workspaces.pause(&id).await?),
        WorkspaceCmd::Archive { id } => render::workspace(&svc.workspaces.archive(&id).await?),
        WorkspaceCmd::Unarchive { id } => render::workspace(&svc.workspaces.unarchive(&id).await?),
    }
    Ok(())
}
