//! GitHub GraphQL adapter internals — the Projects v2 surface.
//!
//! Everything REST can't reach lives here: a project's Status field schema,
//! draft issues, project membership, and the single-select status writes.
//! GitHub exposes Projects v2 *only* over GraphQL (the REST `projects` API
//! is the sunset v1), so this module talks to `octocrab.graphql()` with
//! hand-written query strings + bespoke response structs.
//!
//! Why raw strings rather than `graphql_client` codegen (RFC 0001 §D3's
//! preferred path): the v2 surface we touch is seven small operations, all
//! enumerated in the RFC's Appendix A. A checked-in multi-megabyte
//! introspection schema + proc-macro codegen buys little against that, and
//! the RFC explicitly sanctions raw strings as the escape hatch. The
//! response is still statically typed — each operation deserializes into a
//! purpose-built struct below.

use chrono::{DateTime, SecondsFormat, Utc};
use octocrab::Octocrab;
use ports::{
    PollPage, PortError, PortResult, RemoteProjectItem, RemoteProjectSnapshot,
    RemoteProjectStatusOption,
};
use serde::Deserialize;
use serde_json::json;

use crate::rest::DEFAULT_BASE_URL;

/// Hard cap on `poll_project_items` pagination. A single tick should never
/// see more than a handful of pages (the `query:` delta filter keeps the
/// payload proportional to the change rate, RFC §D4), so this is a runaway
/// guard, not an expected limit.
const MAX_POLL_PAGES: u32 = 20;
const POLL_PAGE_SIZE: u32 = 100;

/// GraphQL client. A thin wrapper around an `octocrab` instance bound to one
/// token; [`crate::GithubAdapter`] composes this with the REST client and
/// routes the [`ports::RemoteProjectProvider`] methods through it.
pub(crate) struct GraphqlClient {
    http: Octocrab,
}

impl GraphqlClient {
    pub(crate) fn new(token: impl Into<String>, base_url: impl Into<String>) -> PortResult<Self> {
        let http = Octocrab::builder()
            .personal_token(token.into())
            .base_uri(base_url.into())
            .map_err(|e| PortError::Backend(format!("github graphql base_uri: {e}")))?
            .build()
            .map_err(|e| PortError::Backend(format!("github graphql client build: {e}")))?;
        Ok(Self { http })
    }

    #[allow(dead_code)]
    pub(crate) fn with_default_base(token: impl Into<String>) -> PortResult<Self> {
        Self::new(token, DEFAULT_BASE_URL)
    }

    /// POST a query/variables pair to `/graphql` and deserialize the `data`
    /// payload into `R`. `octocrab` unwraps `data` for us and turns a
    /// GraphQL `errors` array into [`octocrab::Error::Graphql`].
    async fn run<R: serde::de::DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> PortResult<R> {
        self.http
            .graphql(&json!({ "query": query, "variables": variables }))
            .await
            .map_err(map_gql_err)
    }
}

// ---------- Queries / mutations (RFC 0001 Appendix A) ----------------------

/// Resolve `owner/number` → project schema. Uses `repositoryOwner` + an
/// `... on ProjectV2Owner` fragment so it works for both user- and
/// organization-owned projects (Appendix A's `user(login:)` form only
/// handles users).
const FETCH_PROJECT: &str = r#"
query($owner: String!, $number: Int!) {
  repositoryOwner(login: $owner) {
    ... on ProjectV2Owner {
      projectV2(number: $number) {
        id
        number
        title
        owner { ... on User { login } ... on Organization { login } }
        fields(first: 50) {
          nodes {
            __typename
            ... on ProjectV2SingleSelectField { id name options { id name } }
          }
        }
      }
    }
  }
}"#;

const ADD_ITEM: &str = r#"
mutation($input: AddProjectV2ItemByIdInput!) {
  addProjectV2ItemById(input: $input) { item { id } }
}"#;

const CREATE_DRAFT: &str = r#"
mutation($input: AddProjectV2DraftIssueInput!) {
  addProjectV2DraftIssue(input: $input) { projectItem { id } }
}"#;

