//! Tests covering the 10 core invariants from INIT-1 §17.
//!
//! 1. Proxy exposure compiles to a null-op workflow transition.
//! 2. All transitions validate inputSchema before execution.
//! 3. Guards run before executor dispatch.
//! 4. Executors never decide workflow legality (errors become `failed`, not `executed`).
//! 5. Invalid transitions return current legal links.
//! 6. Every submit requires expectedVersion.
//! 7. Every successful transition increments workflow.version.
//! 8. Terminal states return no links.
//! 9. MCP-facing tools remain stable. (covered by binary smoke test)
//! 10. Downstream tools are only reachable through configured transitions.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use mcp_flowgate_core::audit::{AuditSink, MemoryAuditSink};
use mcp_flowgate_core::error::ExecutorError;
use mcp_flowgate_core::guards::DefaultGuardEvaluator;
use mcp_flowgate_core::model::{
    ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use mcp_flowgate_core::ports::{Executor, ExecutorRegistry};
use mcp_flowgate_core::proxy_workflow::{compile_proxy_workflow, DEFAULT_PROXY_WORKFLOW_ID};
use mcp_flowgate_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use mcp_flowgate_core::WorkflowRuntime;
use serde_json::{json, Value};

// ---- test harness -----------------------------------------------------------

#[derive(Default)]
struct RecordingExecutor {
    calls: Mutex<Vec<Value>>,
    output: Mutex<Value>,
    failures_left: AtomicUsize,
}

impl RecordingExecutor {
    fn new(output: Value) -> Self {
        Self {
            calls: Mutex::new(vec![]),
            output: Mutex::new(output),
            failures_left: AtomicUsize::new(0),
        }
    }
    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl Executor for RecordingExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        self.calls
            .lock()
            .unwrap()
            .push(request.executor_config.clone());
        if self.failures_left.load(Ordering::SeqCst) > 0 {
            self.failures_left.fetch_sub(1, Ordering::SeqCst);
            return Err(ExecutorError::Transient("recorded failure".into()));
        }
        Ok(ExecuteResult {
            output: self.output.lock().unwrap().clone(),
            evidence: vec![],
        })
    }
}

struct SingleExecRegistry {
    inner: Arc<RecordingExecutor>,
}

impl ExecutorRegistry for SingleExecRegistry {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        match kind {
            "noop" | "test" | "mcp" | "cli" | "human" => Some(self.inner.clone()),
            _ => None,
        }
    }
}

fn build_runtime(
    config: Value,
    exec_output: Value,
) -> (
    WorkflowRuntime,
    Arc<RecordingExecutor>,
    Arc<MemoryAuditSink>,
) {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executor = Arc::new(RecordingExecutor::new(exec_output));
    let executors = Arc::new(SingleExecRegistry {
        inner: executor.clone(),
    });
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    );
    (runtime, executor, audit)
}

fn proxy_config() -> Value {
    json!({
        "version": "1.0.0",
        "proxy": {
            "expose": [
                {
                    "name": "echo",
                    "title": "Echo",
                    "inputSchema": {
                        "type": "object",
                        "required": ["msg"],
                        "properties": { "msg": { "type": "string" } },
                        "additionalProperties": false
                    },
                    "executor": { "kind": "noop" }
                }
            ]
        }
    })
}

