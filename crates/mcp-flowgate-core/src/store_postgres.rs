//! Postgres-backed `WorkflowStore`.
//!
//! Uses `sqlx` with the `postgres` and `runtime-tokio` features. The schema
//! is one table:
//!
//! ```sql
//! CREATE TABLE workflows (
//!     id           TEXT PRIMARY KEY,
//!     version      BIGINT NOT NULL,
//!     instance     JSONB  NOT NULL
//! );
//! ```
//!
//! Optimistic locking is enforced with `UPDATE ... WHERE id = $1 AND
//! version = $2` inside a transaction; rows-affected = 0 means stale.
//! `create` uses `INSERT ... ON CONFLICT (id) DO NOTHING` with a row-count
//! check for collision detection.

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::model::WorkflowInstance;
use crate::ports::WorkflowStore;

pub struct PostgresWorkflowStore {
    pool: PgPool,
}

impl PostgresWorkflowStore {
    /// Connect to Postgres and run migrations.
    pub async fn connect(database_url: &str) -> anyhow::Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(database_url)
            .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS workflows (
                id       TEXT PRIMARY KEY,
                version  BIGINT NOT NULL,
                instance JSONB  NOT NULL
            )",
        )
        .execute(&pool)
        .await?;

        Ok(Self { pool })
    }
}

#[async_trait]
impl WorkflowStore for PostgresWorkflowStore {
    async fn create(&self, instance: WorkflowInstance) -> anyhow::Result<WorkflowInstance> {
        let json = serde_json::to_value(&instance)?;
        let id = instance.id.clone();
        let version = instance.version as i64;

        let result = sqlx::query(
            "INSERT INTO workflows (id, version, instance) VALUES ($1, $2, $3) ON CONFLICT (id) DO NOTHING",
        )
        .bind(&id)
        .bind(version)
        .bind(&json)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() == 0 {
            bail!("workflow id collision: {}", id);
        }

        Ok(instance)
    }

    async fn load(&self, workflow_id: &str) -> anyhow::Result<WorkflowInstance> {
        let row: Option<(serde_json::Value,)> =
            sqlx::query_as("SELECT instance FROM workflows WHERE id = $1")
                .bind(workflow_id)
                .fetch_optional(&self.pool)
                .await?;

        match row {
            Some((json,)) => {
                let instance: WorkflowInstance = serde_json::from_value(json)?;
                Ok(instance)
            }
            None => Err(anyhow!("workflow {} not found", workflow_id)),
        }
    }

    async fn save_if_version(
        &self,
        instance: WorkflowInstance,
        expected_version: u64,
    ) -> anyhow::Result<WorkflowInstance> {
        let json = serde_json::to_value(&instance)?;
        let id = instance.id.clone();
        let new_version = instance.version as i64;
        let expected = expected_version as i64;

        let mut tx = self.pool.begin().await?;

        let result = sqlx::query(
            "UPDATE workflows SET version = $1, instance = $2 WHERE id = $3 AND version = $4",
        )
        .bind(new_version)
        .bind(&json)
        .bind(&id)
        .bind(expected)
        .execute(&mut *tx)
        .await?;

        if result.rows_affected() == 0 {
            tx.rollback().await?;
            bail!(
                "stale workflow version (expected {} for {})",
                expected_version,
                id
            );
        }

        tx.commit().await?;
        Ok(instance)
    }
}
