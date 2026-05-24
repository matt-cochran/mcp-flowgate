//! Guarantee tests for transition record emission.
//!
//! Every applied workflow transition must emit exactly one `workflow.transition`
//! audit event (a "transition record"), and it must be emitted *record-first*:
//! the record is written before the authoritative state snapshot is committed.
//! If the record write fails, the transition fails fast and the snapshot is NOT
//! committed.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mcp_flowgate_core::audit::{AuditEvent, AuditSink, MemoryAuditSink};
use mcp_flowgate_core::guards::DefaultGuardEvaluator;
use mcp_flowgate_core::model::{GetWorkflow, Principal, StartWorkflow, SubmitTransition};
use mcp_flowgate_core::ports::{Executor, ExecutorRegistry, WorkflowStore};
use mcp_flowgate_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use mcp_flowgate_core::WorkflowRuntime;
use serde_json::{json, Value};

// ---- test harness -----------------------------------------------------------

/// Executor that does nothing useful and never fails. Deterministic chains
/// reference `{ "kind": "noop" }`; the registry hands this back for any kind.
struct NoopExecutor;

#[async_trait]
impl Executor for NoopExecutor {
    async fn execute(
        &self,
        _: mcp_flowgate_core::model::ExecuteRequest,
    ) -> Result<mcp_flowgate_core::model::ExecuteResult, mcp_flowgate_core::error::ExecutorError>
    {
        Ok(mcp_flowgate_core::model::ExecuteResult::default())
    }
}

struct SingleExecRegistry {
    inner: Arc<dyn Executor>,
}

impl ExecutorRegistry for SingleExecRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(self.inner.clone())
    }
}

/// An `AuditSink` that fails all `workflow.transition` audit events and
/// succeeds for all other event types.
struct FailingAuditSink {
    recorded: Mutex<Vec<AuditEvent>>,
}

impl FailingAuditSink {
    fn fail_all_transition_records() -> Self {
        Self {
            recorded: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl AuditSink for FailingAuditSink {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()> {
        let is_transition_record = event.event_type == "workflow.transition";
        if is_transition_record {
            anyhow::bail!("simulated audit sink failure");
        }
        self.recorded.lock().unwrap().push(event);
        Ok(())
    }

    async fn list_events(&self) -> Option<Vec<AuditEvent>> {
        Some(self.recorded.lock().unwrap().clone())
    }
}

fn build_runtime(
    config: Value,
    audit: Arc<dyn AuditSink>,
) -> (WorkflowRuntime, Arc<InMemoryWorkflowStore>) {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(SingleExecRegistry {
        inner: Arc::new(NoopExecutor),
    });
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let runtime = WorkflowRuntime::new(definitions, store.clone(), executors, guards, audit);
    (runtime, store)
}

// ---- configs ----------------------------------------------------------------

/// a -> b -> c -> d, all deterministic, d terminal. One `start` applies three
/// transitions via the deterministic chain.
fn three_step_chain() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "s1": { "target": "b", "actor": "deterministic", "executor": { "kind": "noop" } }
                        }
                    },
                    "b": {
                        "transitions": {
                            "s2": { "target": "c", "actor": "deterministic", "executor": { "kind": "noop" } }
                        }
                    },
                    "c": {
                        "transitions": {
                            "s3": { "target": "d", "actor": "deterministic", "executor": { "kind": "noop" } }
                        }
                    },
                    "d": { "terminal": true }
                }
            }
        }
    })
}

/// a -> b, single agent transition. Used to drive a `submit` and observe what
/// happens when the transition record write fails.
fn single_agent_transition() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "go": { "target": "b", "actor": "agent", "executor": { "kind": "noop" } }
                        }
                    },
                    "b": { "terminal": true }
                }
            }
        }
    })
}

// =============================================================================
// Tests
// =============================================================================

/// A workflow that applies N transitions (here N=3, via a deterministic chain
/// out of one `start`) must emit exactly N `workflow.transition` records, whose
/// `seq` values are 1..=N.
#[tokio::test]
async fn record_emitted_per_applied_transition() {
    let audit = Arc::new(MemoryAuditSink::new());
    let (runtime, _store) = build_runtime(three_step_chain(), audit.clone());

    runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .expect("start should succeed");

    let records: Vec<AuditEvent> = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type == "workflow.transition")
        .collect();

    assert_eq!(
        records.len(),
        3,
        "exactly one workflow.transition record per applied transition"
    );

    let seqs: Vec<u64> = records
        .iter()
        .map(|e| {
            e.payload
                .get("seq")
                .and_then(Value::as_u64)
                .expect("record must carry a numeric seq")
        })
        .collect();
    assert_eq!(seqs, vec![1, 2, 3], "seq must run 1..=N");
}

