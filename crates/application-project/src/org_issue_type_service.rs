//! Org-level native issue-type registry orchestration (RFC 0006 D5/D8).
//!
//! [`OrgIssueTypeService::refresh`] is the org (re)fetch point invoked at `rl
//! project link`: it persists the provider-fetched types as the owner's
//! registry and reports availability (D8). An empty catalog is a no-op write
//! (see `refresh`) so a blank fetch never clobbers a previously good cache.

use std::sync::Arc;

use domain_project::{OrgIssueType, OrgIssueTypeRegistry};
use ports::{OrgIssueTypeRepository, RemoteIssueType};

use crate::error::Result;

pub struct OrgIssueTypeService {
    repo: Arc<dyn OrgIssueTypeRepository>,
}

impl OrgIssueTypeService {
    pub fn new(repo: Arc<dyn OrgIssueTypeRepository>) -> Self {
        Self { repo }
    }

    /// Persist the owner's freshly-fetched issue-type catalog and report
    /// whether native types are available for the org (D8).
    ///
    /// Non-destructive: an empty catalog — a user-owned owner, a
    /// feature-disabled org, or a transient empty response — returns
    /// `Ok(false)` **without touching the cache**, so a blank fetch can never
    /// wipe a previously good registry (`save` is replace-wholesale). A
    /// non-empty catalog is persisted, replacing the prior set. Availability is
    /// derivable from the input, so no read-back is needed.
    pub async fn refresh(
        &self,
        owner_login: &str,
        remote_types: Vec<RemoteIssueType>,
    ) -> Result<bool> {
        if remote_types.is_empty() {
            return Ok(false);
        }
        let types = remote_types
            .into_iter()
            .map(|t| OrgIssueType {
                issue_type_id: t.issue_type_id,
                name: t.name,
            })
            .collect();
        let registry = OrgIssueTypeRegistry::new(owner_login, types);
        self.repo.save(&registry).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use testing_fixtures::InMemoryOrgIssueTypeRepository;

    fn rt(id: &str, name: &str) -> RemoteIssueType {
        RemoteIssueType {
            issue_type_id: id.into(),
            name: name.into(),
        }
    }

    #[tokio::test]
    async fn refresh_persists_and_reports_available() {
        let repo = Arc::new(InMemoryOrgIssueTypeRepository::new());
        let svc = OrgIssueTypeService::new(repo.clone());

        let available = svc
            .refresh("acme", vec![rt("IT_bug", "Bug"), rt("IT_feat", "Feature")])
            .await
            .unwrap();
        assert!(available);

        let stored = repo.get("acme").await.unwrap();
        assert_eq!(stored.types.len(), 2);
        assert_eq!(stored.types[0].issue_type_id, "IT_bug");
    }

    #[tokio::test]
    async fn refresh_empty_reports_unavailable_no_error() {
        // D8: a user-owned owner / disabled feature yields an empty catalog —
        // Ok(false), not Err.
        let repo = Arc::new(InMemoryOrgIssueTypeRepository::new());
        let svc = OrgIssueTypeService::new(repo.clone());

        let available = svc.refresh("some-user", vec![]).await.unwrap();
        assert!(!available);
        assert!(repo.get("some-user").await.unwrap().types.is_empty());
    }

    #[tokio::test]
    async fn refresh_replaces_previous() {
        let repo = Arc::new(InMemoryOrgIssueTypeRepository::new());
        let svc = OrgIssueTypeService::new(repo.clone());

        svc.refresh("acme", vec![rt("IT_a", "Alpha"), rt("IT_b", "Beta")])
            .await
            .unwrap();
        svc.refresh("acme", vec![rt("IT_c", "Gamma")])
            .await
            .unwrap();

        let stored = repo.get("acme").await.unwrap();
        assert_eq!(stored.types.len(), 1);
        assert_eq!(stored.types[0].issue_type_id, "IT_c");
    }

    #[tokio::test]
    async fn refresh_empty_does_not_wipe_existing() {
        // A transient empty-but-successful fetch must NOT clobber a good cache.
        let repo = Arc::new(InMemoryOrgIssueTypeRepository::new());
        let svc = OrgIssueTypeService::new(repo.clone());

        svc.refresh("acme", vec![rt("IT_bug", "Bug"), rt("IT_feat", "Feature")])
            .await
            .unwrap();

        let available = svc.refresh("acme", vec![]).await.unwrap();
        assert!(!available); // reported unavailable...
        let stored = repo.get("acme").await.unwrap();
        assert_eq!(stored.types.len(), 2); // ...but the cached registry is intact
    }
}