fn governed_config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "open",
                "states": {
                    "open": {
                        "transitions": {
                            "approve": {
                                "title": "Approve",
                                "target": "done",
                                "guards": [
                                    { "kind": "permission", "permission": "demo.approve" }
                                ],
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

fn principal_with(perms: &[&str]) -> Principal {
    Principal {
        subject: "tester".into(),
        roles: vec![],
        permissions: perms.iter().map(|s| s.to_string()).collect(),
    }
}

// ---- 1. Proxy exposure compiles to a null-op workflow transition -----------

#[test]
fn invariant_1_proxy_compiles_to_null_op_workflow() {
    let cfg = proxy_config();
    let workflow = compile_proxy_workflow(&cfg).expect("proxy workflow");
    assert_eq!(workflow.pointer("/initialState").unwrap(), "ready");
    let transition = workflow
        .pointer("/states/ready/transitions/echo")
        .expect("echo transition");
    assert_eq!(transition.get("target").unwrap(), "ready");
}

// ---- 2. All transitions validate inputSchema before execution --------------

#[tokio::test]
async fn invariant_2_input_schema_is_validated_before_executor() {
    let (runtime, exec, _) = build_runtime(proxy_config(), json!({}));

    let started = runtime
        .start(StartWorkflow {
            definition_id: DEFAULT_PROXY_WORKFLOW_ID.into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    // Bad input: msg is required.
    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: version,
            transition: "echo".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    assert_eq!(resp["result"]["status"], "rejected");
    assert_eq!(resp["error"]["code"], "INPUT_SCHEMA_VIOLATION");
    assert_eq!(exec.count(), 0, "executor must not run on schema violation");
}

// ---- 3. Guards run before executor dispatch --------------------------------

#[tokio::test]
async fn invariant_3_guards_run_before_executor() {
    let (runtime, exec, _) = build_runtime(governed_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    // No permission → guard rejects.
    let denied = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: version,
            transition: "approve".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    assert_eq!(denied["error"]["code"], "GUARD_REJECTED");
    assert_eq!(exec.count(), 0, "executor must not run when guard rejects");
}

// ---- 4. Executors never decide workflow legality ---------------------------

#[tokio::test]
async fn invariant_4_executor_failure_yields_failed_not_advanced_state() {
    let (runtime, exec, _) = build_runtime(governed_config(), json!({}));
    // configure the executor to fail enough times to exhaust default retries
    exec.failures_left.store(10, Ordering::SeqCst);

    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: version,
            transition: "approve".into(),
            arguments: json!({}),
            principal: principal_with(&["demo.approve"]),
        })
        .await
        .unwrap();

    assert_eq!(resp["result"]["status"], "failed");
    // state must remain `open`, not `done`
    assert_eq!(resp["workflow"]["state"], "open");
}

// ---- 5. Invalid transitions return current legal links ---------------------

#[tokio::test]
async fn invariant_5_invalid_transitions_return_current_links() {
    let (runtime, _, _) = build_runtime(governed_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "definitely_not_a_thing".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    assert_eq!(resp["error"]["code"], "INVALID_TRANSITION");
    let rels: Vec<&str> = resp["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["rel"].as_str().unwrap())
        .collect();
    assert!(
        rels.contains(&"approve"),
        "rejected response must list legal links"
    );
}

// ---- 6. Every submit requires expectedVersion ------------------------------

#[tokio::test]
async fn invariant_6_stale_expected_version_is_rejected() {
    let (runtime, _, _) = build_runtime(governed_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let actual_version = started["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: actual_version + 99,
            transition: "approve".into(),
            arguments: json!({}),
            principal: principal_with(&["demo.approve"]),
        })
        .await
        .unwrap();
    assert_eq!(resp["error"]["code"], "STALE_WORKFLOW_VERSION");
}

// ---- 7. Every successful transition increments workflow.version -----------

#[tokio::test]
async fn invariant_7_successful_transition_increments_version() {
    let (runtime, _, _) = build_runtime(governed_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version_before = started["workflow"]["version"].as_u64().unwrap();

    let after = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version_before,
            transition: "approve".into(),
            arguments: json!({}),
            principal: principal_with(&["demo.approve"]),
        })
        .await
        .unwrap();
    let version_after = after["workflow"]["version"].as_u64().unwrap();
    assert!(
        version_after > version_before,
        "version must increase on successful transition (was {version_before}, now {version_after})"
    );
}

// ---- 8. Terminal states return no links ------------------------------------

#[tokio::test]
async fn invariant_8_terminal_state_has_no_links() {
    let (runtime, _, _) = build_runtime(governed_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    let after = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "approve".into(),
            arguments: json!({}),
            principal: principal_with(&["demo.approve"]),
        })
        .await
        .unwrap();

    assert_eq!(after["workflow"]["state"], "done");
    assert_eq!(after["result"]["status"], "completed");
    let links = after["links"].as_array().unwrap();
    assert!(links.is_empty(), "terminal state must return no links");
}

// ---- 10. Downstream tools only reachable through configured transitions ----
//
// This invariant is structural: the runtime never invokes an executor outside
// of a transition or onEnter action. We assert it by checking that no executor
// calls happened when only `start` ran (no onEnter), and that an unknown
// transition does not call the executor.

#[tokio::test]
async fn invariant_10_unknown_transition_does_not_invoke_executor() {
    let (runtime, exec, _) = build_runtime(governed_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    assert_eq!(
        exec.count(),
        0,
        "start without onEnter must not call executor"
    );

    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();
    let _ = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "ghost".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    assert_eq!(exec.count(), 0, "ghost transition must not call executor");
}

// ---- bonus: audit emits a workflow.transitioned event on successful submit -

#[tokio::test]
async fn audit_records_workflow_transitioned() {
    let (runtime, _, audit) = build_runtime(governed_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "approve".into(),
            arguments: json!({}),
            principal: principal_with(&["demo.approve"]),
        })
        .await
        .unwrap();

    let types = audit.event_types();
    assert!(types.iter().any(|t| t == "workflow.started"));
    assert!(types.iter().any(|t| t == "transition.requested"));
    assert!(types.iter().any(|t| t == "guard.evaluated"));
    assert!(types.iter().any(|t| t == "executor.started"));
    assert!(types.iter().any(|t| t == "executor.succeeded"));
    assert!(types.iter().any(|t| t == "workflow.transitioned"));
    assert!(types.iter().any(|t| t == "workflow.completed"));
}

#[tokio::test]
async fn audit_records_transition_rejected_on_guard_rejection() {
    let (runtime, _, audit) = build_runtime(governed_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    // No permission → guard rejects → transition.rejected audited.
    runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "approve".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    let events = audit.snapshot();
    let rejection = events
        .iter()
        .find(|e| e.event_type == "transition.rejected")
        .expect("transition.rejected event must be emitted");
    assert_eq!(rejection.payload["code"], "GUARD_REJECTED");
    assert_eq!(rejection.payload["transition"], "approve");
}

#[tokio::test]
async fn audit_records_transition_rejected_on_stale_version() {
    let (runtime, _, audit) = build_runtime(governed_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version + 99,
            transition: "approve".into(),
            arguments: json!({}),
            principal: principal_with(&["demo.approve"]),
        })
        .await
        .unwrap();

    let codes: Vec<String> = audit
        .snapshot()
        .iter()
        .filter(|e| e.event_type == "transition.rejected")
        .filter_map(|e| {
            e.payload
                .get("code")
                .and_then(|c| c.as_str())
                .map(String::from)
        })
        .collect();
    assert!(codes.contains(&"STALE_WORKFLOW_VERSION".to_string()));
}

#[tokio::test]
async fn audit_records_fallback_selected_when_primary_exhausts() {
    // Build a config whose transition has a fallback executor; primary will
    // always fail; fallback should succeed and the audit must capture
    // `fallback.selected` exactly once for the second candidate.
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "open",
                "states": {
                    "open": {
                        "transitions": {
                            "go": {
                                "target": "done",
                                "executor": { "kind": "always_fail" },
                                "reliability": {
                                    "retry": { "maxAttempts": 1 },
                                    "fallback": {
                                        "executors": [{ "kind": "always_ok" }]
                                    }
                                }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    // Custom registry: two executors, one always fails, one always succeeds.
    struct AlwaysFail;
    #[async_trait]
    impl Executor for AlwaysFail {
        async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Err(ExecutorError::Transient("nope".into()))
        }
    }
    struct AlwaysOk;
    #[async_trait]
    impl Executor for AlwaysOk {
        async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Ok(ExecuteResult::default())
        }
    }
    struct PairRegistry {
        fail: Arc<dyn Executor>,
        ok: Arc<dyn Executor>,
    }
    impl ExecutorRegistry for PairRegistry {
        fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
            match kind {
                "always_fail" => Some(self.fail.clone()),
                "always_ok" => Some(self.ok.clone()),
                _ => None,
            }
        }
    }

    let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(PairRegistry {
        fail: Arc::new(AlwaysFail),
        ok: Arc::new(AlwaysOk),
    });
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    );

    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "go".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    // Result should be completed because fallback succeeded and target is terminal.
    assert_eq!(resp["result"]["status"], "completed");

    let fallbacks: Vec<_> = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type == "fallback.selected")
        .collect();
    assert_eq!(fallbacks.len(), 1, "exactly one fallback.selected event");
    assert_eq!(fallbacks[0].payload["candidate"], 1);
    assert_eq!(fallbacks[0].payload["kind"], "always_ok");
}

// ---- Actor gate: human-only transitions reject agent submits ---------------

fn human_only_config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "approval": {
                "initialState": "pending",
                "states": {
                    "pending": {
                        "transitions": {
                            "approve": {
                                "title": "Approve",
                                "target": "done",
                                "actor": "human",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

fn human_principal() -> Principal {
    Principal {
        subject: "alice".into(),
        roles: vec![Principal::HUMAN_ROLE.into()],
        permissions: vec![],
    }
}

#[tokio::test]
async fn actor_gate_rejects_agent_on_human_only_transition() {
    let (runtime, exec, audit) = build_runtime(human_only_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "approval".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    // Anonymous (agent-equivalent) principal must be rejected without
    // the executor ever running.
    let denied = runtime
        .submit(SubmitTransition {
            workflow_id: workflow_id.clone(),
            expected_version: version,
            transition: "approve".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    assert_eq!(denied["result"]["status"], "rejected");
    assert_eq!(denied["error"]["code"], "ACTOR_MISMATCH");
    assert_eq!(denied["workflow"]["state"], "pending");
    assert_eq!(exec.count(), 0, "executor must not run on actor mismatch");

    let rejections: Vec<_> = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type == "transition.rejected" && e.payload["code"] == "ACTOR_MISMATCH")
        .collect();
    assert_eq!(rejections.len(), 1, "one ACTOR_MISMATCH audit event");
}

#[tokio::test]
async fn actor_gate_admits_human_on_human_only_transition() {
    let (runtime, exec, _) = build_runtime(human_only_config(), json!({}));
    let started = runtime
        .start(StartWorkflow {
            definition_id: "approval".into(),
            input: json!({}),
            principal: human_principal(),
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "approve".into(),
            arguments: json!({}),
            principal: human_principal(),
        })
        .await
        .unwrap();
    assert_eq!(resp["result"]["status"], "completed");
    assert_eq!(resp["workflow"]["state"], "done");
    assert_eq!(exec.count(), 1);
}
