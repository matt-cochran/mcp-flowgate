//! Filesystem-backed `WorkflowStore`.
//!
//! Each workflow lives in its own JSON file under `<root>/<workflow_id>.json`.
//! Writes are made durable through atomic rename (`*.tmp` → `*.json`) and
//! serialized through a single async mutex so the version check + write is
//! one critical section. Loads read directly off disk — they don't need the
//! lock since file writes are atomic.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::model::WorkflowInstance;
use crate::ports::WorkflowStore;

#[derive(Clone)]
pub struct FileWorkflowStore {
    root: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl FileWorkflowStore {
    pub fn new(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating workflow store dir {}", root.display()))?;
        Ok(Self {
            root,
            write_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, id: &str) -> PathBuf {
        // Workflow ids are uuid-shaped (`wf_<32 hex>`), no path-separator
        // concerns. Defensive sanitize anyway.
        let sanitized: String = id
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.root.join(format!("{sanitized}.json"))
    }

    async fn read_file(&self, id: &str) -> anyhow::Result<Option<WorkflowInstance>> {
        let path = self.path_for(id);
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let inst = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?;
                Ok(Some(inst))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow!("reading {}: {e}", path.display())),
        }
    }

    async fn write_atomic(&self, instance: &WorkflowInstance) -> anyhow::Result<()> {
        let final_path = self.path_for(&instance.id);
        let tmp_path = final_path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(instance)?;
        tokio::fs::write(&tmp_path, bytes)
            .await
            .with_context(|| format!("writing {}", tmp_path.display()))?;
        tokio::fs::rename(&tmp_path, &final_path)
            .await
            .with_context(|| {
                format!("renaming {} → {}", tmp_path.display(), final_path.display())
            })?;
        Ok(())
    }
}

#[async_trait]
impl WorkflowStore for FileWorkflowStore {
    async fn create(&self, instance: WorkflowInstance) -> anyhow::Result<WorkflowInstance> {
        let _guard = self.write_lock.lock().await;
        if self.read_file(&instance.id).await?.is_some() {
            bail!("workflow id collision: {}", instance.id);
        }
        self.write_atomic(&instance).await?;
        Ok(instance)
    }

    async fn load(&self, workflow_id: &str) -> anyhow::Result<WorkflowInstance> {
        self.read_file(workflow_id)
            .await?
            .ok_or_else(|| anyhow!("workflow {} not found", workflow_id))
    }

    async fn save_if_version(
        &self,
        instance: WorkflowInstance,
        expected_version: u64,
    ) -> anyhow::Result<WorkflowInstance> {
        let _guard = self.write_lock.lock().await;
        match self.read_file(&instance.id).await? {
            Some(existing) if existing.version != expected_version => {
                bail!(
                    "stale workflow version: stored={}, expected={}",
                    existing.version,
                    expected_version
                );
            }
            None => bail!("workflow {} not found", instance.id),
            _ => {}
        }
        self.write_atomic(&instance).await?;
        Ok(instance)
    }
}