/// Resolve a `ProjectV2Item`'s node id → its `DraftIssue` content id.
/// `updateProjectV2DraftIssue` keys on the draft's id (`DI_…`), which is a
/// different node from the item id (`PVTI_…`) the port hands us.
const RESOLVE_DRAFT_ID: &str = r#"
query($id: ID!) {
  node(id: $id) {
    ... on ProjectV2Item { content { ... on DraftIssue { id } } }
  }
}"#;

const UPDATE_DRAFT: &str = r#"
mutation($input: UpdateProjectV2DraftIssueInput!) {
  updateProjectV2DraftIssue(input: $input) { draftIssue { id } }
}"#;

const CONVERT_DRAFT: &str = r#"
mutation($input: ConvertProjectV2DraftIssueItemToIssueInput!) {
  convertProjectV2DraftIssueItemToIssue(input: $input) {
    item { content { ... on Issue { id number } } }
  }
}"#;

const SET_STATUS: &str = r#"
mutation($input: UpdateProjectV2ItemFieldValueInput!) {
  updateProjectV2ItemFieldValue(input: $input) {
    projectV2Item {
      id
      fieldValues(first: 20) {
        nodes {
          __typename
          ... on ProjectV2ItemFieldSingleSelectValue {
            optionId
            field { ... on ProjectV2FieldCommon { id } }
          }
        }
      }
    }
  }
}"#;

const POLL_ITEMS: &str = r#"
query($projectId: ID!, $query: String!, $first: Int!, $after: String) {
  node(id: $projectId) {
    ... on ProjectV2 {
      items(first: $first, after: $after, query: $query) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          updatedAt
          fieldValues(first: 20) {
            nodes {
              __typename
              ... on ProjectV2ItemFieldSingleSelectValue {
                optionId
                field { ... on ProjectV2FieldCommon { id } }
              }
            }
          }
          content {
            __typename
            ... on Issue {
              id number title body state
              repository { nameWithOwner }
            }
            ... on DraftIssue { title body }
          }
        }
      }
    }
  }
}"#;

// ---------- Provider methods ----------------------------------------------

impl GraphqlClient {
    pub(crate) async fn fetch_project(
        &self,
        owner: &str,
        number: u64,
    ) -> PortResult<RemoteProjectSnapshot> {
        let data: FetchProjectData = self
            .run(FETCH_PROJECT, json!({ "owner": owner, "number": number }))
            .await?;
        let project = data
            .repository_owner
            .and_then(|o| o.project_v2)
            .ok_or_else(|| PortError::NotFound(format!("project {owner}/{number}")))?;

        // Pick the single-select field literally named "Status"; else the
        // first single-select field (RFC 0001 §3 D1).
        let single_selects: Vec<&FieldNode> = project
            .fields
            .nodes
            .iter()
            .filter(|f| f.typename == "ProjectV2SingleSelectField")
            .collect();
        let status_field = single_selects
            .iter()
            .find(|f| f.name.as_deref() == Some("Status"))
            .or_else(|| single_selects.first())
            .ok_or_else(|| {
                PortError::Backend(format!(
                    "project {owner}/{number} has no single-select field to use as Status"
                ))
            })?;

        let status_field_id = status_field
            .id
            .clone()
            .ok_or_else(|| PortError::Backend("status field missing id".into()))?;
        let status_options = status_field
            .options
            .as_deref()
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(i, o)| RemoteProjectStatusOption {
                option_id: o.id.clone(),
                name: o.name.clone(),
                ordinal: u32::try_from(i).unwrap_or(u32::MAX),
            })
            .collect();

