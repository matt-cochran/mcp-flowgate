//! Acceptance test for the `flowgate-meta` sibling repo — walk each
//! of the four meta-authoring orchestrators (`meta/flow.author-flow`,
//! `meta/flow.author-capability`, `meta/flow.optimize-flow`,
//! `meta/flow.optimize-capability`) through their full lifecycle to a
//! terminal state, against the vendored fixture under
//! `crates/mcp-flowgate-core/tests/fixtures/flowgate-meta/`.
//!
//! Same harness pattern as `flow_orchestrators_e2e.rs` — see that
//! file's module doc for the OnceLock-based WorkflowExecutor wiring
//! rationale and the "test lives in executors crate to avoid a
//! core→executors dev-dep cycle" caveat.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mcp_flowgate_core::audit::{AuditSink, MemoryAuditSink};
use mcp_flowgate_core::config::load_resolved_with_repos;
use mcp_flowgate_core::error::ExecutorError;
use mcp_flowgate_core::guards::DefaultGuardEvaluator;
use mcp_flowgate_core::model::{ExecuteRequest, ExecuteResult, Principal, StartWorkflow};
use mcp_flowgate_core::ports::{Executor, ExecutorRegistry, WorkflowStore};
use mcp_flowgate_core::runtime::WorkflowRuntime;
use mcp_flowgate_core::store::{
    ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore,
};
use mcp_flowgate_executors::workflow::WorkflowExecutor;
use serde_json::{json, Value};
use tempfile::TempDir;

struct MetaRegistry {
    workflow_executor: OnceLock<Arc<WorkflowExecutor>>,
}
impl MetaRegistry {
    fn new() -> Self {
        Self { workflow_executor: OnceLock::new() }
    }
    fn install(&self, exec: Arc<WorkflowExecutor>) {
        self.workflow_executor.set(exec).map_err(|_| ()).unwrap();
    }
}
impl ExecutorRegistry for MetaRegistry {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        if kind == "workflow" {
            return self
                .workflow_executor
                .get()
                .map(|w| w.clone() as Arc<dyn Executor>);
        }
        Some(Arc::new(NoopExecutor))
    }
}
struct NoopExecutor;
#[async_trait]
impl Executor for NoopExecutor {
    async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult::default())
    }
}

fn fixture_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("crates");
    p.push("mcp-flowgate-core");
    p.push("tests");
    p.push("fixtures");
    p.push("flowgate-meta");
    p
}

fn write_host_config(td: &TempDir) -> PathBuf {
    let body = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
        fixture_path().display()
    );
    let p = td.path().join("flowgate.yaml");
    std::fs::write(&p, body).unwrap();
    p
}

async fn build_runtime(config: &Value) -> WorkflowRuntime {
    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(config));
    let store: Arc<dyn WorkflowStore> = Arc::new(InMemoryWorkflowStore::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));
    let registry = Arc::new(MetaRegistry::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store.clone(),
        registry.clone() as Arc<dyn ExecutorRegistry>,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    )
    .with_evidence(evidence);
    registry.install(Arc::new(WorkflowExecutor::new(
        runtime.clone(),
        audit.clone() as Arc<dyn AuditSink>,
    )));
    runtime
}

async fn walk_to_terminal(definition_id: &str, input: Value, config: &Value) -> Value {
    let runtime = build_runtime(config).await;
    let resp = runtime
        .start(StartWorkflow {
            definition_id: definition_id.to_string(),
            input,
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap_or_else(|e| panic!("start({definition_id}): {e}"));

    let status = resp.pointer("/result/status").and_then(Value::as_str).unwrap_or("?");
    let state = resp.pointer("/workflow/state").and_then(Value::as_str).unwrap_or("?");
    assert_eq!(
        state, "done",
        "{definition_id} should walk to terminal 'done'; got state='{state}' status='{status}'. \
         resp: {resp:#}"
    );
    assert_eq!(
        status, "completed",
        "{definition_id} status; resp: {resp:#}"
    );
    resp
}

#[tokio::test]
async fn meta_flow_author_capability_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let _ = walk_to_terminal(
        "meta/flow.author-capability",
        json!({
            "goal":      "Author a cap.test.python-pytest capability",
            "namespace": "draft",
            "base_ref":  "main"
        }),
        &config,
    )
    .await;
}

#[tokio::test]
async fn meta_flow_author_flow_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let _ = walk_to_terminal(
        "meta/flow.author-flow",
        json!({
            "goal":      "Author flow.deploy-helm-chart orchestrator",
            "namespace": "draft",
            "base_ref":  "main"
        }),
        &config,
    )
    .await;
}

#[tokio::test]
async fn meta_flow_optimize_capability_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let _ = walk_to_terminal(
        "meta/flow.optimize-capability",
        json!({
            "target_definition_id": "cognitive/cap.implement.tdd-loop",
            "base_ref":             "main"
        }),
        &config,
    )
    .await;
}

#[tokio::test]
async fn meta_flow_optimize_flow_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let _ = walk_to_terminal(
        "meta/flow.optimize-flow",
        json!({
            "target_definition_id": "cognitive/flow.add-feature",
            "base_ref":             "main"
        }),
        &config,
    )
    .await;
}