/// If the transition record write fails, the `submit` must fail with an error
/// identifiable as `RECORD_WRITE_FAILED`.
#[tokio::test]
async fn record_write_failure_aborts_transition() {
    let audit = Arc::new(FailingAuditSink::fail_all_transition_records());
    let (runtime, _store) = build_runtime(single_agent_transition(), audit.clone());

    let started = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .expect("start should succeed (no transition applied at start)");
    let wf_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    let result = runtime
        .submit(SubmitTransition {
            workflow_id: wf_id.clone(),
            expected_version: version,
            transition: "go".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
        })
        .await;

    let err = result.expect_err("submit must fail when the transition record write fails");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("RECORD_WRITE_FAILED"),
        "error must be identifiable as RECORD_WRITE_FAILED, got: {msg}"
    );
}

/// After a record-write failure aborts a `submit`, the persisted workflow
/// version must be unchanged — proof the snapshot did not commit.
#[tokio::test]
async fn version_unchanged_when_record_write_fails() {
    let audit = Arc::new(FailingAuditSink::fail_all_transition_records());
    let (runtime, store) = build_runtime(single_agent_transition(), audit.clone());

    let started = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .expect("start should succeed");
    let wf_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version_before = started["workflow"]["version"].as_u64().unwrap();

    let result = runtime
        .submit(SubmitTransition {
            workflow_id: wf_id.clone(),
            expected_version: version_before,
            transition: "go".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
        })
        .await;
    assert!(result.is_err(), "submit must fail");

    let loaded = store.load(&wf_id).await.expect("workflow must still load");
    assert_eq!(
        loaded.version, version_before,
        "version must be unchanged: the snapshot must not have committed"
    );
    assert_eq!(
        loaded.state, "a",
        "state must be unchanged: the snapshot must not have committed"
    );
}

/// A workflow that fires a lazy timeout (via `get`) must emit a
/// `workflow.transition` record with `actor` = `"system"` and
/// `transition` = `"onTimeout"`. The record must appear before the
/// `workflow.timed_out` event (record-first ordering).
#[tokio::test]
async fn timeout_emits_workflow_transition_record_with_system_actor() {
    let config = json!({
        "version": "1.0.0",
        "workflows": {
            "short_lived": {
                "initialState": "open",
                "timeoutMs": 1,
                "onTimeout": { "target": "timed_out_state" },
                "states": {
                    "open": {
                        "transitions": {
                            "approve": { "target": "done", "executor": { "kind": "noop" } }
                        }
                    },
                    "timed_out_state": { "terminal": true },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let audit = Arc::new(MemoryAuditSink::new());
    let (runtime, _store) = build_runtime(config, audit.clone());

    let started = runtime
        .start(StartWorkflow {
            definition_id: "short_lived".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .expect("start should succeed");
    let wf_id = started["workflow"]["id"].as_str().unwrap().to_string();

    // Sleep past the 1ms timeout deadline.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    // Trigger the lazy timeout via a `get`.
    runtime
        .get(GetWorkflow {
            workflow_id: wf_id.clone(),
            principal: Principal::anonymous(),
        })
        .await
        .expect("get should succeed");

    let snapshot = audit.snapshot();
    let event_types: Vec<&str> = snapshot.iter().map(|e| e.event_type.as_str()).collect();

    // A `workflow.transition` record must be present.
    let transition_records: Vec<&AuditEvent> = snapshot
        .iter()
        .filter(|e| e.event_type == "workflow.transition")
        .collect();
    assert!(
        !transition_records.is_empty(),
        "timeout must emit a workflow.transition record; got event types: {event_types:?}"
    );

    // The transition record must name `actor` = `"system"` in its payload.
    let record = transition_records[0];
    assert_eq!(
        record.payload.get("actor").and_then(Value::as_str),
        Some("system"),
        "timeout transition record must carry actor = \"system\""
    );

    // The transition name must be `"onTimeout"`.
    assert_eq!(
        record.payload.get("transition").and_then(Value::as_str),
        Some("onTimeout"),
        "timeout transition record must carry transition = \"onTimeout\""
    );

    // Record-first: the `workflow.transition` record must appear before the
    // `workflow.timed_out` event in the audit stream.
    let tr_pos = snapshot
        .iter()
        .position(|e| e.event_type == "workflow.transition")
        .unwrap();
    let timed_out_pos = snapshot
        .iter()
        .position(|e| e.event_type == "workflow.timed_out")
        .unwrap();
    assert!(
        tr_pos < timed_out_pos,
        "workflow.transition record (pos {tr_pos}) must precede workflow.timed_out (pos {timed_out_pos})"
    );
}
