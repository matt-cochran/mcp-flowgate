//! M4 acceptance — walk each of cognitive-architectures v0.2's four
//! shipping orchestrators (`flow.add-feature`, `flow.bugfix-from-error-log`,
//! `flow.safe-refactor`, `flow.triage-issue`) through their full lifecycle
//! to a terminal state.
//!
//! ## Fixture executor (NOT WorkflowExecutor)
//!
//! In production, the orchestrator's `kind: workflow` transitions are
//! dispatched by `WorkflowExecutor`, which `runtime.start`s the cap
//! sub-workflow and polls until completion. Cognitive caps are
//! agent-driven (`kind: noop + actor: agent`); they'd block forever
//! without an LLM driver submitting per-cap arguments.
//!
//! Tests can't supply an LLM. So we register a fixture executor for
//! `kind: workflow` that short-circuits: receives the orchestrator's
//! `executor_config` (including `_snippetOutputs` embedded by the
//! config-resolve pass), synthesizes valid outputs per the snippet
//! schema, returns them directly. The orchestrator's projection layer
//! merges them as if the cap had really run.
//!
//! This proves the orchestrator state machine + use-binding projection
//! work end-to-end. Cap-internal behavior is tested by each cap's own
//! integration tests (operator-owned, not part of M4).
//!
//! ## Test location
//!
//! Lives in mcp-flowgate-executors (not -core) to avoid a build cycle:
//! the test uses helpers from this crate's tests/ folder pattern and
//! must NOT add a core→executors dev-dep.

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

/// Fixture executor for `kind: workflow` — short-circuits cap invocation
/// by synthesizing outputs per the embedded `_snippetOutputs` schema.
/// Returns a result keyed by capability output name (matching what
/// `WorkflowExecutor` would have produced post-projection, so the
/// orchestrator's synthesized transition output mapping projects to
/// host slots correctly).
struct CapShortCircuit;

#[async_trait]
impl Executor for CapShortCircuit {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        // The expand_use_bindings pass embeds the target capability's
        // snippet.outputs as `_snippetOutputs` on the executor config.
        // We use that schema to synthesize a valid example value per
        // declared output.
        let snippet_outputs = request
            .executor_config
            .get("_snippetOutputs")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let outputs = synthesize_outputs(&snippet_outputs);
        Ok(ExecuteResult {
            output: outputs,
            evidence: vec![],
            child_workflow_id: Some("fixture-cap-instance".to_string()),
        })
    }
}

/// Walk the snippet outputs schema; emit a valid example value for
/// each declared output. Keyed by cap output name (NOT host path) —
/// the synthesized transition output mapping in the orchestrator
/// projects from `$.output.<cap_output_name>` to host slots.
fn synthesize_outputs(snippet_outputs: &Value) -> Value {
    let Some(obj) = snippet_outputs.as_object() else {
        return json!({});
    };
    let mut out = serde_json::Map::new();
    for (name, schema) in obj {
        out.insert(name.clone(), synthesize_one(schema));
    }
    Value::Object(out)
}

fn synthesize_one(schema: &Value) -> Value {
    let ty = schema.get("type").and_then(Value::as_str).unwrap_or("string");
    // Enum constraint wins — use first allowed value.
    if let Some(enum_vals) = schema.get("enum").and_then(Value::as_array) {
        if let Some(first) = enum_vals.first() {
            return first.clone();
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

/// Registry: `kind: workflow` short-circuits via CapShortCircuit;
/// everything else returns NoopExecutor.
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
    p.push("cognitive-architectures");
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
async fn flow_add_feature_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let resp = walk_to_terminal(
        "cognitive/flow.add-feature",
        json!({
            "feature_brief": "Add a /status endpoint",
            "base_ref":      "main",
            "lexicon":       {}
        }),
        &config,
    )
    .await;
    // pr_url projected onto host context proves the full chain wired up
    // through every preceding cap invocation.
    let pr_url = resp
        .pointer("/context/pr_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(!pr_url.is_empty(), "pr_url should be projected; got {resp:#}");
}

#[tokio::test]
async fn flow_bugfix_from_error_log_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let _ = walk_to_terminal(
        "cognitive/flow.bugfix-from-error-log",
        json!({
            "error_log": "panicked at 'index out of bounds'",
            "base_ref":  "main"
        }),
        &config,
    )
    .await;
}

#[tokio::test]
async fn flow_safe_refactor_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let _ = walk_to_terminal(
        "cognitive/flow.safe-refactor",
        json!({
            "scope_description": { "paths": ["src/foo"] },
            "base_ref":           "main"
        }),
        &config,
    )
    .await;
}

#[tokio::test]
async fn flow_triage_issue_walks_to_terminal() {
    let td = TempDir::new().unwrap();
    let host_path = write_host_config(&td);
    let (config, _diags) = load_resolved_with_repos(&host_path).expect("config loads");
    let _ = walk_to_terminal(
        "cognitive/flow.triage-issue",
        json!({ "issue": { "title": "Login button broken", "body": "..." } }),
        &config,
    )
    .await;
}
