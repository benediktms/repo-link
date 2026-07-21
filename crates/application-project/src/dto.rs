//! Mapping from the [`Project`] aggregate to its transport [`ProjectDto`].

use domain_project::Project;
use dto_shared::{
    PriorityMappingDto, PriorityOptionDto, ProjectDto, StatusMappingDto, StatusOptionDto,
};

use crate::status::status_to_str;

pub(crate) fn project_to_dto(p: &Project) -> ProjectDto {
    let mut options: Vec<StatusOptionDto> = p
        .status_options()
        .iter()
        .map(|o| {
            let default_for = p
                .status_mappings
                .iter()
                .find(|m| m.option_id == o.option_id)
                .map(|m| status_to_str(m.is_open).to_string());
            StatusOptionDto {
                option_id: o.option_id.clone(),
                name: o.name.clone(),
                ordinal: o.ordinal,
                default_for,
            }
        })
        .collect();
    options.sort_by_key(|o| o.ordinal);
    let mut priority_options: Vec<PriorityOptionDto> = p
        .priority_options()
        .iter()
        .map(|o| PriorityOptionDto {
            option_id: o.option_id.clone(),
            name: o.name.clone(),
            ordinal: o.ordinal,
        })
        .collect();
    priority_options.sort_by_key(|o| o.ordinal);
    ProjectDto {
        id: p.id.as_str().to_string(),
        owner_login: p.owner_login.clone(),
        number: p.number,
        title: p.title.clone(),
        status_field_id: p.status_field_id().unwrap_or_default().to_string(),
        status_options: options,
        status_mappings: p
            .status_mappings
            .iter()
            .map(|m| StatusMappingDto {
                status: status_to_str(m.is_open).to_string(),
                option_id: m.option_id.clone(),
            })
            .collect(),
        priority_field_id: p.priority_field_id().map(str::to_string),
        priority_options,
        priority_mappings: p
            .priority_mappings
            .iter()
            .map(|m| PriorityMappingDto {
                priority: match m.priority {
                    domain_task::Priority::P0 => "p0",
                    domain_task::Priority::P1 => "p1",
                    domain_task::Priority::P2 => "p2",
                    domain_task::Priority::P3 => "p3",
                }
                .to_string(),
                option_id: m.option_id.clone(),
            })
            .collect(),
        archived: p.archived,
        created_at: p.created_at.into_inner(),
        updated_at: p.updated_at.into_inner(),
    }
}
