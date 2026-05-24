//! Tests for deterministic chaining and phase guidance.
//!
//! Deterministic transitions (`actor: "deterministic"`) auto-execute without
//! LLM involvement. The chain engine runs them in sequence until it hits a
//! decision point (non-deterministic transition), terminal state, depth limit,
//! or failure.
//!
//! Phase guidance (`goal`/`guidance` on states) surfaces contextual
//! instructions in every workflow response.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use mcp_flowgate_core::audit::{AuditSink, MemoryAuditSink};
use mcp_flowgate_core::error::ExecutorError;
use mcp_flowgate_core::guards::DefaultGuardEvaluator;
use mcp_flowgate_core::model::{
    ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use mcp_flowgate_core::ports::{Executor, ExecutorRegistry};
use mcp_flowgate_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use mcp_flowgate_core::WorkflowRuntime;
use serde_json::{json, Value};

// ---- test harness -----------------------------------------------------------

struct FixedExecutor {
    output: Value,
    call_count: AtomicUsize,
}

impl FixedExecutor {
    fn new(output: Value) -> Self {
        Self {
            output,
            call_count: AtomicUsize::new(0),
        }
    }
    fn count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Executor for FixedExecutor {
    async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        Ok(ExecuteResult {
            output: self.output.clone(),
            evidence: vec![],
            child_workflow_id: None,
        })
    }
}

struct FailAfterN {
    succeed_count: AtomicUsize,
    max_successes: usize,
}

impl FailAfterN {
    fn new(max_successes: usize) -> Self {
        Self {
            succeed_count: AtomicUsize::new(0),
            max_successes,
        }
    }
}

#[async_trait]
impl Executor for FailAfterN {
    async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let n = self.succeed_count.fetch_add(1, Ordering::SeqCst);
        if n < self.max_successes {
            Ok(ExecuteResult {
                output: json!({}),
                evidence: vec![],
                child_workflow_id: None,
            })
        } else {
            Err(ExecutorError::Permanent("simulated failure".into()))
        }
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

fn build_runtime_with_executor(
    config: Value,
    executor: Arc<dyn Executor>,
) -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(SingleExecRegistry { inner: executor });
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    );
    (runtime, audit)
}

fn build_runtime(config: Value) -> (WorkflowRuntime, Arc<FixedExecutor>, Arc<MemoryAuditSink>) {
    let executor = Arc::new(FixedExecutor::new(json!({})));
    let (runtime, audit) =
        build_runtime_with_executor(config, executor.clone() as Arc<dyn Executor>);
    (runtime, executor, audit)
}

// ---- configs ----------------------------------------------------------------

/// A → B → C where A→B is deterministic and B→C is agent.
/// Chain should auto-execute A→B, then stop at B waiting for agent.
fn linear_chain_stops_at_agent() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "goal": "Initialize the pipeline",
                        "guidance": "System will auto-validate inputs",
                        "transitions": {
                            "validate": {
                                "title": "Validate",
                                "target": "b",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "b": {
                        "goal": "Review validation results",
                        "guidance": "Check the context for validation output before proceeding",
                        "transitions": {
                            "deploy": {
                                "title": "Deploy",
                                "target": "c",
                                "actor": "agent",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "c": { "terminal": true }
                }
            }
        }
    })
}

/// A → B → C → D all deterministic, D is terminal.
fn fully_deterministic_to_terminal() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "step1": {
                                "target": "b",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "b": {
                        "transitions": {
                            "step2": {
                                "target": "c",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "c": {
                        "transitions": {
                            "step3": {
                                "target": "d",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "d": { "terminal": true }
                }
            }
        }
    })
}

/// Mixed state: A has both deterministic and agent transitions.
/// Chain should NOT execute — stops at mixed states.
fn mixed_state_config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "auto_check": {
                                "target": "b",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            },
                            "manual_override": {
                                "target": "c",
                                "actor": "agent",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "b": { "terminal": true },
                    "c": { "terminal": true }
                }
            }
        }
    })
}

