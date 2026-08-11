//! GitHub GraphQL adapter internals — the Projects v2 surface.
//!
//! Everything REST can't reach lives here: a project's field schema, draft
//! issues, project membership, and the field-agnostic single-select writes
//! (Status, Priority, RFC 0006 D4).
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

use chrono::{DateTime, Utc};
use octocrab::Octocrab;
use std::collections::HashMap;

use ports::{
    ItemStatusPage, PollPage, PortError, PortResult, RemoteIssueType, RemoteProjectField,
    RemoteProjectFieldOption, RemoteProjectItem, RemoteProjectSnapshot,
};
use serde::Deserialize;
use serde_json::json;

use crate::rest::DEFAULT_BASE_URL;

/// Hard cap on `poll_project_items` pagination. There is no server-side delta
/// filter (#208), so a tick enumerates the whole board: this caps the page walk
/// at MAX_POLL_PAGES * POLL_PAGE_SIZE items (2000) — comfortably above real
/// boards, and a runaway guard rather than an expected limit. A board larger
/// than that reports `truncated` and the poller refetches next cycle.
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

/// Fetch an owner's org-level native issue-type catalog (RFC 0006 D5/D8).
/// Reaches the owner via `repositoryOwner(login:)` + an `... on Organization`
/// fragment — the same technique as `FETCH_PROJECT` — so a user-owned owner
/// (personal account, no `issueTypes`) OR a missing owner both deserialize to
/// an empty set rather than raising a GraphQL error. That is what keeps the D8
/// "type unavailable" case error-free.
const FETCH_ORG_ISSUE_TYPES: &str = r#"
query($owner: String!) {
  repositoryOwner(login: $owner) {
    __typename
    ... on Organization {
      issueTypes(first: 100) {
        nodes { id name }
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

/// GraphQL `updateIssue` — the native issue-level "Type" field (RFC 0006 §0
/// A1 / #228). NOT the Projects v2 single-select rail: `issueType` lives on
/// the `Issue` itself, set via `updateIssue(input: { id, issueTypeId })`, so
/// this is a dedicated mutation rather than a `set_single_select_option`
/// caller. `issueTypeId: null` clears the type — the caller always sends the
/// key (never omits it) so `None` maps to an explicit JSON `null`.
const SET_ISSUE_TYPE: &str = r#"
mutation($input: UpdateIssueInput!) {
  updateIssue(input: $input) {
    issue { id issueType { id } }
  }
}"#;

/// GraphQL `transferIssue` — move an issue to another repository (#71). REST
/// has no transfer endpoint, so this is the only forward path; `rest.rs` only
/// *detects* a transfer somebody else made. The response carries the
/// destination issue's reissued node id and its new per-repo number, both of
/// which the caller must persist.
const TRANSFER_ISSUE: &str = r#"
mutation($input: TransferIssueInput!) {
  transferIssue(input: $input) {
    issue { id number }
  }
}"#;

const SET_SINGLE_SELECT_OPTION: &str = r#"
mutation($input: UpdateProjectV2ItemFieldValueInput!) {
  updateProjectV2ItemFieldValue(input: $input) {
    projectV2Item {
      id
      fieldValues(first: 100) {
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

// RFC 0006 #238 — clear a single-select field value on a project item (the
// custom-Type clear path). `updateProjectV2ItemFieldValue` cannot express an
// empty single-select, so clearing needs this dedicated mutation. No option to
// read back — the caller returns `Ok(None)` on success.
const CLEAR_SINGLE_SELECT_OPTION: &str = r#"
mutation($input: ClearProjectV2ItemFieldValueInput!) {
  clearProjectV2ItemFieldValue(input: $input) {
    projectV2Item { id }
  }
}"#;

/// Batched read of specific items' single-select values. Addressed by node id
/// rather than by walking the board, so `rl query drift` pays for the tasks it
/// is checking rather than for the project's size. Deliberately omits
/// `content` — drift compares status only, and the local task already holds
/// everything else.
const ITEM_STATUSES: &str = r#"
query($ids: [ID!]!) {
  nodes(ids: $ids) {
    __typename
    ... on ProjectV2Item {
      id
      fieldValues(first: 100) {
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
query($projectId: ID!, $query: String, $first: Int!, $after: String) {
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

        // Retain EVERY single-select field (RFC 0006 D2) — don't collapse to
        // one Status field here. Which one drives lifecycle is a domain concern
        // (named matching over the retained set, at link time), so the adapter
        // just maps the wire shape faithfully. A project with no single-select
        // field yields an empty `fields` (a valid wire state); the "no field to
        // use as Status" error moves to link-time classification.
        let mut fields = Vec::new();
        for f in &project.fields.nodes {
            if f.typename != "ProjectV2SingleSelectField" {
                continue;
            }
            let field_id =
                f.id.clone()
                    .ok_or_else(|| PortError::Backend("single-select field missing id".into()))?;
            let options = f
                .options
                .as_deref()
                .unwrap_or_default()
                .iter()
                .enumerate()
                .map(|(i, o)| RemoteProjectFieldOption {
                    option_id: o.id.clone(),
                    name: o.name.clone(),
                    ordinal: u32::try_from(i).unwrap_or(u32::MAX),
                })
                .collect();
            fields.push(RemoteProjectField {
                field_id,
                name: f.name.clone().unwrap_or_default(),
                options,
            });
        }

        Ok(RemoteProjectSnapshot {
            node_id: project.id,
            number: project.number,
            title: project.title,
            owner_login: project.owner.login,
            fields,
        })
    }

    /// Fetch the owner's org-level native issue types (RFC 0006 D5/D8). A null
    /// owner (missing login) OR a non-org owner (a `User`, which has no
    /// `issueTypes` field) both collapse to an empty vec here — the
    /// `repositoryOwner` + `... on Organization` fragment shape means neither
    /// is a GraphQL error, satisfying the D8 no-error requirement.
    pub(crate) async fn fetch_org_issue_types(
        &self,
        owner_login: &str,
    ) -> PortResult<Vec<RemoteIssueType>> {
        let data: OrgIssueTypesData = self
            .run(FETCH_ORG_ISSUE_TYPES, json!({ "owner": owner_login }))
            .await?;
        Ok(data
            .repository_owner
            .and_then(|o| o.issue_types)
            .map(|c| c.nodes)
            .unwrap_or_default()
            .into_iter()
            .map(|n| RemoteIssueType {
                issue_type_id: n.id,
                name: n.name,
            })
            .collect())
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

    /// Move an issue into `repo_node_id` (#71). GitHub renumbers the issue in
    /// its new home and reissues the node id, so both come back for the caller
    /// to persist. A cross-org destination, a missing repo, or insufficient
    /// permission all arrive as a GraphQL error from [`Self::run`].
    pub(crate) async fn transfer_issue(
        &self,
        issue_node_id: &str,
        repo_node_id: &str,
    ) -> PortResult<(String, u64)> {
        let data: TransferIssueData = self
            .run(
                TRANSFER_ISSUE,
                json!({ "input": { "issueId": issue_node_id, "repositoryId": repo_node_id } }),
            )
            .await?;
        let issue = data.transfer_issue.issue.ok_or_else(|| {
            PortError::Backend(format!(
                "transfer of issue {issue_node_id} returned no destination issue"
            ))
        })?;
        Ok((issue.id, issue.number))
    }

    /// Set an item's single-select field to `option_id` (RFC 0006 D4). Field
    /// agnostic on the wire — `field_id` selects which single-select (Status,
    /// Priority, or any future one) the write targets; callers pin the
    /// meaning by which field id they pass. Used for both the board Status
    /// projection and the Priority projection.
    pub(crate) async fn set_single_select_option(
        &self,
        project_node_id: &str,
        item_node_id: &str,
        field_id: &str,
        option_id: Option<&str>,
    ) -> PortResult<Option<String>> {
        // Clear path (#238): a `None` option maps to the dedicated
        // `clearProjectV2ItemFieldValue` mutation. No value to read back.
        let Some(option_id) = option_id else {
            let _: ClearSingleSelectOptionData = self
                .run(
                    CLEAR_SINGLE_SELECT_OPTION,
                    json!({ "input": {
                        "projectId": project_node_id,
                        "itemId": item_node_id,
                        "fieldId": field_id,
                    } }),
                )
                .await?;
            return Ok(None);
        };
        let data: SetSingleSelectOptionData = self
            .run(
                SET_SINGLE_SELECT_OPTION,
                json!({ "input": {
                    "projectId": project_node_id,
                    "itemId": item_node_id,
                    "fieldId": field_id,
                    "value": { "singleSelectOptionId": option_id },
                } }),
            )
            .await?;
        // Read back the applied option from the targeted field (matched by
        // id, mirroring `map_poll_item`). The caller (drainer) compares it
        // against the sent `option_id` to detect a conflict. A mutation that
        // succeeds but returns no single-select value for the field is
        // ambiguous — surface it as a backend error so the drainer retries
        // rather than dead-lettering on a false conflict.
        data.update_project_v2_item_field_value
            .project_v2_item
            .field_values
            .nodes
            .into_iter()
            .find(|v| v.field.as_ref().and_then(|f| f.id.as_deref()) == Some(field_id))
            .and_then(|v| v.option_id)
            .map(Some)
            .ok_or_else(|| {
                PortError::Backend(format!(
                    "set_single_select_option on item {item_node_id} returned no single-select value for field {field_id}"
                ))
            })
    }

    /// Set (or clear) an issue's native "Type" field via `updateIssue` (RFC
    /// 0006 §0 A1 / #228). `issue_type_id` is the org registry's
    /// `issue_type_id` (`IT_…`) to apply, or `None` to clear — the `id` key
    /// is always sent, so `None` serializes to an explicit JSON `null`
    /// rather than being omitted (an omitted key would leave the field
    /// unchanged instead of clearing it).
    ///
    /// Unlike [`Self::set_single_select_option`] this does NOT read back and
    /// compare the applied value — the drainer's `SetIssueType` arm treats any
    /// `Ok` as success (no read-back `Conflict`, mirroring
    /// `SetProjectPriority`). The response is still deserialized into a typed
    /// struct so a wrong shape surfaces as a deserialize error rather than a
    /// silent no-op.
    pub(crate) async fn set_issue_type(
        &self,
        issue_node_id: &str,
        issue_type_id: Option<&str>,
    ) -> PortResult<()> {
        let _: SetIssueTypeData = self
            .run(
                SET_ISSUE_TYPE,
                json!({ "input": { "id": issue_node_id, "issueTypeId": issue_type_id } }),
            )
            .await?;
        Ok(())
    }

    pub(crate) async fn poll_project_items(
        &self,
        project_node_id: &str,
        status_field_id: &str,
        query: &str,
    ) -> PortResult<PollPage> {
        // `ProjectV2.items(query:)` uses Projects-v2 filter syntax (#208), NOT
        // issue-search: it has no `updated:` qualifier, so there is no
        // server-side time delta. We pass only a caller-supplied filter (e.g.
        // `is:open`, which Projects-v2 filter syntax does honour); an empty
        // filter becomes `null` (no filter → the full board). The per-project
        // watermark and the delta itself are client-side, in the poller.
        let query_arg = if query.trim().is_empty() {
            serde_json::Value::Null
        } else {
            json!(query.trim())
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
                        "query": query_arg,
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
            // Exhausted the page cap with more pages still available. A tick
            // enumerates the full board (no server-side delta, #208), so this
            // means the board exceeds MAX_POLL_PAGES * POLL_PAGE_SIZE items; the
            // caller treats the result as partial (via `PollPage.truncated`) and
            // refetches next cycle.
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

impl GraphqlClient {
    /// Read the current status option of each item in `item_node_ids`.
    ///
    /// `nodes(ids:)` takes at most [`POLL_PAGE_SIZE`] ids per call, so the list
    /// is chunked rather than cursor-paged — the ids are already in hand, which
    /// makes this a `chunks()` loop instead of a `pageInfo` walk. The same
    /// [`MAX_POLL_PAGES`] ceiling bounds it, and ids past that ceiling are left
    /// out of the map so the caller falls back to its cached value for them.
    ///
    /// Ids that GitHub resolves to something other than a project item (a
    /// deleted item comes back as a JSON `null`, and a node of some other type
    /// carries no `id`) are likewise absent rather than an error: an item
    /// disappearing from a board is a normal race against a local cache, not a
    /// failure of the read.
    ///
    /// A chunk that fails outright ends the walk but keeps what earlier chunks
    /// returned, reported as `truncated`. Only a failure with nothing yet in
    /// hand surfaces as `Err`, because there the caller has no partial result
    /// to prefer over its cache. GitHub answers an unresolvable id with a
    /// `null` node *and* a top-level `errors` array, which octocrab surfaces as
    /// an error for the whole request — so one deleted card costs its chunk,
    /// not the whole read.
    pub(crate) async fn fetch_item_statuses(
        &self,
        item_node_ids: &[String],
        status_field_id: &str,
    ) -> PortResult<ItemStatusPage> {
        let cap = (MAX_POLL_PAGES * POLL_PAGE_SIZE) as usize;
        let mut truncated = item_node_ids.len() > cap;
        let mut statuses = HashMap::new();

        for chunk in item_node_ids
            .get(..cap.min(item_node_ids.len()))
            .unwrap_or_default()
            .chunks(POLL_PAGE_SIZE as usize)
        {
            let data: ItemStatusesData =
                match self.run(ITEM_STATUSES, json!({ "ids": chunk })).await {
                    Ok(data) => data,
                    Err(_) if !statuses.is_empty() => {
                        truncated = true;
                        break;
                    }
                    Err(e) => return Err(e),
                };
            for node in data.nodes.into_iter().flatten() {
                let Some(id) = node.id else {
                    continue;
                };
                let option_id = node
                    .field_values
                    .nodes
                    .into_iter()
                    .find(|v| {
                        v.field.as_ref().and_then(|f| f.id.as_deref()) == Some(status_field_id)
                    })
                    .and_then(|v| v.option_id);
                statuses.insert(id, option_id);
            }
        }

        Ok(ItemStatusPage {
            statuses,
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
    options: Option<Vec<SingleSelectOptionNode>>,
}
/// One `options` node under a `ProjectV2SingleSelectField`. Named to avoid
/// confusion with the domain/ports `FieldOption` (neither is imported here).
#[derive(Deserialize)]
struct SingleSelectOptionNode {
    id: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrgIssueTypesData {
    repository_owner: Option<OrgIssueTypesOwner>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrgIssueTypesOwner {
    // Absent for a non-org owner (a `User` has no `issueTypes` field) — the
    // `#[serde(default)]` keeps that a `None`, i.e. an empty catalog, rather
    // than a deserialize error (D8 no-error).
    #[serde(default)]
    issue_types: Option<IssueTypesConn>,
}
#[derive(Deserialize)]
struct IssueTypesConn {
    nodes: Vec<IssueTypeNode>,
}
#[derive(Deserialize)]
struct IssueTypeNode {
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
struct TransferIssueData {
    transfer_issue: TransferIssueWrap,
}
#[derive(Deserialize)]
struct TransferIssueWrap {
    issue: Option<TransferredIssue>,
}
#[derive(Deserialize)]
struct TransferredIssue {
    id: String,
    number: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetSingleSelectOptionData {
    update_project_v2_item_field_value: ProjectV2ItemWrap,
}
// Clear response (#238): typed (not `serde_json::Value`) so a wrong shape is a
// deserialize failure, mirroring `SetIssueTypeData`. The value is unused — a
// successful clear returns `Ok(None)`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClearSingleSelectOptionData {
    #[allow(dead_code)]
    clear_project_v2_item_field_value: ClearProjectV2ItemWrap,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClearProjectV2ItemWrap {
    #[allow(dead_code)]
    project_v2_item: OptionalIdNode,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectV2ItemWrap {
    project_v2_item: SetSingleSelectOptionItem,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetSingleSelectOptionItem {
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    field_values: FieldValuesConn,
}

// Typed (rather than `serde_json::Value`) so a wrong response sub-shape is a
// deserialize failure rather than a silent pass, mirroring `UpdateDraftData`
// — the value itself is unused (RFC 0006 §0 A1: no read-back comparison).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetIssueTypeData {
    #[allow(dead_code)]
    update_issue: UpdateIssueWrap,
}
#[derive(Deserialize)]
struct UpdateIssueWrap {
    #[allow(dead_code)]
    issue: OptionalIdNode,
}

#[derive(Deserialize)]
struct ItemStatusesData {
    /// `nodes(ids:)` returns a positional `null` for any id it cannot resolve,
    /// so the element type is optional even though the ids are non-null.
    nodes: Vec<Option<ItemStatusNode>>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemStatusNode {
    /// Absent when the node resolved to something that is not a project item,
    /// which the `... on ProjectV2Item` fragment simply does not populate.
    #[serde(default)]
    id: Option<String>,
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
