//! A `workflow` executor that starts a sub-workflow and waits for it to
//! complete. This enables declarative cross-workflow composition:
//!
//! ```yaml
//! executor:
//!   kind: workflow
//!   definitionId: with_artifact_lock
//!   input:
//!     artifact: "$.context.artifact_name"
//!     owner: "$.workflow.input.user"
//!   timeoutMs: 60000
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use mcp_flowgate_core::audit::{AuditEvent, AuditSink};
use mcp_flowgate_core::error::ExecutorError;
use mcp_flowgate_core::model::{
    ExecuteRequest, ExecuteResult, GetWorkflow, Principal, StartWorkflow,
};
use mcp_flowgate_core::ports::Executor;
use mcp_flowgate_core::runtime::WorkflowRuntime;

/// Maximum nesting depth for sub-workflows to prevent infinite recursion.
#[allow(dead_code)]
const MAX_WORKFLOW_DEPTH: u32 = 10;

pub struct WorkflowExecutor {
    runtime: WorkflowRuntime,
    audit: Arc<dyn AuditSink>,
}

impl WorkflowExecutor {
    pub fn new(runtime: WorkflowRuntime, audit: Arc<dyn AuditSink>) -> Self {
        Self { runtime, audit }
    }
}

#[async_trait]
impl Executor for WorkflowExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let definition_id = request
            .executor_config
            .get("definitionId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ExecutorError::Permanent("workflow executor requires 'definitionId'".to_string())
            })?;

        let input = request
            .executor_config
            .get("input")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let timeout_ms = request
            .executor_config
            .get("timeoutMs")
            .and_then(Value::as_u64);

        // Resolve input paths against context/arguments
        let resolved_input = resolve_input(&input, &request.workflow.context, &request.arguments);

        // Start the sub-workflow
        let start_resp = self
            .runtime
            .start(StartWorkflow {
                definition_id: definition_id.to_string(),
                input: resolved_input,
                principal: Principal::anonymous(),
            })
            .await
            .map_err(|e| ExecutorError::Permanent(format!("failed to start sub-workflow: {e}")))?;

        let sub_workflow_id = start_resp
            .pointer("/workflow/id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ExecutorError::Permanent("sub-workflow response missing workflow.id".to_string())
            })?;

        // Emit audit event
        self.audit
            .record(
                AuditEvent::new("sub_workflow.started")
                    .with_workflow(request.workflow.id.clone())
                    .with_payload(json!({
                        "sub_workflow_id": sub_workflow_id,
                        "definition_id": definition_id,
                    })),
            )
            .await
            .ok();

        // Poll until terminal or timeout
        let start_time = std::time::Instant::now();
        loop {
            if let Some(timeout) = timeout_ms {
                if start_time.elapsed().as_millis() as u64 > timeout {
                    self.audit
                        .record(
                            AuditEvent::new("sub_workflow.timed_out")
                                .with_workflow(request.workflow.id.clone())
                                .with_payload(json!({
                                    "sub_workflow_id": sub_workflow_id,
                                    "timeout_ms": timeout,
                                })),
                        )
                        .await
                        .ok();
                    return Err(ExecutorError::Timeout(timeout));
                }
            }

            let get_resp = self
                .runtime
                .get(GetWorkflow {
                    workflow_id: sub_workflow_id.to_string(),
                    principal: Principal::anonymous(),
                })
                .await
                .map_err(|e| {
                    ExecutorError::Permanent(format!("failed to get sub-workflow: {e}"))
                })?;

            let status = get_resp
                .pointer("/result/status")
                .and_then(Value::as_str)
                .unwrap_or("running");

            match status {
                "completed" => {
                    let context = get_resp
                        .pointer("/workflow/context")
                        .cloned()
                        .unwrap_or_else(|| json!({}));

                    self.audit
                        .record(
                            AuditEvent::new("sub_workflow.completed")
                                .with_workflow(request.workflow.id.clone())
                                .with_payload(json!({
                                    "sub_workflow_id": sub_workflow_id,
                                })),
                        )
                        .await
                        .ok();

                    return Ok(ExecuteResult {
                        output: context,
                        evidence: vec![],
                    });
                }
                "failed" | "timed_out" => {
                    self.audit
                        .record(
                            AuditEvent::new("sub_workflow.failed")
                                .with_workflow(request.workflow.id.clone())
                                .with_payload(json!({
                                    "sub_workflow_id": sub_workflow_id,
                                    "status": status,
                                })),
                        )
                        .await
                        .ok();

                    return Err(ExecutorError::Permanent(format!(
                        "sub-workflow reached terminal state '{status}'"
                    )));
                }
                _ => {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    }
}

fn resolve_input(input: &Value, context: &Value, arguments: &Value) -> Value {
    match input {
        Value::String(s) if s.starts_with("$.") => {
            mcp_flowgate_core::mapping::read_in_scopes(s, arguments, context, &json!({}), None)
                .unwrap_or(Value::Null)
        }
        Value::Object(map) => {
            let mut resolved = serde_json::Map::new();
            for (k, v) in map {
                resolved.insert(k.clone(), resolve_input(v, context, arguments));
            }
            Value::Object(resolved)
        }
        other => other.clone(),
    }
}