        Ok(RemoteProjectSnapshot {
            node_id: project.id,
            number: project.number,
            title: project.title,
            owner_login: project.owner.login,
            status_field_id,
            status_options,
        })
    }

    pub(crate) async fn add_item(
        &self,
        project_node_id: &str,
        issue_node_id: &str,
    ) -> PortResult<String> {
        let data: AddItemData = self
            .run(
                ADD_ITEM,
                json!({ "input": { "projectId": project_node_id, "contentId": issue_node_id } }),
            )
            .await?;
        Ok(data.add_project_v2_item_by_id.item.id)
    }

    pub(crate) async fn create_draft_issue(
        &self,
        project_node_id: &str,
        title: &str,
        body: &str,
    ) -> PortResult<String> {
        let data: CreateDraftData = self
            .run(
                CREATE_DRAFT,
                json!({ "input": { "projectId": project_node_id, "title": title, "body": body } }),
            )
            .await?;
        Ok(data.add_project_v2_draft_issue.project_item.id)
    }

    pub(crate) async fn update_draft_issue(
        &self,
        item_node_id: &str,
        title: Option<&str>,
        body: Option<&str>,
    ) -> PortResult<()> {
        // The update mutation keys on the DraftIssue content id, not the
        // item id, so resolve it first.
        let resolved: ResolveDraftData = self
            .run(RESOLVE_DRAFT_ID, json!({ "id": item_node_id }))
            .await?;
        let draft_id = resolved
            .node
            .and_then(|n| n.content)
            .and_then(|c| c.id)
            .ok_or_else(|| {
                PortError::NotFound(format!("draft issue for project item {item_node_id}"))
            })?;

        // Only send the fields the caller supplied — an absent key leaves
        // the value unchanged, whereas an explicit null would clear it.
        let mut input = serde_json::Map::new();
        input.insert("draftIssueId".into(), json!(draft_id));
        if let Some(t) = title {
            input.insert("title".into(), json!(t));
        }
        if let Some(b) = body {
            input.insert("body".into(), json!(b));
        }
        let _: UpdateDraftData = self.run(UPDATE_DRAFT, json!({ "input": input })).await?;
        Ok(())
    }

    pub(crate) async fn convert_draft_to_issue(
        &self,
        item_node_id: &str,
        repo_node_id: &str,
    ) -> PortResult<(String, u64)> {
        let data: ConvertDraftData = self
            .run(
                CONVERT_DRAFT,
                json!({ "input": { "itemId": item_node_id, "repositoryId": repo_node_id } }),
            )
            .await?;
        // Capture BOTH the new issue's node id AND its REST `number`. The
        // number is what addresses the issue for REST/`UpdateRemote`; without
        // it the write-back would persist an issue-backed `RemoteRef` with an
        // empty `remote_id`, which `plan_mutations` would later try to push to
        // an unaddressable issue (#54).
        let content = data
            .convert_project_v2_draft_issue_item_to_issue
            .item
            .content
            .ok_or_else(|| {
                PortError::Backend(format!(
                    "convert of item {item_node_id} returned no issue content"
                ))
            })?;
        let node_id = content.id.ok_or_else(|| {
            PortError::Backend(format!(
                "convert of item {item_node_id} returned no issue node id"
            ))
        })?;
        let number = content.number.ok_or_else(|| {
            PortError::Backend(format!(
                "convert of item {item_node_id} returned no issue number"
            ))
        })?;
        Ok((node_id, number))
    }

    pub(crate) async fn set_status(
        &self,
        project_node_id: &str,
        item_node_id: &str,
        status_field_id: &str,
        option_id: &str,
    ) -> PortResult<String> {
        let data: SetStatusData = self
            .run(
                SET_STATUS,
                json!({ "input": {
                    "projectId": project_node_id,
                    "itemId": item_node_id,
                    "fieldId": status_field_id,
                    "value": { "singleSelectOptionId": option_id },
                } }),
            )
            .await?;
        // Read back the applied option from the project's chosen Status field
        // (matched by id, mirroring `map_poll_item`). The caller (drainer)
        // compares it against the sent `option_id` to detect a conflict. A
        // mutation that succeeds but returns no single-select value for the
        // field is ambiguous — surface it as a backend error so the drainer
        // retries rather than dead-lettering on a false conflict.
        data.update_project_v2_item_field_value
            .project_v2_item
            .field_values
            .nodes
            .into_iter()
            .find(|v| v.field.as_ref().and_then(|f| f.id.as_deref()) == Some(status_field_id))
            .and_then(|v| v.option_id)
            .ok_or_else(|| {
                PortError::Backend(format!(
                    "set_status on item {item_node_id} returned no single-select value for field {status_field_id}"
                ))
            })
    }

    pub(crate) async fn poll_project_items(
        &self,
        project_node_id: &str,
        status_field_id: &str,
        since: domain_core::Timestamp,
        query: &str,
    ) -> PortResult<PollPage> {
        // GitHub issue-search syntax: `updated:>` is the delta lever (RFC
        // §D4). Combine it with any caller-supplied filter (e.g. "is:open").
        let since_str = since.as_inner().to_rfc3339_opts(SecondsFormat::Secs, true);
        let search = if query.trim().is_empty() {
            format!("updated:>{since_str}")
        } else {
            format!("{} updated:>{since_str}", query.trim())
        };

        let mut out = Vec::new();
        let mut after: Option<String> = None;
        let mut truncated = true;
        for _ in 0..MAX_POLL_PAGES {
            let data: PollData = self
                .run(
                    POLL_ITEMS,
                    json!({
                        "projectId": project_node_id,
                        "query": search,
                        "first": POLL_PAGE_SIZE,
                        "after": after,
                    }),
                )
                .await?;
            let items = data
                .node
                .ok_or_else(|| PortError::NotFound(format!("project {project_node_id}")))?
                .items;
            for node in items.nodes {
                if let Some(item) = map_poll_item(node, status_field_id)? {
                    out.push(item);
                }
            }
            if items.page_info.has_next_page {
                after = items.page_info.end_cursor;
                if after.is_none() {
                    // Broken pagination metadata: more pages claimed but no
                    // cursor to fetch them. Leave `truncated` set so the
                    // warning below fires — this result IS incomplete.
                    break;
                }
            } else {
                truncated = false;
                break;
            }
        }
        if truncated {
            // Exhausted the page cap with more pages still available. The
            // `updated:>` delta filter keeps a steady-state tick well under the
            // cap, so this signals an unusually large change set; the caller
            // treats the result as partial (via `PollPage.truncated`) and
            // refetches the same window next cycle.
            tracing::warn!(
                project = project_node_id,
                max_pages = MAX_POLL_PAGES,
                "poll_project_items hit the page cap; results truncated"
            );
        }
        Ok(PollPage {
            items: out,
            truncated,
        })
    }
}

