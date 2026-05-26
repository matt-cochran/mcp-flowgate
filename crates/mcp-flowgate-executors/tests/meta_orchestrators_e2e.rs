//! M4 acceptance — walk flowgate-meta v0.1's four meta-authoring
//! orchestrators (`meta/flow.author-capability`, `meta/flow.author-flow`,
//! `meta/flow.optimize-capability`, `meta/flow.optimize-flow`) through
//! their full lifecycle to a terminal state.
//!
//! Same fixture-executor pattern as `flow_orchestrators_e2e.rs` — see
//! that file's module doc for the rationale.

use std::path::PathBuf;
use std::sync::Arc;

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
use serde_json::{json, Value};
use tempfile::TempDir;

struct CapShortCircuit;
#[async_trait]
impl Executor for CapShortCircuit {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let snippet_outputs = request
            .executor_config
            .get("_snippetOutputs")
            .cloned()
            .unwrap_or_else(|| json!({}));
        Ok(ExecuteResult {
            output: synthesize_outputs(&snippet_outputs),
            evidence: vec![],
            child_workflow_id: Some("fixture-cap-instance".to_string()),
        })
    }
}
fn synthesize_outputs(snippet_outputs: &Value) -> Value {
    let Some(obj) = snippet_outputs.as_object() else { return json!({}) };
    let mut out = serde_json::Map::new();
    for (name, schema) in obj {
        out.insert(name.clone(), synthesize_one(schema));
    }
    Value::Object(out)
}
fn synthesize_one(schema: &Value) -> Value {
    let ty = schema.get("type").and_then(Value::as_str).unwrap_or("string");
    if let Some(e) = schema.get("enum").and_then(Value::as_array) {
        if let Some(f) = e.first() {
            return f.clone();
        }
    }
    match ty {
        "string" => json!("fixture-value"),
        "integer" => json!(0),
        "number" => json!(0.0),
        "boolean" => json!(true),
        "array" => json!([]),
        "object" => json!({}),
        _ => Value::Null,
    }
}
struct NoopExecutor;
#[async_trait]
impl Executor for NoopExecutor {
    async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult::default())
    }
}
struct FixtureRegistry;
impl ExecutorRegistry for FixtureRegistry {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        match kind {
            "workflow" => Some(Arc::new(CapShortCircuit)),
            _ => Some(Arc::new(NoopExecutor)),
        }
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
    let registry = Arc::new(FixtureRegistry) as Arc<dyn ExecutorRegistry>;
    WorkflowRuntime::new(
        definitions,
        store,
        registry,
        guards,
        audit as Arc<dyn AuditSink>,
    )
    .with_evidence(evidence)
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
        "{definition_id} should walk to terminal 'done'; got state='{state}' status='{status}'. resp: {resp:#}"
    );
    assert_eq!(status, "completed", "{definition_id}: resp: {resp:#}");
    resp
}

#[tokio::test]
async fn meta_flow_author_capability_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let _ = walk_to_terminal(
        "meta/flow.author-capability",
        json!({ "goal": "Author cap.test.python-pytest", "namespace": "draft", "base_ref": "main" }),
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
        json!({ "goal": "Author flow.deploy-helm-chart", "namespace": "draft", "base_ref": "main" }),
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
        json!({ "target_definition_id": "cognitive/cap.implement.tdd-loop", "base_ref": "main" }),
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
        json!({ "target_definition_id": "cognitive/flow.add-feature", "base_ref": "main" }),
        &config,
    )
    .await;
}
