//! SQLite access.
//!
//! SQLite permits a single writer. The writer pool therefore has exactly one connection and every
//! write path goes through it; reads use a separate pool. No transaction is ever held across a
//! provider call: services persist intent, act, then record the outcome.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

#[derive(Debug, Clone)]
pub struct Db {
    pub reader: SqlitePool,
    pub writer: SqlitePool,
}

impl Db {
    pub async fn connect(database_url: &str) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(database_url)
            .with_context(|| format!("invalid DATABASE_URL {database_url:?}"))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(10))
            .optimize_on_close(true, None);

        let writer = SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(15))
            .connect_with(options.clone())
            .await
            .context("connecting the SQLite writer pool")?;

        let reader = SqlitePoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(15))
            .connect_with(options)
            .await
            .context("connecting the SQLite reader pool")?;

        Ok(Self { reader, writer })
    }

    /// Run the embedded, versioned migrations.
    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("./migrations")
            .run(&self.writer)
            .await
            .context("running database migrations")?;
        Ok(())
    }

    /// Cheap readiness probe.
    pub async fn ping(&self) -> bool {
        sqlx::query_scalar::<_, i64>("SELECT 1")
            .fetch_one(&self.reader)
            .await
            .map(|value| value == 1)
            .unwrap_or(false)
    }

    pub async fn close(&self) {
        self.reader.close().await;
        self.writer.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_create_the_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite://{}/test.db?mode=rwc", dir.path().display());
        let db = Db::connect(&url).await.expect("connect");
        db.migrate().await.expect("migrate");
        assert!(db.ping().await);

        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .fetch_all(&db.reader)
                .await
                .expect("tables");
        for expected in [
            "tenants",
            "users",
            "tenant_users",
            "api_tokens",
            "sources",
            "source_providers",
            "actor_registry",
            "communities",
            "search_profiles",
            "keywords",
            "discovered_posts",
            "post_sources",
            "ingestion_runs",
            "ingestion_items",
            "ai_analyses",
            "decision_runs",
            "decision_answers",
            "benchmark_runs",
            "benchmark_results",
            "leads",
            "lead_scores",
            "lead_score_versions",
            "generated_comments",
            "comment_versions",
            "review_events",
            "engagements",
            "jobs",
            "job_attempts",
            "provider_usage",
            "provider_costs",
            "audit_logs",
        ] {
            assert!(
                tables.iter().any(|table| table == expected),
                "missing table {expected}"
            );
        }

        // Foreign keys are enforced.
        let violation = sqlx::query(
            "INSERT INTO tenant_users (tenant_id, user_id, role, created_at) \
             VALUES ('missing', 'missing', 'owner', '2024-01-01T00:00:00Z')",
        )
        .execute(&db.writer)
        .await;
        assert!(violation.is_err(), "foreign keys must be enabled");

        // Migrations are idempotent.
        db.migrate().await.expect("second migrate");
        db.close().await;
    }

    #[tokio::test]
    async fn writer_pool_has_a_single_connection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite://{}/single.db?mode=rwc", dir.path().display());
        let db = Db::connect(&url).await.expect("connect");
        assert_eq!(db.writer.options().get_max_connections(), 1);
        db.close().await;
    }
}