/// Map one polled GraphQL node into a [`RemoteProjectItem`]. Returns `Ok(None)`
/// for content kinds we don't model (e.g. a `PullRequest` attached to the
/// board) so the caller skips them rather than erroring.
fn map_poll_item(node: ItemNode, status_field_id: &str) -> PortResult<Option<RemoteProjectItem>> {
    let updated_at = parse_ts(&node.updated_at)?;
    // Read the option from the project's *chosen* Status field (matched by id),
    // not by the literal name "Status" — boards may name the field anything.
    let status_option_id = node
        .field_values
        .nodes
        .into_iter()
        .find(|v| v.field.as_ref().and_then(|f| f.id.as_deref()) == Some(status_field_id))
        .and_then(|v| v.option_id);
    let Some(content) = node.content else {
        return Ok(None);
    };
    let item = match content.typename.as_str() {
        "Issue" => RemoteProjectItem {
            item_node_id: node.id,
            issue_node_id: content.id,
            canonical_repo: content
                .repository
                .map(|r| format!("github.com/{}", r.name_with_owner)),
            number: content.number,
            title: content.title.unwrap_or_default(),
            body: content.body.unwrap_or_default(),
            closed: content.state.as_deref() == Some("CLOSED"),
            status_option_id,
            updated_at,
        },
        "DraftIssue" => RemoteProjectItem {
            item_node_id: node.id,
            issue_node_id: None,
            canonical_repo: None,
            number: None,
            title: content.title.unwrap_or_default(),
            body: content.body.unwrap_or_default(),
            // Drafts have no open/closed lifecycle of their own.
            closed: false,
            status_option_id,
            updated_at,
        },
        _ => return Ok(None),
    };
    Ok(Some(item))
}

fn parse_ts(s: &str) -> PortResult<domain_core::Timestamp> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| domain_core::Timestamp::from_utc(dt.with_timezone(&Utc)))
        .map_err(|e| PortError::Backend(format!("invalid updatedAt timestamp {s:?}: {e}")))
}

