//! dto-shared — command/query/response payloads crossing layer boundaries.
//!
//! IDs are strings here on purpose: DTOs cross JSON, SQL TEXT columns, and
//! external API responses, so they stay free of the typed `domain-core`
//! newtypes. The application layer converts at the boundary.

mod project;
mod repo;
mod search;
mod sync;
mod task;
mod workspace;

pub use project::{
    LinkProjectCmd, MapPriorityCmd, MapStatusCmd, PriorityMappingDto, PriorityOptionDto,
    ProjectDto, SetWorkspaceProjectCmd, StatusMappingDto, StatusOptionDto,
};
pub use repo::{
    AttachRepoCmd, FilingRepoRefDto, FindRepoMatchDto, FindRepoResponseDto, HereMatchDto,
    HereRepoSummaryDto, HereResponseDto, LinkWorktreeCmd, LocateResponseDto, RepoAttachOutcomeDto,
    RepoBindingDto, RepoMembershipDto, UnlinkWorktreeCmd, WorktreeLinkDto,
};
pub use search::{
    LexicalUnavailableReasonDto, MatchedSourceDto, MatchedSourceKindDto, QueryModeDto,
    SearchIndexMaintenanceDto, SearchIndexStatusDto, SearchMatchDto, SearchModelStatusDto,
    SearchResultDto, SemanticSkippedReasonDto, TaskSearchResponseDto,
};
pub use sync::{
    PromoteTaskCmd, PullTaskCmd, PushTaskCmd, RelationChange, RelationReconciledNotice,
    RelationTargetUntrackedNotice, SyncNoticeDto, SyncSummaryDto,
};
pub use task::{
    AddTaskRelationCmd, CreateTaskCmd, ImportMirrorCmd, ListTasksQuery, RemoteRefDto,
    RemoveTaskRelationCmd, TaskCommentDto, TaskDto, TaskRelationDto, UpdateTaskCmd,
};
pub use workspace::{CreateWorkspaceCmd, ListWorkspacesQuery, UpdateWorkspaceCmd, WorkspaceDto};
