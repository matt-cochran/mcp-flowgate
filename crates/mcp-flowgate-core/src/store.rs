use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

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
        let mut g = self.inner.lock().expect("LOCK_POISONED: workflow store");
        if g.contains_key(&instance.id) {
            bail!("workflow id collision: {}", instance.id);
        }
        g.insert(instance.id.clone(), instance.clone());
        Ok(instance)
    }

    async fn load(&self, workflow_id: &str) -> anyhow::Result<WorkflowInstance> {
        self.inner
            .lock()
            .expect("LOCK_POISONED: workflow store")
            .get(workflow_id)
            .cloned()
            .ok_or_else(|| anyhow!("workflow {} not found", workflow_id))
    }

    async fn save_if_version(
        &self,
        instance: WorkflowInstance,
        expected_version: u64,
    ) -> anyhow::Result<WorkflowInstance> {
        let mut g = self.inner.lock().expect("LOCK_POISONED: workflow store");
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

/// SPEC §8.4 — in-memory writable definition store, intended for the
/// authoring workflow's `registry` executor when
/// `flowgate.authoring.write_enabled` is true.
///
/// Audit-before-commit: `register` emits `definition.published` via the
/// supplied audit sink BEFORE the new snapshot becomes loadable. If audit
/// fails, the commit aborts and the new definition is NOT visible.
#[derive(Clone)]
pub struct InMemoryWritableDefinitionStore {
    inner: Arc<RwLock<HashMap<String, Value>>>,
    audit: Arc<dyn crate::audit::AuditSink>,
}

impl InMemoryWritableDefinitionStore {
    pub fn new(audit: Arc<dyn crate::audit::AuditSink>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            audit,
        }
    }

    /// Seed the store with an existing definition map (e.g. the resolved
    /// config at startup). Useful for tests and bootstrap.
    pub fn with_seed(audit: Arc<dyn crate::audit::AuditSink>, seed: HashMap<String, Value>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(seed)),
            audit,
        }
    }

    pub fn known_ids(&self) -> Vec<String> {
        self.inner.read().expect("LOCK_POISONED: writable definition store").keys().cloned().collect()
    }
}

#[async_trait]
impl DefinitionStore for InMemoryWritableDefinitionStore {
    async fn load(&self, definition_id: &str) -> anyhow::Result<Value> {
        self.inner
            .read()
            .expect("LOCK_POISONED: writable definition store")
            .get(definition_id)
            .cloned()
            .ok_or_else(|| anyhow!("workflow definition '{}' not found", definition_id))
    }
}

/// SPEC §5.9 — in-memory implementation of `GuidanceAcknowledgmentStore`.
/// Suitable for single-process gateways; replace with a persistent store
/// when ack must survive restarts.
#[derive(Default, Clone)]
pub struct InMemoryGuidanceAcknowledgmentStore {
    inner: Arc<RwLock<HashMap<(String, String), String>>>,
}

impl InMemoryGuidanceAcknowledgmentStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl crate::ports::GuidanceAcknowledgmentStore for InMemoryGuidanceAcknowledgmentStore {
    async fn record(
        &self,
        workflow_id: &str,
        subject: &str,
        body_hash: &str,
    ) -> anyhow::Result<()> {
        self.inner.write().expect("LOCK_POISONED: guidance acknowledgment store").insert(
            (workflow_id.to_string(), subject.to_string()),
            body_hash.to_string(),
        );
        Ok(())
    }

    async fn last_acknowledged_hash(
        &self,
        workflow_id: &str,
        subject: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(self
            .inner
            .read()
            .expect("LOCK_POISONED: guidance acknowledgment store")
            .get(&(workflow_id.to_string(), subject.to_string()))
            .cloned())
    }
}

/// SPEC §22 — in-memory implementation of `ScriptAcknowledgmentStore`.
/// Same shape as [`InMemoryGuidanceAcknowledgmentStore`] but in a distinct
/// keyspace so script acks don't pollute guidance acks (the two surfaces
/// can be wired independently).
#[derive(Default, Clone)]
pub struct InMemoryScriptAcknowledgmentStore {
    inner: Arc<RwLock<HashMap<(String, String), String>>>,
}

impl InMemoryScriptAcknowledgmentStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl crate::ports::ScriptAcknowledgmentStore for InMemoryScriptAcknowledgmentStore {
    async fn record(
        &self,
        workflow_id: &str,
        subject: &str,
        body_hash: &str,
    ) -> anyhow::Result<()> {
        self.inner.write().expect("LOCK_POISONED: script acknowledgment store").insert(
            (workflow_id.to_string(), subject.to_string()),
            body_hash.to_string(),
        );
        Ok(())
    }

    async fn last_acknowledged_hash(
        &self,
        workflow_id: &str,
        subject: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(self
            .inner
            .read()
            .expect("LOCK_POISONED: script acknowledgment store")
            .get(&(workflow_id.to_string(), subject.to_string()))
            .cloned())
    }
}

#[async_trait]
impl crate::ports::DefinitionStoreWritable for InMemoryWritableDefinitionStore {
    async fn register(&self, definition_id: &str, definition: Value) -> anyhow::Result<()> {
        // Audit-before-commit (SPEC §8.4). If this fails, abort.
        let event = crate::audit::AuditEvent::new("definition.published")
            .with_payload(serde_json::json!({
                "definitionId": definition_id,
                "outcome":      "pending_commit",
            }));
        if let Err(e) = self.audit.record(event).await {
            anyhow::bail!(
                "RECORD_WRITE_FAILED: audit of definition.published for '{definition_id}' failed: {e}"
            );
        }
        // Commit becomes visible only after audit succeeded.
        {
            let mut guard = self.inner.write().expect("LOCK_POISONED: writable definition store");
            guard.insert(definition_id.to_string(), definition);
        }
        // Post-commit best-effort event (mirrors §5.8 non-critical semantics).
        // A self-event surfaces audit-write failure; we can't use
        // `record_or_self_event` here because that helper lives on
        // `WorkflowRuntime`, not on the store. Inline the pattern.
        let post = crate::audit::AuditEvent::new("definition.loadable")
            .with_payload(serde_json::json!({
                "definitionId": definition_id,
                "outcome":      "loadable",
            }));
        if let Err(primary_err) = self.audit.record(post).await {
            let self_event = crate::audit::AuditEvent::new("audit.write_failed")
                .with_payload(serde_json::json!({
                    "originalEvent": "definition.loadable",
                    "definitionId":  definition_id,
                    "error":         primary_err.to_string(),
                }));
            if let Err(inner) = self.audit.record(self_event).await {
                tracing::warn!(
                    definition_id = %definition_id,
                    primary_err = %primary_err,
                    selfevt_err = %inner,
                    "non-critical definition.loadable audit failed; \
                     self-event also failed"
                );
            }
        }
        Ok(())
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
        let mut g = self.inner.lock().expect("LOCK_POISONED: evidence store");
        g.entry(workflow_id.to_string()).or_default().push(evidence);
        Ok(())
    }

    async fn list(&self, workflow_id: &str) -> anyhow::Result<Vec<Evidence>> {
        Ok(self
            .inner
            .lock()
            .expect("LOCK_POISONED: evidence store")
            .get(workflow_id)
            .cloned()
            .unwrap_or_default())
    }
}
