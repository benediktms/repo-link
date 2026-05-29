//! Integration-style wiremock tests for the GraphQL (Projects v2) surface.
//! Each test mocks the single `POST /graphql` endpoint, asserts the outgoing
//! query string (so a typo'd operation/field can't ship green) and the
//! variables where they matter, and feeds back a canned `data` payload.
//! Fixture values mirror this account's project #3 from RFC 0001 Appendix A.

use crate::GithubAdapter;
use chrono::Utc;
use domain_core::Timestamp;
use ports::{PortError, RemoteProjectProvider};
use wiremock::matchers::{body_partial_json, body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn provider(server: &MockServer) -> GithubAdapter {
    GithubAdapter::with_base_url("t0k", server.uri()).unwrap()
}

/// Mount a `POST /graphql` responder that also asserts the outgoing query
/// string contains `query_contains` (e.g. the operation name), returning
/// `{"data": data}`.
async fn mount_graphql(server: &MockServer, query_contains: &str, data: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains(query_contains))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": data })))
        .mount(server)
        .await;
}

/// The project #3 schema payload (RFC Appendix A live sample).
fn project3_payload() -> serde_json::Value {
    serde_json::json!({
        "repositoryOwner": {
            "projectV2": {
                "id": "PVT_kwHOAukuJ84BYZR7",
                "number": 3,
                "title": "repo-link",
                "owner": { "login": "benediktms" },
                "fields": { "nodes": [
                    { "__typename": "ProjectV2Field", "id": "PVTF_text", "name": "Title" },
                    { "__typename": "ProjectV2SingleSelectField",
                      "id": "PVTSSF_lAHOAukuJ84BYZR7zhTfceU",
                      "name": "Status",
                      "options": [
                        { "id": "f75ad846", "name": "Backlog" },
                        { "id": "e18bf179", "name": "Ready" },
                        { "id": "47fc9ee4", "name": "In progress" },
                        { "id": "aba860b9", "name": "In review" },
                        { "id": "98236657", "name": "Done" }
                      ] }
                ] }
            }
        }
    })
}

#[tokio::test]
async fn fetch_project_maps_schema_and_ordinals() {
    let server = MockServer::start().await;
    mount_graphql(&server, "ProjectV2SingleSelectField", project3_payload()).await;

    let snap = provider(&server)
        .fetch_project("benediktms", 3)
        .await
        .unwrap();

    assert_eq!(snap.node_id, "PVT_kwHOAukuJ84BYZR7");
    assert_eq!(snap.number, 3);
    assert_eq!(snap.title, "repo-link");
    assert_eq!(snap.owner_login, "benediktms");
    assert_eq!(snap.status_field_id, "PVTSSF_lAHOAukuJ84BYZR7zhTfceU");
    assert_eq!(snap.status_options.len(), 5);
    // Ordinal is the array index — preserves the board's column order.
    assert_eq!(snap.status_options[0].name, "Backlog");
    assert_eq!(snap.status_options[0].ordinal, 0);
    assert_eq!(snap.status_options[2].option_id, "47fc9ee4");
    assert_eq!(snap.status_options[2].ordinal, 2);
    assert_eq!(snap.status_options[4].name, "Done");
}

#[tokio::test]
async fn fetch_project_prefers_field_named_status() {
    let server = MockServer::start().await;
    // Two single-select fields; "Status" is NOT first. The adapter must
    // still pick it over the earlier "Priority".
    mount_graphql(
        &server,
        "repositoryOwner",
        serde_json::json!({ "repositoryOwner": { "projectV2": {
            "id": "PVT_x", "number": 7, "title": "t", "owner": { "login": "acme" },
            "fields": { "nodes": [
                { "__typename": "ProjectV2SingleSelectField", "id": "PVTSSF_prio",
                  "name": "Priority", "options": [ { "id": "p0", "name": "P0" } ] },
                { "__typename": "ProjectV2SingleSelectField", "id": "PVTSSF_status",
                  "name": "Status", "options": [ { "id": "s0", "name": "Todo" } ] }
            ] }
        } } }),
    )
    .await;

    let snap = provider(&server).fetch_project("acme", 7).await.unwrap();
    assert_eq!(snap.status_field_id, "PVTSSF_status");
    assert_eq!(snap.status_options[0].name, "Todo");
}

