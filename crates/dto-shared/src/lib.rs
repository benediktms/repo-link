//! dto-shared — command/query/response payloads crossing layer boundaries.
//!
//! IDs are strings here on purpose: DTOs cross JSON, SQL TEXT columns, and
//! external API responses, so they stay free of the typed `domain-core`
//! newtypes. The application layer converts at the boundary.

mod project;
mod repo;
mod sync;
mod task;
mod workspace;

pub use project::{
    LinkProjectCmd, MapStatusCmd, ProjectDto, SetWorkspaceProjectCmd, StatusMappingDto,
    StatusOptionDto,
};
pub use repo::{
    AttachRepoCmd, FilingRepoRefDto, FindRepoMatchDto, FindRepoResponseDto, LinkWorktreeCmd,
    LocateResponseDto, RepoAttachOutcomeDto, RepoBindingDto, RepoMembershipDto, UnlinkWorktreeCmd,
    WorktreeLinkDto,
};
pub use sync::{PromoteTaskCmd, PullTaskCmd, PushTaskCmd, SyncSummaryDto};
pub use task::{
    AddTaskRelationCmd, CreateTaskCmd, ImportMirrorCmd, ListTasksQuery, RemoteRefDto,
    RemoveTaskRelationCmd, TaskCommentDto, TaskDto, TaskRelationDto, UpdateTaskCmd,
};
pub use workspace::{CreateWorkspaceCmd, ListWorkspacesQuery, UpdateWorkspaceCmd, WorkspaceDto};
