//! SPEC §17.2 + §8.4 — `registry` executor. Writes a candidate definition
//! through `DefinitionStoreWritable`. Feature-flagged: with
//! `flowgate.authoring.write_enabled: false` (default), the executor fails
//! fast with `WRITE_DISABLED` and performs no I/O.

use std::sync::Arc;

use async_trait::async_trait;
use mcp_flowgate_core::error::ExecutorError;
use mcp_flowgate_core::model::{ExecuteRequest, ExecuteResult};
use mcp_flowgate_core::ports::{DefinitionStoreWritable, Executor};
use serde_json::{json, Value};

/// Construct with `Some(writable)` when the flag is on, `None` when off.
/// The executor's behavior is governed by the variant: when `None`, every
/// invocation fails fast with `WRITE_DISABLED`.
pub struct RegistryExecutor {
    writable: Option<Arc<dyn DefinitionStoreWritable>>,
}

impl RegistryExecutor {
    pub fn new(writable: Option<Arc<dyn DefinitionStoreWritable>>) -> Self {
        Self { writable }
    }

    pub fn enabled(writable: Arc<dyn DefinitionStoreWritable>) -> Self {
        Self {
            writable: Some(writable),
        }
    }

    pub fn disabled() -> Self {
        Self { writable: None }
    }
}

#[async_trait]
impl Executor for RegistryExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let args = &request.arguments;
        let definition_id = args
            .get("definition_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ExecutorError::Permanent("registry: missing required argument `definition_id`".into())
            })?;
        let definition = args.get("definition").cloned().ok_or_else(|| {
            ExecutorError::Permanent("registry: missing required argument `definition`".into())
        })?;

        let Some(writable) = self.writable.as_ref() else {
            return Ok(ExecuteResult {
                output: json!({
                    "error": "WRITE_DISABLED",
                    "message": "registry executor invoked while \
                                flowgate.authoring.write_enabled is false",
                }),
                evidence: vec![],
                child_workflow_id: None,
            });
        };

        match writable.register(definition_id, definition).await {
            Ok(()) => Ok(ExecuteResult {
                output: json!({
                    "definitionId": definition_id,
                    "outcome":      "published",
                }),
                evidence: vec![],
                child_workflow_id: None,
            }),
            Err(e) => Err(ExecutorError::Permanent(format!(
                "registry: register('{definition_id}') failed: {e}"
            ))),
        }
    }
}