#[tokio::test]
async fn fetch_project_falls_back_to_first_single_select() {
    let server = MockServer::start().await;
    // No field literally named "Status" → first single-select wins.
    mount_graphql(
        &server,
        "repositoryOwner",
        serde_json::json!({ "repositoryOwner": { "projectV2": {
            "id": "PVT_x", "number": 7, "title": "t", "owner": { "login": "acme" },
            "fields": { "nodes": [
                { "__typename": "ProjectV2Field", "id": "PVTF_t", "name": "Title" },
                { "__typename": "ProjectV2SingleSelectField", "id": "PVTSSF_stage",
                  "name": "Stage", "options": [ { "id": "x", "name": "Doing" } ] }
            ] }
        } } }),
    )
    .await;

    let snap = provider(&server).fetch_project("acme", 7).await.unwrap();
    assert_eq!(snap.status_field_id, "PVTSSF_stage");
}

#[tokio::test]
async fn fetch_project_missing_owner_maps_to_not_found() {
    let server = MockServer::start().await;
    mount_graphql(
        &server,
        "repositoryOwner",
        serde_json::json!({ "repositoryOwner": null }),
    )
    .await;

    let err = provider(&server)
        .fetch_project("ghost", 99)
        .await
        .unwrap_err();
    assert!(matches!(err, PortError::NotFound(_)), "got {err:?}");
}

#[tokio::test]
async fn fetch_project_null_project_maps_to_not_found() {
    let server = MockServer::start().await;
    // Owner exists but the numbered project does not → projectV2: null.
    mount_graphql(
        &server,
        "repositoryOwner",
        serde_json::json!({ "repositoryOwner": { "projectV2": null } }),
    )
    .await;

    let err = provider(&server)
        .fetch_project("acme", 99)
        .await
        .unwrap_err();
    assert!(matches!(err, PortError::NotFound(_)), "got {err:?}");
}

#[tokio::test]
async fn fetch_project_without_single_select_maps_to_backend() {
    let server = MockServer::start().await;
    // A project whose fields contain no single-select at all → there is no
    // Status field to drive, which is a backend/data problem, not "not found".
    mount_graphql(
        &server,
        "repositoryOwner",
        serde_json::json!({ "repositoryOwner": { "projectV2": {
            "id": "PVT_x", "number": 7, "title": "t", "owner": { "login": "acme" },
            "fields": { "nodes": [
                { "__typename": "ProjectV2Field", "id": "PVTF_t", "name": "Title" }
            ] }
        } } }),
    )
    .await;

    let err = provider(&server)
        .fetch_project("acme", 7)
        .await
        .unwrap_err();
    assert!(matches!(err, PortError::Backend(_)), "got {err:?}");
}

#[tokio::test]
async fn add_item_sends_content_id_and_returns_item_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("addProjectV2ItemById"))
        .and(body_partial_json(serde_json::json!({
            "variables": { "input": { "projectId": "PVT_x", "contentId": "I_issue" } }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "addProjectV2ItemById": { "item": { "id": "PVTI_new" } } }
        })))
        .mount(&server)
        .await;

    let id = provider(&server)
        .add_item("PVT_x", "I_issue")
        .await
        .unwrap();
    assert_eq!(id, "PVTI_new");
}

#[tokio::test]
async fn create_draft_issue_sends_title_body_and_returns_item_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("addProjectV2DraftIssue"))
        .and(body_partial_json(serde_json::json!({
            "variables": { "input": { "projectId": "PVT_x", "title": "triage me", "body": "later" } }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "addProjectV2DraftIssue": { "projectItem": { "id": "PVTI_draft" } } }
        })))
        .mount(&server)
        .await;

    let id = provider(&server)
        .create_draft_issue("PVT_x", "triage me", "later")
        .await
        .unwrap();
    assert_eq!(id, "PVTI_draft");
}

#[tokio::test]
async fn update_draft_issue_resolves_draft_id_then_updates() {
    let server = MockServer::start().await;
    // First call: resolve item → draft content id. Matched by `variables.id`.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(
            serde_json::json!({ "variables": { "id": "PVTI_draft" } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "node": { "content": { "id": "DI_draft" } } }
        })))
        .mount(&server)
        .await;
    // Second call: the update, keyed on the resolved draftIssueId. Only the
    // supplied `title` rides along — `body` is absent (None).
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("updateProjectV2DraftIssue"))
        .and(body_partial_json(serde_json::json!({
            "variables": { "input": { "draftIssueId": "DI_draft", "title": "new title" } }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "updateProjectV2DraftIssue": { "draftIssue": { "id": "DI_draft" } } }
        })))
        .mount(&server)
        .await;

    provider(&server)
        .update_draft_issue("PVTI_draft", Some("new title"), None)
        .await
        .unwrap();
}