/// Translate an `octocrab::Error` from a GraphQL call into a [`PortError`].
/// A GraphQL-level `errors` array (bad query, permissions, rate limit) is a
/// backend-reported failure; transport/decode problems are network-class.
fn map_gql_err(e: octocrab::Error) -> PortError {
    match e {
        octocrab::Error::Graphql { source, .. } => {
            PortError::Backend(format!("github graphql: {source}"))
        }
        octocrab::Error::GitHub { source, .. } => {
            let message = source.message.clone();
            match source.status_code.as_u16() {
                404 => PortError::NotFound(message),
                code => PortError::Network(format!("github {code}: {message}")),
            }
        }
        other => PortError::Network(other.to_string()),
    }
}

// ---------- Response structs (one per operation) ---------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FetchProjectData {
    repository_owner: Option<OwnerNode>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OwnerNode {
    project_v2: Option<ProjectNode>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectNode {
    id: String,
    number: u64,
    title: String,
    owner: OwnerLogin,
    fields: FieldsConn,
}
#[derive(Deserialize)]
struct OwnerLogin {
    login: String,
}
#[derive(Deserialize)]
struct FieldsConn {
    nodes: Vec<FieldNode>,
}
#[derive(Deserialize)]
struct FieldNode {
    #[serde(rename = "__typename")]
    typename: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    options: Option<Vec<FieldOption>>,
}
#[derive(Deserialize)]
struct FieldOption {
    id: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddItemData {
    add_project_v2_item_by_id: ItemWrap,
}
#[derive(Deserialize)]
struct ItemWrap {
    item: IdNode,
}
#[derive(Deserialize)]
struct IdNode {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateDraftData {
    add_project_v2_draft_issue: ProjectItemWrap,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectItemWrap {
    project_item: IdNode,
}

#[derive(Deserialize)]
struct ResolveDraftData {
    node: Option<ItemContentNode>,
}
#[derive(Deserialize)]
struct ItemContentNode {
    content: Option<OptionalIdNode>,
}
#[derive(Deserialize)]
struct OptionalIdNode {
    #[serde(default)]
    id: Option<String>,
}

// Typed (rather than `serde_json::Value`) so a wrong response sub-shape is a
// deserialize failure rather than a silent pass — the value itself is unused.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateDraftData {
    #[allow(dead_code)]
    update_project_v2_draft_issue: DraftIssueWrap,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DraftIssueWrap {
    #[allow(dead_code)]
    draft_issue: OptionalIdNode,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConvertDraftData {
    convert_project_v2_draft_issue_item_to_issue: ConvertItemWrap,
}
#[derive(Deserialize)]
struct ConvertItemWrap {
    item: ConvertItem,
}
#[derive(Deserialize)]
struct ConvertItem {
    content: Option<ConvertIssueContent>,
}
/// The new issue's id + REST `number`, projected inline from the convert
/// mutation. `number` is load-bearing: it becomes the task's `remote_id`
/// (#54).
#[derive(Deserialize)]
struct ConvertIssueContent {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    number: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetStatusData {
    update_project_v2_item_field_value: ProjectV2ItemWrap,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectV2ItemWrap {
    project_v2_item: SetStatusItem,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetStatusItem {
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    field_values: FieldValuesConn,
}

#[derive(Deserialize)]
struct PollData {
    node: Option<PollNode>,
}
#[derive(Deserialize)]
struct PollNode {
    items: ItemsConn,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemsConn {
    page_info: PageInfo,
    nodes: Vec<ItemNode>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemNode {
    id: String,
    updated_at: String,
    #[serde(default)]
    field_values: FieldValuesConn,
    #[serde(default)]
    content: Option<ContentNode>,
}
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct FieldValuesConn {
    nodes: Vec<FieldValueNode>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FieldValueNode {
    // Non-single-select value kinds deserialize with both fields absent
    // (`None`) and are filtered out by the field-id match in `map_poll_item`.
    #[serde(default)]
    option_id: Option<String>,
    #[serde(default)]
    field: Option<FieldIdNode>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FieldIdNode {
    #[serde(default)]
    id: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentNode {
    #[serde(rename = "__typename")]
    typename: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    repository: Option<RepoRef>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoRef {
    name_with_owner: String,
}
