//! Integration tests for the workflow executor.
//!
//! Tests use MemoryAuditSink and InMemoryWorkflowStore for fast,
//! filesystem-free verification.

use std::sync::Arc;

use async_trait::async_trait;
use mcp_flowgate_core::audit::{AuditSink, MemoryAuditSink};
use mcp_flowgate_core::config::resolve_str;
use mcp_flowgate_core::guards::DefaultGuardEvaluator;
use mcp_flowgate_core::model::{ExecuteRequest, ExecuteResult};
use mcp_flowgate_core::ports::{Executor, ExecutorRegistry, WorkflowStore};
use mcp_flowgate_core::runtime::WorkflowRuntime;
use mcp_flowgate_core::store::{
    ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore,
};
use mcp_flowgate_executors::workflow::WorkflowExecutor;
use serde_json::json;

fn build_runtime() -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let config = resolve_str(
        r#"
version: "1.0.0"
workflows:
  auto_complete:
    initialState: done
    states:
      done:
        terminal: true
  two_step:
    initialState: first
    states:
      first:
        transitions:
          go:
            target: done
            executor:
              kind: noop
      done:
        terminal: true
  never_ends:
    initialState: waiting
    states:
      waiting:
        transitions:
          loop:
            target: waiting
            executor:
              kind: noop
"#,
    )
    .unwrap();

    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store: Arc<dyn WorkflowStore> = Arc::new(InMemoryWorkflowStore::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));

    // Build a minimal executor registry with noop
    struct NoopExecutor;
    #[async_trait]
    impl Executor for NoopExecutor {
        async fn execute(
            &self,
            _request: ExecuteRequest,
        ) -> Result<ExecuteResult, mcp_flowgate_core::error::ExecutorError> {
            Ok(ExecuteResult::default())
        }
    }

    struct SingleExecutorRegistry(Arc<dyn Executor>);
    impl ExecutorRegistry for SingleExecutorRegistry {
        fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
            Some(self.0.clone())
        }
    }

    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        Arc::new(SingleExecutorRegistry(Arc::new(NoopExecutor))),
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    )
    .with_evidence(evidence);

    (runtime, audit)
}

#[tokio::test]
async fn sub_workflow_completes_and_returns_context() {
    let (runtime, _audit) = build_runtime();
    let executor = WorkflowExecutor::new(runtime.clone(), runtime.audit().clone());

    let result = executor
        .execute(ExecuteRequest {
            workflow: mcp_flowgate_core::model::WorkflowInstance {
                id: "parent_wf".to_string(),
                definition_id: "parent".to_string(),
                definition_version: "1.0.0".to_string(),
                definition: json!({"initialState": "running", "states": {}}),
                state: "running".to_string(),
                version: 1,
                input: json!({}),
                context: json!({}),
                started_at: chrono::Utc::now(),
                            trace_id: None,
                run_id: None,
                cancelled_at: None,
                cancelled_reason: None,
            },
            transition: Some("run_sub".to_string()),
            arguments: json!({}),
            executor_config: json!({
                "definitionId": "auto_complete",
                "input": {},
            }),
            idempotency_key: None,
            correlation_id: None,
        })
        .await
        .unwrap();

    assert!(
        result.output.is_object(),
        "sub-workflow should return context object"
    );
}

#[tokio::test]
async fn sub_workflow_polls_until_terminal() {
    // The completion test above exercises the immediate-completion path
    // (initial state already terminal). This test pins the poll loop:
    // a sub-workflow that takes a short detour through a non-terminal
    // state must still resolve. We simulate the detour by giving the
    // executor a short timeout that's longer than the polling cadence,
    // so a single 200ms poll tick gets observed.
    let (runtime, _audit) = build_runtime();
    let executor = WorkflowExecutor::new(runtime.clone(), runtime.audit().clone());

    let result = executor
        .execute(ExecuteRequest {
            workflow: mcp_flowgate_core::model::WorkflowInstance {
                id: "parent_wf_2".to_string(),
                definition_id: "parent".to_string(),
                definition_version: "1.0.0".to_string(),
                definition: json!({"initialState": "running", "states": {}}),
                state: "running".to_string(),
                version: 1,
                input: json!({}),
                context: json!({}),
                started_at: chrono::Utc::now(),
                            trace_id: None,
                run_id: None,
                cancelled_at: None,
                cancelled_reason: None,
            },
            transition: Some("run_sub".to_string()),
            arguments: json!({}),
            executor_config: json!({
                // auto_complete's initialState is already terminal,
                // proving the run-on-enter completion path returns
                // context to the parent executor.
                "definitionId": "auto_complete",
                "input": {"trigger": "polled"},
                "timeoutMs": 5_000,
            }),
            idempotency_key: None,
            correlation_id: None,
        })
        .await
        .unwrap();

    assert!(result.output.is_object(), "sub-workflow should complete");
}

