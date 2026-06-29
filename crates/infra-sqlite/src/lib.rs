//! infra-sqlite — SQLite-backed implementations of the port traits.

mod event_sink;
mod mapping;
mod migrate;
mod outbox_repo;
mod pool;
mod project_repo;
mod repo_binding_repo;
mod task_repo;
mod task_snapshot_repo;
mod workspace_repo;

pub use event_sink::SqliteEventSink;
pub use migrate::{
    backfill_empty_repo_names, backfill_empty_repo_prefixes, backfill_empty_task_hashes, migrate,
};
pub use outbox_repo::SqliteOutboxRepository;
pub use pool::{Db, PoolError, open_db, open_from_path, open_read_pool, open_write_pool};
pub use project_repo::SqliteProjectRepository;
pub use repo_binding_repo::SqliteRepoBindingRepository;
pub use task_repo::SqliteTaskRepository;
pub use task_snapshot_repo::SqliteTaskSnapshotRepository;
pub use workspace_repo::SqliteWorkspaceRepository;

#[cfg(test)]
mod schema_const_consistency {
    //! Enforces the maintenance contract behind the explicit `*_COLS`
    //! projection consts (#110): each const must stay byte-equal to its
    //! table's live column set. A future `ALTER TABLE … ADD COLUMN` that
    //! forgets to update the matching const fails here, in CI, instead of
    //! silently dropping the new column from every read at runtime. The
    //! `statement_cache_capacity(0)` defense stops the *panic*; this test
    //! stops the *drift*.
    use std::collections::BTreeSet;

    use sqlx::{Row, SqlitePool};
    use tempfile::TempDir;

    fn parse_cols(cols: &str) -> BTreeSet<String> {
        cols.split(',').map(|c| c.trim().to_string()).collect()
    }

    async fn live_columns(pool: &SqlitePool, table: &str) -> BTreeSet<String> {
        // `table` is a hard-coded literal below, never user input — PRAGMA
        // can't bind a table name, so interpolation is the only option.
        sqlx::query(&format!("PRAGMA table_info({table})"))
            .fetch_all(pool)
            .await
            .expect("pragma table_info")
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect()
    }

    #[tokio::test]
    async fn cols_consts_match_live_schema() {
        let dir = TempDir::new().unwrap();
        let db = crate::open_from_path(&dir.path().join("schema-check.db"))
            .await
            .expect("open db");

        let cases: &[(&str, &str)] = &[
            (
                "repo_instances",
                crate::repo_binding_repo::REPO_INSTANCE_COLS,
            ),
            ("repo_origins", crate::repo_binding_repo::REPO_ORIGIN_COLS),
            (
                "worktree_links",
                crate::repo_binding_repo::WORKTREE_LINK_COLS,
            ),
            ("tasks", crate::task_repo::TASK_COLS),
            (
                "task_snapshots",
                crate::task_snapshot_repo::TASK_SNAPSHOT_COLS,
            ),
            ("projects", crate::project_repo::PROJECT_COLS),
            ("workspaces", crate::workspace_repo::WORKSPACE_COLS),
        ];

        for (table, cols) in cases {
            let want = live_columns(&db.reads, table).await;
            let got = parse_cols(cols);
            assert_eq!(
                got,
                want,
                "{table}: *_COLS const drifted from live schema (missing from const: {:?}; extra in const: {:?})",
                want.difference(&got).collect::<Vec<_>>(),
                got.difference(&want).collect::<Vec<_>>(),
            );
        }

        // The JOIN variant must name the same columns as PROJECT_COLS, just
        // table-qualified — so it can't drift away from the canonical set.
        let qualified: BTreeSet<String> = parse_cols(crate::project_repo::PROJECT_COLS_QUALIFIED)
            .iter()
            .map(|c| c.strip_prefix("projects.").unwrap_or(c).to_string())
            .collect();
        assert_eq!(
            qualified,
            parse_cols(crate::project_repo::PROJECT_COLS),
            "PROJECT_COLS_QUALIFIED must mirror PROJECT_COLS (column set, projects.-qualified)"
        );
    }
}
