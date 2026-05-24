use std::sync::Arc;

use async_trait::async_trait;
use mcp_flowgate_core::audit::{AuditEvent, AuditSink, NullAuditSink};
use mcp_flowgate_core::error::ExecutorError;
use mcp_flowgate_core::model::{Evidence, ExecuteRequest, ExecuteResult};
use mcp_flowgate_core::ports::Executor;
use serde_json::{json, Value};
use uuid::Uuid;

/// Human-in-the-loop executor. Records `human.approval.requested` and returns
/// success with queue metadata — the actual approval comes via a later
/// `workflow.submit` from a human principal. Pair with
/// `actor: human` and `kind: permission` guards on the receiving transition.
pub struct HumanExecutor {
    audit: Arc<dyn AuditSink>,
}

impl HumanExecutor {
    pub fn new() -> Self {
        Self {
            audit: Arc::new(NullAuditSink),
        }
    }

    pub fn with_audit(audit: Arc<dyn AuditSink>) -> Self {
        Self { audit }
    }
}

impl Default for HumanExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for HumanExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let queue = request
            .executor_config
            .get("queue")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_string();

        let request_id = format!("hr_{}", Uuid::new_v4().simple());

        let _ = self
            .audit
            .record(
                AuditEvent::new("human.approval.requested")
                    .with_workflow(&request.workflow.id)
                    .with_payload(json!({
                        "queue": queue,
                        "requestId": request_id,
                        "transition": request.transition,
                    })),
            )
            .await;

        Ok(ExecuteResult {
            output: json!({
                "queue": queue,
                "requestId": request_id,
                "status": "queued",
            }),
            evidence: vec![Evidence {
                kind: "human_request".to_string(),
                id: request_id,
                uri: None,
                summary: Some(format!("Human action queued in '{queue}'")),
            }],
            child_workflow_id: None,
        })
    }
}
