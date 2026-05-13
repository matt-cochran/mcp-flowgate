use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::ExecutorError;
use crate::model::*;

#[async_trait]
pub trait DefinitionStore: Send + Sync {
    async fn load(&self, definition_id: &str) -> anyhow::Result<Value>;
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
}

#[async_trait]
pub trait EvidenceStore: Send + Sync {
    /// Append a new evidence record for the given workflow.
    async fn record(&self, workflow_id: &str, evidence: Evidence) -> anyhow::Result<()>;

    /// Return every recorded evidence item for a workflow.
    async fn list(&self, workflow_id: &str) -> anyhow::Result<Vec<Evidence>>;
}
