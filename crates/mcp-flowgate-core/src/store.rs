use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail};
use async_trait::async_trait;
use serde_json::Value;

use crate::model::{Evidence, WorkflowInstance};
use crate::ports::{DefinitionStore, EvidenceStore, WorkflowStore};
use crate::proxy_workflow::{compile_proxy_workflow, DEFAULT_PROXY_WORKFLOW_ID};

/// In-memory workflow store with optimistic concurrency on `version`.
#[derive(Default, Clone)]
pub struct InMemoryWorkflowStore {
    inner: Arc<Mutex<HashMap<String, WorkflowInstance>>>,
}

impl InMemoryWorkflowStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl WorkflowStore for InMemoryWorkflowStore {
    async fn create(&self, instance: WorkflowInstance) -> anyhow::Result<WorkflowInstance> {
        let mut g = self.inner.lock().unwrap();
        if g.contains_key(&instance.id) {
            bail!("workflow id collision: {}", instance.id);
        }
        g.insert(instance.id.clone(), instance.clone());
        Ok(instance)
    }

    async fn load(&self, workflow_id: &str) -> anyhow::Result<WorkflowInstance> {
        self.inner
            .lock()
            .unwrap()
            .get(workflow_id)
            .cloned()
            .ok_or_else(|| anyhow!("workflow {} not found", workflow_id))
    }

    async fn save_if_version(
        &self,
        instance: WorkflowInstance,
        expected_version: u64,
    ) -> anyhow::Result<WorkflowInstance> {
        let mut g = self.inner.lock().unwrap();
        match g.get(&instance.id) {
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
        g.insert(instance.id.clone(), instance.clone());
        Ok(instance)
    }
}

/// Definition store backed by an in-memory map of workflow JSON values. Built
/// from a parsed gateway config: every `workflows.*` entry is registered, and
/// if the config has any `proxy.expose` entries, a `proxy_default` workflow is
/// compiled and registered.
#[derive(Clone, Default)]
pub struct ConfigDefinitionStore {
    defs: Arc<HashMap<String, Value>>,
}

impl ConfigDefinitionStore {
    pub fn new(defs: HashMap<String, Value>) -> Self {
        Self {
            defs: Arc::new(defs),
        }
    }

    /// Build a definition store from a parsed gateway config Value.
    /// - registers every `workflows.<id>` definition
    /// - if `proxy.expose` is non-empty, compiles a `proxy_default` workflow
    pub fn from_config(config: &Value) -> Self {
        let mut defs = HashMap::new();

        if let Some(map) = config.pointer("/workflows").and_then(Value::as_object) {
            for (id, def) in map {
                defs.insert(id.clone(), def.clone());
            }
        }

        if let Some(proxy) = compile_proxy_workflow(config) {
            defs.insert(DEFAULT_PROXY_WORKFLOW_ID.to_string(), proxy);
        }

        Self::new(defs)
    }

    pub fn ids(&self) -> Vec<String> {
        self.defs.keys().cloned().collect()
    }
}

#[async_trait]
impl DefinitionStore for ConfigDefinitionStore {
    async fn load(&self, definition_id: &str) -> anyhow::Result<Value> {
        self.defs
            .get(definition_id)
            .cloned()
            .ok_or_else(|| anyhow!("workflow definition '{}' not found", definition_id))
    }
}

/// In-memory evidence store. Each workflow id maps to its append-only list
/// of evidence records.
#[derive(Default, Clone)]
pub struct InMemoryEvidenceStore {
    inner: Arc<Mutex<HashMap<String, Vec<Evidence>>>>,
}

impl InMemoryEvidenceStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl EvidenceStore for InMemoryEvidenceStore {
    async fn record(&self, workflow_id: &str, evidence: Evidence) -> anyhow::Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.entry(workflow_id.to_string()).or_default().push(evidence);
        Ok(())
    }

    async fn list(&self, workflow_id: &str) -> anyhow::Result<Vec<Evidence>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .get(workflow_id)
            .cloned()
            .unwrap_or_default())
    }
}