#[tokio::test]
async fn update_draft_issue_errors_when_item_is_not_a_draft() {
    let server = MockServer::start().await;
    // Resolve returns a node whose content has no DraftIssue id (it's a real
    // issue) → the adapter can't address an update.
    mount_graphql(
        &server,
        "DraftIssue",
        serde_json::json!({ "node": { "content": {} } }),
    )
    .await;

    let err = provider(&server)
        .update_draft_issue("PVTI_issue", Some("x"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, PortError::NotFound(_)), "got {err:?}");
}

#[tokio::test]
async fn convert_draft_to_issue_returns_issue_node_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains(
            "convertProjectV2DraftIssueItemToIssue",
        ))
        .and(body_partial_json(serde_json::json!({
            "variables": { "input": { "itemId": "PVTI_draft", "repositoryId": "R_repo" } }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "convertProjectV2DraftIssueItemToIssue": {
                "item": { "content": { "id": "I_converted", "number": 42 } }
            } }
        })))
        .mount(&server)
        .await;

    let (issue_id, number) = provider(&server)
        .convert_draft_to_issue("PVTI_draft", "R_repo")
        .await
        .unwrap();
    assert_eq!(issue_id, "I_converted");
    assert_eq!(
        number, 42,
        "the REST number is captured for remote_id (#54)"
    );
}

#[tokio::test]
async fn convert_draft_to_issue_errors_when_content_id_absent() {
    let server = MockServer::start().await;
    // The mutation committed but the inline projection of the new issue id is
    // null → surfaced as a backend error rather than silently dropped.
    mount_graphql(
        &server,
        "convertProjectV2DraftIssueItemToIssue",
        serde_json::json!({ "convertProjectV2DraftIssueItemToIssue": { "item": { "content": null } } }),
    )
    .await;

    let err = provider(&server)
        .convert_draft_to_issue("PVTI_draft", "R_repo")
        .await
        .unwrap_err();
    assert!(matches!(err, PortError::Backend(_)), "got {err:?}");
}

#[tokio::test]
async fn convert_draft_to_issue_errors_when_number_absent() {
    let server = MockServer::start().await;
    // The id projected but the REST `number` is null. Without the number the
    // task's `remote_id` would be empty (#54), so surface a backend error
    // rather than persist a half-populated issue-backed RemoteRef.
    mount_graphql(
        &server,
        "convertProjectV2DraftIssueItemToIssue",
        serde_json::json!({ "convertProjectV2DraftIssueItemToIssue": {
            "item": { "content": { "id": "I_converted" } }
        } }),
    )
    .await;

    let err = provider(&server)
        .convert_draft_to_issue("PVTI_draft", "R_repo")
        .await
        .unwrap_err();
    assert!(matches!(err, PortError::Backend(_)), "got {err:?}");
}

#[tokio::test]
async fn set_status_sends_single_select_option_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("updateProjectV2ItemFieldValue"))
        .and(body_partial_json(serde_json::json!({
            "variables": { "input": {
                "projectId": "PVT_x", "itemId": "PVTI_y", "fieldId": "PVTSSF_z",
                "value": { "singleSelectOptionId": "47fc9ee4" }
            } }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "updateProjectV2ItemFieldValue": { "projectV2Item": { "id": "PVTI_y" } } }
        })))
        .mount(&server)
        .await;

    provider(&server)
        .set_status("PVT_x", "PVTI_y", "PVTSSF_z", "47fc9ee4")
        .await
        .unwrap();
}

/// One project item with a single-select value on the status field.
fn poll_node(
    id: &str,
    typename: &str,
    status_field_id: &str,
    option_id: &str,
) -> serde_json::Value {
    let content = if typename == "DraftIssue" {
        serde_json::json!({ "__typename": "DraftIssue", "title": "a draft", "body": "d" })
    } else {
        serde_json::json!({ "__typename": "Issue", "id": "I_x", "number": 1,
            "title": "t", "body": "b", "state": "OPEN",
            "repository": { "nameWithOwner": "o/r" } })
    };
    serde_json::json!({
        "id": id,
        "updatedAt": "2026-05-26T10:00:00Z",
        "fieldValues": { "nodes": [
            { "__typename": "ProjectV2ItemFieldSingleSelectValue",
              "optionId": option_id, "field": { "id": status_field_id } }
        ] },
        "content": content,
    })
}

