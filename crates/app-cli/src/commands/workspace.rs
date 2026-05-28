//! `rl workspace` dispatch.

use anyhow::{Result, anyhow};
use dto_shared::{CreateWorkspaceCmd, ListWorkspacesQuery};

use crate::cli::WorkspaceCmd;
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
            let dto = svc.workspaces.set_project(&workspace, spec).await?;
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
        WorkspaceCmd::Activate { id } => render::workspace(&svc.workspaces.activate(&id).await?),
        WorkspaceCmd::Pause { id } => render::workspace(&svc.workspaces.pause(&id).await?),
        WorkspaceCmd::Archive { id } => render::workspace(&svc.workspaces.archive(&id).await?),
    }
    Ok(())
}