/// Chain with maxChainDepth: 2 but 5 deterministic steps.
fn depth_limited_config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "maxChainDepth": 2,
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
                    "d": {
                        "transitions": {
                            "s4": { "target": "e", "actor": "deterministic", "executor": { "kind": "noop" } }
                        }
                    },
                    "e": { "terminal": true }
                }
            }
        }
    })
}

// =============================================================================
// Tests
// =============================================================================

// ---- 1. Linear chain stops at agent decision point --------------------------

#[tokio::test]
async fn chain_auto_executes_deterministic_and_stops_at_agent() {
    let (runtime, exec, _) = build_runtime(linear_chain_stops_at_agent());
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    // Should have chained from a→b, stopping at b (agent transition)
    assert_eq!(resp["workflow"]["state"], "b");
    assert_eq!(resp["result"]["status"], "started");

    let chain = resp["chain"].as_array().expect("chain array");
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0]["fromState"], "a");
    assert_eq!(chain[0]["transition"], "validate");
    assert_eq!(chain[0]["toState"], "b");

    // Executor ran once for the deterministic step
    assert_eq!(exec.count(), 1);
}

// ---- 2. Fully deterministic chain reaches terminal --------------------------

#[tokio::test]
async fn fully_deterministic_chain_reaches_terminal() {
    let (runtime, exec, _) = build_runtime(fully_deterministic_to_terminal());
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    assert_eq!(resp["workflow"]["state"], "d");
    assert_eq!(resp["result"]["status"], "completed");

    let chain = resp["chain"].as_array().expect("chain array");
    assert_eq!(chain.len(), 3);
    assert_eq!(chain[0]["fromState"], "a");
    assert_eq!(chain[0]["toState"], "b");
    assert_eq!(chain[1]["fromState"], "b");
    assert_eq!(chain[1]["toState"], "c");
    assert_eq!(chain[2]["fromState"], "c");
    assert_eq!(chain[2]["toState"], "d");

    assert_eq!(exec.count(), 3);
    assert!(resp["links"].as_array().unwrap().is_empty());
}

// ---- 3. Mixed state stops the chain (no auto-execute) -----------------------

#[tokio::test]
async fn mixed_state_stops_chain() {
    let (runtime, exec, _) = build_runtime(mixed_state_config());
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    // Chain should NOT execute; stays at initial state "a"
    assert_eq!(resp["workflow"]["state"], "a");
    assert!(resp.get("chain").is_none() || resp["chain"].as_array().unwrap().is_empty());
    assert_eq!(exec.count(), 0);

    // Only the agent transition should appear in links (deterministic hidden)
    let rels: Vec<&str> = resp["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["rel"].as_str().unwrap())
        .collect();
    assert!(rels.contains(&"manual_override"));
    assert!(
        !rels.contains(&"auto_check"),
        "deterministic transitions must be hidden from links"
    );
}

// ---- 4. Deterministic transitions are hidden from links ---------------------

#[tokio::test]
async fn deterministic_transitions_hidden_from_links() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "auto_lint": {
                                "target": "b",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            },
                            "manual_review": {
                                "target": "b",
                                "actor": "agent",
                                "executor": { "kind": "noop" }
                            },
                            "human_approve": {
                                "target": "c",
                                "actor": "human",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "b": { "terminal": true },
                    "c": { "terminal": true }
                }
            }
        }
    });

    let (runtime, _, _) = build_runtime(cfg);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    let rels: Vec<&str> = resp["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["rel"].as_str().unwrap())
        .collect();
    assert!(rels.contains(&"manual_review"));
    assert!(rels.contains(&"human_approve"));
    assert!(!rels.contains(&"auto_lint"));
}

// ---- 5. Depth limit stops chain early ---------------------------------------

#[tokio::test]
async fn depth_limit_stops_chain_early() {
    let (runtime, exec, _) = build_runtime(depth_limited_config());
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    // maxChainDepth=2, so chain should stop after 2 steps (at state "c")
    assert_eq!(resp["workflow"]["state"], "c");
    let chain = resp["chain"].as_array().expect("chain array");
    assert_eq!(chain.len(), 2);
    assert_eq!(exec.count(), 2);
}

// ---- 6. Chain failure returns partial steps and recovery link ---------------