#[tokio::test]
async fn poll_project_items_reads_option_from_matching_field_id() {
    let server = MockServer::start().await;
    // The issue carries TWO single-select values: one for an unrelated
    // "Priority" field and one for the Status field. The adapter must read the
    // option from the field whose id matches `status_field_id`, ignoring the
    // other — the bug this guards against is reading by the literal name
    // "Status" or by position.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("items(first:"))
        .and(body_string_contains("fieldValues"))
        // The `updated:>` delta lever must be present in the search string.
        .and(body_string_contains("updated:>"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "node": { "items": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": [
                    { "id": "PVTI_issue", "updatedAt": "2026-05-26T10:00:00Z",
                      "fieldValues": { "nodes": [
                        { "__typename": "ProjectV2ItemFieldSingleSelectValue",
                          "optionId": "PRIO_HIGH", "field": { "id": "PVTSSF_priority" } },
                        { "__typename": "ProjectV2ItemFieldSingleSelectValue",
                          "optionId": "47fc9ee4", "field": { "id": "PVTSSF_status" } }
                      ] },
                      "content": { "__typename": "Issue", "id": "I_1", "number": 12,
                        "title": "real issue", "body": "b", "state": "OPEN",
                        "repository": { "nameWithOwner": "o/r" } } },
                    { "id": "PVTI_draft", "updatedAt": "2026-05-26T11:00:00Z",
                      "fieldValues": { "nodes": [
                        { "__typename": "ProjectV2ItemFieldSingleSelectValue",
                          "optionId": "f75ad846", "field": { "id": "PVTSSF_status" } }
                      ] },
                      "content": { "__typename": "DraftIssue", "title": "a draft", "body": "d" } },
                    { "id": "PVTI_pr", "updatedAt": "2026-05-26T12:00:00Z",
                      "fieldValues": { "nodes": [] },
                      "content": { "__typename": "PullRequest" } }
                ]
            } } }
        })))
        .mount(&server)
        .await;

    let since = Timestamp::from_utc(Utc::now());
    let page = provider(&server)
        .poll_project_items("PVT_x", "PVTSSF_status", since, "is:open")
        .await
        .unwrap();
    let items = &page.items;

    // Single complete page → not truncated.
    assert!(!page.truncated);
    // The PullRequest node is skipped — only the issue + draft map through.
    assert_eq!(items.len(), 2);

    let issue = &items[0];
    assert_eq!(issue.item_node_id, "PVTI_issue");
    assert_eq!(issue.issue_node_id.as_deref(), Some("I_1"));
    assert_eq!(issue.number, Some(12));
    assert_eq!(issue.canonical_repo.as_deref(), Some("github.com/o/r"));
    assert_eq!(issue.title, "real issue");
    assert!(!issue.closed);
    // Read from PVTSSF_status, NOT the Priority value that appears first.
    assert_eq!(issue.status_option_id.as_deref(), Some("47fc9ee4"));

    let draft = &items[1];
    assert_eq!(draft.item_node_id, "PVTI_draft");
    assert_eq!(draft.issue_node_id, None);
    assert_eq!(draft.number, None);
    assert_eq!(draft.canonical_repo, None);
    assert_eq!(draft.title, "a draft");
    assert!(!draft.closed);
    assert_eq!(draft.status_option_id.as_deref(), Some("f75ad846"));
}

#[tokio::test]
async fn poll_project_items_yields_none_when_status_field_absent() {
    let server = MockServer::start().await;
    // The item has only a value for some OTHER field — no value on the chosen
    // status field. status_option_id must be None (not a wrong field's option).
    mount_graphql(
        &server,
        "items(first:",
        serde_json::json!({ "node": { "items": {
            "pageInfo": { "hasNextPage": false, "endCursor": null },
            "nodes": [
                { "id": "PVTI_a", "updatedAt": "2026-05-26T10:00:00Z",
                  "fieldValues": { "nodes": [
                    { "__typename": "ProjectV2ItemFieldSingleSelectValue",
                      "optionId": "PRIO_HIGH", "field": { "id": "PVTSSF_priority" } }
                  ] },
                  "content": { "__typename": "Issue", "id": "I_a", "number": 1,
                    "title": "t", "body": "", "state": "OPEN",
                    "repository": { "nameWithOwner": "o/r" } } }
            ]
        } } }),
    )
    .await;

    let page = provider(&server)
        .poll_project_items(
            "PVT_x",
            "PVTSSF_status",
            Timestamp::from_utc(Utc::now()),
            "",
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].status_option_id, None);
}