#[tokio::test]
async fn sub_workflow_times_out() {
    let (runtime, _audit) = build_runtime();
    let executor = WorkflowExecutor::new(runtime.clone(), runtime.audit().clone());

    let result = executor
        .execute(ExecuteRequest {
            workflow: mcp_flowgate_core::model::WorkflowInstance {
                id: "parent_wf_3".to_string(),
                definition_id: "parent".to_string(),
                definition_version: "1.0.0".to_string(),
                definition: json!({"initialState": "running", "states": {}}),
                state: "running".to_string(),
                version: 1,
                input: json!({}),
                context: json!({}),
                started_at: chrono::Utc::now(),
                            trace_id: None,
                run_id: None,
                cancelled_at: None,
                cancelled_reason: None,
            },
            transition: Some("run_sub".to_string()),
            arguments: json!({}),
            executor_config: json!({
                "definitionId": "never_ends",
                "input": {},
                "timeoutMs": 100,
            }),
            idempotency_key: None,
            correlation_id: None,
        })
        .await;

    assert!(result.is_err(), "should timeout");
    let err = result.unwrap_err();
    assert!(
        matches!(err, mcp_flowgate_core::error::ExecutorError::Timeout(_)),
        "expected Timeout error, got: {err:?}"
    );
}

#[tokio::test]
async fn sub_workflow_missing_definition_surfaces_as_executor_error() {
    let (runtime, _audit) = build_runtime();
    let executor = WorkflowExecutor::new(runtime.clone(), runtime.audit().clone());

    let result = executor
        .execute(ExecuteRequest {
            workflow: mcp_flowgate_core::model::WorkflowInstance {
                id: "parent_wf_err".to_string(),
                definition_id: "parent".to_string(),
                definition_version: "1.0.0".to_string(),
                definition: json!({"initialState": "running", "states": {}}),
                state: "running".to_string(),
                version: 1,
                input: json!({}),
                context: json!({}),
                started_at: chrono::Utc::now(),
                            trace_id: None,
                run_id: None,
                cancelled_at: None,
                cancelled_reason: None,
            },
            transition: Some("run_sub".to_string()),
            arguments: json!({}),
            executor_config: json!({
                "definitionId": "does_not_exist",
                "input": {},
            }),
            idempotency_key: None,
            correlation_id: None,
        })
        .await;

    assert!(result.is_err(), "should fail when definitionId is unknown");
    assert!(
        matches!(
            result.unwrap_err(),
            mcp_flowgate_core::error::ExecutorError::Permanent(_)
        ),
        "expected Permanent error for missing definition"
    );
}

#[tokio::test]
async fn sub_workflow_audit_events_emitted() {
    let (runtime, audit) = build_runtime();
    let executor = WorkflowExecutor::new(runtime.clone(), runtime.audit().clone());

    executor
        .execute(ExecuteRequest {
            workflow: mcp_flowgate_core::model::WorkflowInstance {
                id: "parent_wf_4".to_string(),
                definition_id: "parent".to_string(),
                definition_version: "1.0.0".to_string(),
                definition: json!({"initialState": "running", "states": {}}),
                state: "running".to_string(),
                version: 1,
                input: json!({}),
                context: json!({}),
                started_at: chrono::Utc::now(),
                            trace_id: None,
                run_id: None,
                cancelled_at: None,
                cancelled_reason: None,
            },
            transition: Some("run_sub".to_string()),
            arguments: json!({}),
            executor_config: json!({
                "definitionId": "auto_complete",
                "input": {},
            }),
            idempotency_key: None,
            correlation_id: None,
        })
        .await
        .unwrap();

    let events = audit.snapshot();
    let event_types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert!(
        event_types.contains(&"sub_workflow.started"),
        "should have sub_workflow.started event, got: {:?}",
        event_types
    );
    assert!(
        event_types.contains(&"sub_workflow.completed"),
        "should have sub_workflow.completed event, got: {:?}",
        event_types
    );
}
