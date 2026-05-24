//! Tests for graceful drain behavior on the workflow runtime.
//!
//! The end-to-end signal handling lives in `mcp-flowgate/src/main.rs`; here
//! we pin the contract that `WorkflowRuntime::begin_drain()` switches the
//! runtime into a state where `start` is refused but `submit`/`get` keep
//! working so in-flight workflows can finish.

use std::sync::Arc;

use async_trait::async_trait;
use mcp_flowgate_core::audit::{AuditSink, MemoryAuditSink};
use mcp_flowgate_core::config::resolve_str;
use mcp_flowgate_core::error::ExecutorError;
use mcp_flowgate_core::guards::DefaultGuardEvaluator;
use mcp_flowgate_core::model::{
    ExecuteRequest, ExecuteResult, GetWorkflow, Principal, StartWorkflow, SubmitTransition,
};
use mcp_flowgate_core::ports::{Executor, ExecutorRegistry};
use mcp_flowgate_core::store::{
    ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore,
};
use mcp_flowgate_core::WorkflowRuntime;
use serde_json::json;

fn config() -> &'static str {
    r#"
version: "1.0.0"
workflows:
  drain_demo:
    initialState: open
    states:
      open:
        transitions:
          go:
            target: done
            executor: { kind: noop }
      done:
        terminal: true
"#
}

struct NoopExec;
#[async_trait]
impl Executor for NoopExec {
    async fn execute(&self, _req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult::default())
    }
}
struct AnyKind(Arc<dyn Executor>);
impl ExecutorRegistry for AnyKind {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(self.0.clone())
    }
}

fn build_runtime() -> WorkflowRuntime {
    let cfg = resolve_str(config()).unwrap();
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));
    WorkflowRuntime::new(
        definitions,
        store,
        Arc::new(AnyKind(Arc::new(NoopExec))),
        guards,
        audit,
    )
    .with_evidence(evidence)
}

#[tokio::test]
async fn drain_flag_starts_false() {
    let rt = build_runtime();
    assert!(!rt.is_draining());
}

#[tokio::test]
async fn drain_rejects_new_workflows() {
    let rt = build_runtime();
    rt.begin_drain();
    assert!(rt.is_draining());

    let err = rt
        .start(StartWorkflow {
            definition_id: "drain_demo".to_string(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .expect_err("start should fail while draining");
    assert!(
        err.to_string().contains("shutting down"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn drain_allows_inflight_submit_and_get() {
    let rt = build_runtime();

    // Start a workflow *before* drain begins.
    let start = rt
        .start(StartWorkflow {
            definition_id: "drain_demo".to_string(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    let wf_id = start["workflow"]["id"].as_str().unwrap().to_string();
    let version = start["workflow"]["version"].as_u64().unwrap();

    // Now drain.
    rt.begin_drain();

    // get still works.
    let got = rt
        .get(GetWorkflow {
            workflow_id: wf_id.clone(),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    assert_eq!(got["workflow"]["id"], wf_id);

    // submit still works — the in-flight workflow can drive to completion.
    let submitted = rt
        .submit(SubmitTransition {
            workflow_id: wf_id,
            expected_version: version,
            transition: "go".to_string(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
        })
        .await
        .unwrap();
    assert_eq!(submitted["workflow"]["state"], "done");
}
