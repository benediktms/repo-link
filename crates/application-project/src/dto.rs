//! Mapping from the [`Project`] aggregate to its transport [`ProjectDto`].

use domain_project::Project;
use dto_shared::{ProjectDto, StatusMappingDto, StatusOptionDto};

use crate::status::status_to_str;

pub(crate) fn project_to_dto(p: &Project) -> ProjectDto {
    let mut options: Vec<StatusOptionDto> = p
        .status_options
        .iter()
        .map(|o| {
            let default_for = p
                .status_mappings
                .iter()
                .find(|m| m.option_id == o.option_id)
                .map(|m| status_to_str(m.status).to_string());
            StatusOptionDto {
                option_id: o.option_id.clone(),
                name: o.name.clone(),
                ordinal: o.ordinal,
                default_for,
            }
        })
        .collect();
    options.sort_by_key(|o| o.ordinal);
    ProjectDto {
        id: p.id.as_str().to_string(),
        owner_login: p.owner_login.clone(),
        number: p.number,
        title: p.title.clone(),
        status_field_id: p.status_field_id.clone(),
        status_options: options,
        status_mappings: p
            .status_mappings
            .iter()
            .map(|m| StatusMappingDto {
                status: status_to_str(m.status).to_string(),
                option_id: m.option_id.clone(),
            })
            .collect(),
        archived: p.archived,
        created_at: p.created_at.into_inner(),
        updated_at: p.updated_at.into_inner(),
    }
}