#[tokio::test]
async fn poll_project_items_marks_closed_issue() {
    let server = MockServer::start().await;
    mount_graphql(
        &server,
        "items(first:",
        serde_json::json!({ "node": { "items": {
            "pageInfo": { "hasNextPage": false, "endCursor": null },
            "nodes": [
                { "id": "PVTI_c", "updatedAt": "2026-05-26T10:00:00Z",
                  "fieldValues": { "nodes": [
                    { "__typename": "ProjectV2ItemFieldSingleSelectValue",
                      "optionId": "98236657", "field": { "id": "PVTSSF_status" } }
                  ] },
                  "content": { "__typename": "Issue", "id": "I_2", "number": 9,
                    "title": "done", "body": "", "state": "CLOSED",
                    "repository": { "nameWithOwner": "o/r" } } }
            ]
        } } }),
    )
    .await;

    let page = provider(&server)
        .poll_project_items(
            "PVT_x",
            "PVTSSF_status",
            Timestamp::from_utc(Utc::now()),
            "",
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(page.items[0].closed);
}

#[tokio::test]
async fn poll_project_items_follows_pagination() {
    let server = MockServer::start().await;
    // Page 1 (after = null): one item, hasNextPage → cursor "c1".
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(
            serde_json::json!({ "variables": { "after": null } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "node": { "items": {
                "pageInfo": { "hasNextPage": true, "endCursor": "c1" },
                "nodes": [ poll_node("PVTI_a", "Issue", "PVTSSF_status", "f75ad846") ]
            } } }
        })))
        .mount(&server)
        .await;
    // Page 2 (after = "c1"): one item, no more pages.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_partial_json(
            serde_json::json!({ "variables": { "after": "c1" } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "node": { "items": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": [ poll_node("PVTI_b", "Issue", "PVTSSF_status", "98236657") ]
            } } }
        })))
        .mount(&server)
        .await;

    let page = provider(&server)
        .poll_project_items(
            "PVT_x",
            "PVTSSF_status",
            Timestamp::from_utc(Utc::now()),
            "is:open",
        )
        .await
        .unwrap();
    // Last page reported `hasNextPage: false` → the read is complete.
    assert!(!page.truncated);
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0].item_node_id, "PVTI_a");
    assert_eq!(page.items[1].item_node_id, "PVTI_b");
}

#[tokio::test]
async fn poll_project_items_stops_at_page_cap() {
    let server = MockServer::start().await;
    // A responder that ALWAYS claims another page (fresh cursor). The loop must
    // stop at the MAX_POLL_PAGES cap (20) rather than spin forever — one item
    // per page → exactly 20 items.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "node": { "items": {
                "pageInfo": { "hasNextPage": true, "endCursor": "always-more" },
                "nodes": [ poll_node("PVTI_loop", "Issue", "PVTSSF_status", "f75ad846") ]
            } } }
        })))
        .mount(&server)
        .await;

    let page = provider(&server)
        .poll_project_items(
            "PVT_x",
            "PVTSSF_status",
            Timestamp::from_utc(Utc::now()),
            "",
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 20, "must stop at MAX_POLL_PAGES");
    assert!(
        page.truncated,
        "hitting the page cap with more pages available must report truncated"
    );
}

#[tokio::test]
async fn poll_project_items_stops_on_missing_cursor() {
    let server = MockServer::start().await;
    // Broken pagination metadata: hasNextPage=true but endCursor=null. The
    // loop must stop after this page (not spin, not error) and return what it
    // has — the truncation warning covers the incompleteness.
    mount_graphql(
        &server,
        "items(first:",
        serde_json::json!({ "node": { "items": {
            "pageInfo": { "hasNextPage": true, "endCursor": null },
            "nodes": [ poll_node("PVTI_x", "Issue", "PVTSSF_status", "f75ad846") ]
        } } }),
    )
    .await;

    let page = provider(&server)
        .poll_project_items(
            "PVT_x",
            "PVTSSF_status",
            Timestamp::from_utc(Utc::now()),
            "",
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(
        page.truncated,
        "hasNextPage with a null cursor is an incomplete read → truncated"
    );
}

#[tokio::test]
async fn graphql_errors_map_to_backend() {
    let server = MockServer::start().await;
    // A GraphQL `errors` array (no usable `data`) → backend failure.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "errors": [ { "message": "Could not resolve to a node with the global id." } ]
        })))
        .mount(&server)
        .await;

    let err = provider(&server)
        .fetch_project("acme", 7)
        .await
        .unwrap_err();
    assert!(matches!(err, PortError::Backend(_)), "got {err:?}");
}