#[tokio::test]
async fn chain_failure_returns_partial_and_recovery_link() {
    let executor = Arc::new(FailAfterN::new(1)); // succeed once, fail on second
    let (runtime, audit) = build_runtime_with_executor(
        fully_deterministic_to_terminal(),
        executor as Arc<dyn Executor>,
    );

    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    // First step (a→b) succeeds, second step (b→c) fails
    assert_eq!(resp["workflow"]["state"], "b");
    assert_eq!(resp["result"]["status"], "failed");
    assert_eq!(resp["error"]["code"], "CHAIN_FAILED");

    let chain = resp["chain"].as_array().expect("chain array");
    assert_eq!(chain.len(), 1, "only the successful step recorded");
    assert_eq!(chain[0]["fromState"], "a");
    assert_eq!(chain[0]["toState"], "b");

    // Recovery link should include the failed deterministic transition
    let rels: Vec<&str> = resp["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["rel"].as_str().unwrap())
        .collect();
    assert!(
        rels.contains(&"step2"),
        "failed deterministic transition should appear in links for recovery"
    );

    // Audit should include chain.failed
    let types = audit.event_types();
    assert!(types.iter().any(|t| t == "chain.failed"));
}

// ---- 7. Chain after submit auto-executes from new state ---------------------

#[tokio::test]
async fn chain_runs_after_submit() {
    // a→b is agent, b→c is deterministic, c is terminal
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "begin": {
                                "target": "b",
                                "actor": "agent",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "b": {
                        "transitions": {
                            "finalize": {
                                "target": "c",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "c": { "terminal": true }
                }
            }
        }
    });

    let (runtime, exec, _) = build_runtime(cfg);
    let started = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    // Start should stay at "a" (agent transition, no chain)
    assert_eq!(started["workflow"]["state"], "a");

    let wf_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: wf_id,
            expected_version: version,
            transition: "begin".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
        })
        .await
        .unwrap();

    // Submit should execute begin (a→b) then chain finalize (b→c)
    assert_eq!(resp["workflow"]["state"], "c");
    assert_eq!(resp["result"]["status"], "completed");

    let chain = resp["chain"].as_array().expect("chain array");
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0]["fromState"], "b");
    assert_eq!(chain[0]["transition"], "finalize");
    assert_eq!(chain[0]["toState"], "c");

    // 2 executor calls: 1 for submit's "begin" + 1 for chain's "finalize"
    assert_eq!(exec.count(), 2);
}

// ---- 8. Phase guidance in responses -----------------------------------------

#[tokio::test]
async fn phase_guidance_appears_in_response() {
    let (runtime, _, _) = build_runtime(linear_chain_stops_at_agent());
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    // After chain, we're at state "b" which has goal and guidance
    assert_eq!(resp["workflow"]["state"], "b");
    assert_eq!(resp["guidance"]["goal"], "Review validation results");
    assert_eq!(
        resp["guidance"]["instructions"],
        "Check the context for validation output before proceeding"
    );
}

// ---- 9. Phase guidance absent when state has none ---------------------------

#[tokio::test]
async fn phase_guidance_absent_when_not_configured() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "plain": {
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
    });

    let (runtime, _, _) = build_runtime(cfg);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "plain".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    assert!(
        resp.get("guidance").is_none(),
        "guidance should not appear when state has no goal/guidance"
    );
}

// ---- 10. Chain steps record correct versions --------------------------------

#[tokio::test]
async fn chain_steps_have_incrementing_versions() {
    let (runtime, _, _) = build_runtime(fully_deterministic_to_terminal());
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    let chain = resp["chain"].as_array().unwrap();
    assert_eq!(chain.len(), 3);

    // Each step's version should be strictly increasing
    let versions: Vec<u64> = chain
        .iter()
        .map(|s| s["version"].as_u64().unwrap())
        .collect();
    for i in 1..versions.len() {
        assert!(
            versions[i] > versions[i - 1],
            "versions must increase: {:?}",
            versions
        );
    }
}

// ---- 11. Audit trail for deterministic chain --------------------------------

