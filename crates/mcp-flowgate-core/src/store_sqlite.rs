//! SQLite-backed `WorkflowStore`.
//!
//! Uses `rusqlite` with the `bundled` feature so no system libsqlite is
//! needed. The schema is one table:
//!
//! ```sql
//! CREATE TABLE workflows (
//!     id           TEXT PRIMARY KEY,
//!     version      INTEGER NOT NULL,
//!     instance     TEXT    NOT NULL  -- JSON-serialized WorkflowInstance
//! );
//! ```
//!
//! All ops happen on a `tokio::task::spawn_blocking` boundary to keep
//! synchronous SQLite calls off the async runtime. Optimistic locking is
//! enforced with `UPDATE ... WHERE id = ? AND version = ?` inside a
//! transaction; rows-affected = 0 means stale.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use rusqlite::{params, Connection};

use crate::model::WorkflowInstance;
use crate::ports::WorkflowStore;

#[derive(Clone)]
pub struct SqliteWorkflowStore {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl SqliteWorkflowStore {
    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating dir {}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("opening sqlite at {}", path.display()))?;
        // WAL gives much better concurrent-read performance for our pattern
        // (many reads, occasional writes).
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS workflows (
                id       TEXT PRIMARY KEY,
                version  INTEGER NOT NULL,
                instance TEXT NOT NULL
            )",
            [],
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path,
        })
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS workflows (
                id       TEXT PRIMARY KEY,
                version  INTEGER NOT NULL,
                instance TEXT NOT NULL
            )",
            [],
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: PathBuf::from(":memory:"),
        })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[async_trait]
impl WorkflowStore for SqliteWorkflowStore {
    async fn create(&self, instance: WorkflowInstance) -> anyhow::Result<WorkflowInstance> {
        let conn = self.conn.clone();
        let json = serde_json::to_string(&instance)?;
        let inst = instance.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<WorkflowInstance> {
            let conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            let rows = conn.execute(
                "INSERT INTO workflows (id, version, instance) VALUES (?1, ?2, ?3)",
                params![inst.id, inst.version as i64, json],
            );
            match rows {
                Ok(_) => Ok(inst),
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    bail!("workflow id collision: {}", inst.id)
                }
                Err(e) => Err(e.into()),
            }
        })
        .await?
    }

    async fn load(&self, workflow_id: &str) -> anyhow::Result<WorkflowInstance> {
        let conn = self.conn.clone();
        let id = workflow_id.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<WorkflowInstance> {
            let conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            let mut stmt = conn.prepare("SELECT instance FROM workflows WHERE id = ?1")?;
            let json: String =
                stmt.query_row(params![id], |row| row.get(0))
                    .map_err(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => {
                            anyhow!("workflow {} not found", id)
                        }
                        other => other.into(),
                    })?;
            let instance: WorkflowInstance = serde_json::from_str(&json)?;
            Ok(instance)
        })
        .await?
    }

    async fn save_if_version(
        &self,
        instance: WorkflowInstance,
        expected_version: u64,
    ) -> anyhow::Result<WorkflowInstance> {
        let conn = self.conn.clone();
        let json = serde_json::to_string(&instance)?;
        let inst = instance.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<WorkflowInstance> {
            let mut conn = conn.lock().expect("LOCK_POISONED: sqlite connection");
            let tx = conn.transaction()?;
            // Confirm the row exists.
            let exists: bool = tx
                .query_row(
                    "SELECT 1 FROM workflows WHERE id = ?1",
                    params![inst.id],
                    |_| Ok(()),
                )
                .map(|_: ()| true)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(false),
                    other => Err(other),
                })?;
            if !exists {
                bail!("workflow {} not found", inst.id);
            }
            let updated = tx.execute(
                "UPDATE workflows SET version = ?1, instance = ?2
                 WHERE id = ?3 AND version = ?4",
                params![inst.version as i64, json, inst.id, expected_version as i64],
            )?;
            if updated == 0 {
                bail!(
                    "stale workflow version (expected {} for {})",
                    expected_version,
                    inst.id
                );
            }
            tx.commit()?;
            Ok(inst)
        })
        .await?
    }
}
