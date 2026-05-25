use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::ExecutorError;
use crate::model::*;

#[async_trait]
pub trait DefinitionStore: Send + Sync {
    async fn load(&self, definition_id: &str) -> anyhow::Result<Value>;
}

/// SPEC §8.4 — opt-in writable extension of `DefinitionStore`. Implementations
/// are constructed only when `flowgate.authoring.write_enabled` is true at
/// gateway startup. Runtime call sites hold this as
/// `Option<Arc<dyn DefinitionStoreWritable>>` — `None` means the write path
/// is disabled and the registry executor fails fast with `WRITE_DISABLED`.
///
/// The implementation MUST emit `definition.published` to the audit sink
/// BEFORE the new snapshot becomes loadable (audit-before-commit), mirroring
/// SPEC §7.3 record-first ordering for transition records. Audit failure
/// MUST abort the commit and return an error containing `RECORD_WRITE_FAILED`
/// in its display.
#[async_trait]
pub trait DefinitionStoreWritable: DefinitionStore {
    async fn register(&self, definition_id: &str, definition: Value) -> anyhow::Result<()>;
}

#[async_trait]
pub trait WorkflowStore: Send + Sync {
    async fn create(&self, instance: WorkflowInstance) -> anyhow::Result<WorkflowInstance>;
    async fn load(&self, workflow_id: &str) -> anyhow::Result<WorkflowInstance>;
    async fn save_if_version(
        &self,
        instance: WorkflowInstance,
        expected_version: u64,
    ) -> anyhow::Result<WorkflowInstance>;
}

#[async_trait]
pub trait Executor: Send + Sync {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError>;
}

pub trait ExecutorRegistry: Send + Sync {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>>;
}

#[async_trait]
pub trait GuardEvaluator: Send + Sync {
    async fn evaluate(
        &self,
        guard: &Value,
        instance: &WorkflowInstance,
        arguments: &Value,
        principal: &Principal,
    ) -> anyhow::Result<bool>;

    /// SPEC §20.1 + §20.4 — when a guard rejects for a specific named
    /// reason (e.g. `EVIDENCE_DIGEST_REQUIRED`,
    /// `EVIDENCE_CONFIDENCE_BELOW_THRESHOLD`), implementers return the code
    /// alongside the pass/fail bool so the caller can surface the precise
    /// rejection in `error.code` instead of generic `GUARD_REJECTED`.
    ///
    /// Default impl delegates to `evaluate` and returns `None` for the
    /// diagnostic — preserves backward compat for any external implementer.
    async fn evaluate_with_diagnostic(
        &self,
        guard: &Value,
        instance: &WorkflowInstance,
        arguments: &Value,
        principal: &Principal,
    ) -> anyhow::Result<(bool, Option<String>)> {
        let pass = self
            .evaluate(guard, instance, arguments, principal)
            .await?;
        Ok((pass, None))
    }
}

#[async_trait]
pub trait EvidenceStore: Send + Sync {
    /// Append a new evidence record for the given workflow.
    async fn record(&self, workflow_id: &str, evidence: Evidence) -> anyhow::Result<()>;

    /// Return every recorded evidence item for a workflow.
    async fn list(&self, workflow_id: &str) -> anyhow::Result<Vec<Evidence>>;
}

/// SPEC §5.9 — tracks `gateway.describe` calls per workflow + subject so the
/// `guidance_acknowledged` guard (§17.4) can verify that the body was
/// fetched AND that the fetched body's hash still matches the current
/// definition snapshot. Hash-flip invalidation is the TRIZ-bounded
/// semantic teeth (FMECA FM-4): we can't prove the LLM *read* the body,
/// but we can prove it fetched the *current* one.
#[async_trait]
pub trait GuidanceAcknowledgmentStore: Send + Sync {
    /// Record that `subject` was fetched for `workflow_id` while the body's
    /// normalized hash was `body_hash`.
    async fn record(
        &self,
        workflow_id: &str,
        subject: &str,
        body_hash: &str,
    ) -> anyhow::Result<()>;

    /// Return the hash of the body last fetched for `(workflow_id, subject)`,
    /// or `None` if no fetch was recorded.
    async fn last_acknowledged_hash(
        &self,
        workflow_id: &str,
        subject: &str,
    ) -> anyhow::Result<Option<String>>;
}