#[tokio::test]
async fn chain_emits_audit_events() {
    let (runtime, _, audit) = build_runtime(fully_deterministic_to_terminal());
    runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    let types = audit.event_types();

    // chain.step for each deterministic step
    let chain_steps: Vec<_> = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type == "chain.step")
        .collect();
    assert_eq!(chain_steps.len(), 3);

    // chain.completed at the end
    assert!(types.iter().any(|t| t == "chain.completed"));

    // workflow.transitioned for each step (with deterministic: true)
    let transitions: Vec<_> = audit
        .snapshot()
        .into_iter()
        .filter(|e| {
            e.event_type == "workflow.transitioned"
                && e.payload.get("deterministic").and_then(Value::as_bool) == Some(true)
        })
        .collect();
    assert_eq!(transitions.len(), 3);
}

// ---- 12. No chain when initial state is terminal ----------------------------

#[tokio::test]
async fn no_chain_when_already_terminal() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "instant": {
                "initialState": "done",
                "states": {
                    "done": { "terminal": true }
                }
            }
        }
    });

    let (runtime, exec, _) = build_runtime(cfg);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "instant".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    assert_eq!(resp["workflow"]["state"], "done");
    assert_eq!(resp["result"]["status"], "completed");
    assert!(resp.get("chain").is_none() || resp["chain"].as_array().unwrap().is_empty());
    assert_eq!(exec.count(), 0);
}

// ---- 13. No chain when no transitions exist ---------------------------------

#[tokio::test]
async fn no_chain_when_state_has_no_transitions() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "stuck": {
                "initialState": "a",
                "states": {
                    "a": {}
                }
            }
        }
    });

    let (runtime, exec, _) = build_runtime(cfg);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "stuck".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    assert_eq!(resp["workflow"]["state"], "a");
    assert!(resp.get("chain").is_none() || resp["chain"].as_array().unwrap().is_empty());
    assert_eq!(exec.count(), 0);
}

// ---- 14. Explain includes actor and deterministic flag ----------------------

#[tokio::test]
async fn explain_shows_actor_and_deterministic_flag() {
    let (runtime, _, _) = build_runtime(mixed_state_config());
    let started = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    let wf_id = started["workflow"]["id"].as_str().unwrap();

    let explain_det = runtime.explain(wf_id, "auto_check").await.unwrap();
    assert_eq!(explain_det["actor"], "deterministic");
    assert_eq!(explain_det["deterministic"], true);

    let explain_agent = runtime.explain(wf_id, "manual_override").await.unwrap();
    assert_eq!(explain_agent["actor"], "agent");
    assert_eq!(explain_agent["deterministic"], false);
}

// ---- 15. Deterministic transition can still be submitted manually -----------
// (No actor gate — FMECA finding: gate creates stuck workflows)

#[tokio::test]
async fn deterministic_transition_submittable_for_recovery() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "auto_step": {
                                "target": "b",
                                "actor": "deterministic",
                                "executor": { "kind": "noop" }
                            },
                            "manual_alt": {
                                "target": "b",
                                "actor": "agent",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "b": { "terminal": true }
                }
            }
        }
    });

    let (runtime, _, _) = build_runtime(cfg);
    let started = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();
    let wf_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    // Manually submitting a deterministic transition should work
    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: wf_id,
            expected_version: version,
            transition: "auto_step".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
        })
        .await
        .unwrap();

    assert_eq!(resp["workflow"]["state"], "b");
    assert_eq!(resp["result"]["status"], "completed");
}

// ---- 16. Chain without executor (pure routing) ------------------------------

#[tokio::test]
async fn chain_works_without_executor() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "pipeline": {
                "initialState": "a",
                "states": {
                    "a": {
                        "transitions": {
                            "route": {
                                "target": "b",
                                "actor": "deterministic"
                            }
                        }
                    },
                    "b": {
                        "transitions": {
                            "next": {
                                "target": "c",
                                "actor": "agent",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "c": { "terminal": true }
                }
            }
        }
    });

    let (runtime, exec, _) = build_runtime(cfg);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "pipeline".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    assert_eq!(resp["workflow"]["state"], "b");
    let chain = resp["chain"].as_array().unwrap();
    assert_eq!(chain.len(), 1);
    assert_eq!(exec.count(), 0, "no executor should run for pure routing");
}
