//! M4 acceptance test — walk each of the four shipping orchestrators
//! (`flow.add-feature`, `flow.bugfix-from-error-log`, `flow.safe-refactor`,
//! `flow.triage-issue`) through their full lifecycle to a terminal
//! state, against the vendored cognitive-architectures fixture.
//!
//! ## Test location note
//!
//! The plan places this test in `mcp-flowgate-core::tests::flow_orchestrators_e2e`,
//! but the orchestrator transitions invoke capabilities via
//! `kind: workflow`, which requires the [`WorkflowExecutor`] from this
//! crate. Making `mcp-flowgate-core` depend on `mcp-flowgate-executors`
//! even as a dev-dep would be a build cycle. We host the test here in
//! the executors crate; the acceptance condition (each orchestrator
//! reaches its terminal state) is identical.
//!
//! ## Fixture freshness
//!
//! The fixture lives at `crates/mcp-flowgate-core/tests/fixtures/cognitive-architectures/`
//! as a vendored copy of the sibling `/home/mc/working/cognitive-architectures` repo.
//! When cognitive-architectures ships a release, the fixture is updated
//! by hand (see CONTRIBUTING.md). Symlinks would be fragile across
//! checkouts; the vendored copy keeps the test self-contained.

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

/// Custom registry: WorkflowExecutor for `kind: workflow` once installed;
/// NoopExecutor for everything else. The OnceLock pattern bridges the
/// circular dependency between WorkflowRuntime (needs a registry) and
/// WorkflowExecutor (needs a runtime to spawn into).
struct M4Registry {
    workflow_executor: OnceLock<Arc<WorkflowExecutor>>,
}
impl M4Registry {
    fn new() -> Self {
        Self { workflow_executor: OnceLock::new() }
    }
    fn install(&self, exec: Arc<WorkflowExecutor>) {
        self.workflow_executor.set(exec).map_err(|_| ()).unwrap();
    }
}
impl ExecutorRegistry for M4Registry {
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
        // The cognitive-architectures v0.6 caps don't actually invoke
        // these via mcp/script/cli — every cap is a terminal-initialState
        // stub that auto-completes against initialContext. NoopExecutor is
        // here purely as a registry fallback so any cap that does grow a
        // real executor later doesn't blow up the e2e.
        Ok(ExecuteResult::default())
    }
}

/// Absolute path to the vendored cognitive-architectures fixture.
fn fixture_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/mcp-flowgate-executors → crates
    p.pop(); // crates → workspace root
    p.push("crates");
    p.push("mcp-flowgate-core");
    p.push("tests");
    p.push("fixtures");
    p.push("cognitive-architectures");
    p
}

/// Build a host gateway config that loads the fixture repo and write
/// it to a tempfile; return its path. Repo paths in `repos:` resolve
/// relative to the host file's directory, so we hand it an absolute
/// path to the fixture.
fn write_host_config(td: &TempDir) -> PathBuf {
    let body = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
        fixture_path().display()
    );
    let p = td.path().join("flowgate.yaml");
    std::fs::write(&p, body).unwrap();
    p
}

async fn build_runtime(config: &Value) -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(config));
    let store: Arc<dyn WorkflowStore> = Arc::new(InMemoryWorkflowStore::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));
    let registry = Arc::new(M4Registry::new());
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
    (runtime, audit)
}

/// Drive one orchestrator to its terminal state and assert. Returns
/// the final response so individual tests can spot-check projected
/// slots if useful.
async fn walk_to_terminal(
    definition_id: &str,
    input: Value,
    config: &Value,
) -> Value {
    let (runtime, _audit) = build_runtime(config).await;
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

    let status = resp
        .pointer("/result/status")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let state = resp
        .pointer("/workflow/state")
        .and_then(Value::as_str)
        .unwrap_or("?");
    assert_eq!(
        state, "done",
        "{definition_id} should walk to terminal 'done'; got state='{state}' status='{status}'. \
         resp: {resp:#}"
    );
    assert_eq!(
        status, "completed",
        "{definition_id} should report status=completed at terminal; got '{status}'. \
         resp: {resp:#}"
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
    // The terminal `opening_pr → done` transition writes pr_url; a
    // present-and-non-empty value proves the use.outputs projection
    // wired end-to-end through every preceding state.
    let pr_url = resp
        .pointer("/context/pr_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(!pr_url.is_empty(), "pr_url should be projected onto host context");
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
