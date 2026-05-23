//! Tests for `{{ }}` template interpolation in `goal` / `guidance` strings.
//!
//! SPEC v2 §5.2: placeholders of the form `{{ $.path }}` are resolved against
//! the live workflow instance at render time. Unresolved paths render as a
//! marked stub. Interpolation is single-pass and non-recursive.

use mcp_flowgate_core::audit::{AuditSink, MemoryAuditSink};
use mcp_flowgate_core::guards::DefaultGuardEvaluator;
use mcp_flowgate_core::model::{Principal, StartWorkflow};
use mcp_flowgate_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use mcp_flowgate_core::WorkflowRuntime;
use serde_json::json;
use std::sync::Arc;

// ── test harness ─────────────────────────────────────────────────────────────

/// Minimal registry that returns `None` for every executor kind — sufficient
/// for tests that never reach an executor step.
struct NoopRegistry;
impl mcp_flowgate_core::ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn mcp_flowgate_core::Executor>> {
        None
    }
}

fn build_runtime(config: serde_json::Value) -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(NoopRegistry);
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

// ── test 1 ────────────────────────────────────────────────────────────────────
// A resolved placeholder is replaced with the context value.

#[tokio::test]
async fn guidance_string_interpolates_context() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "check",
                "initialContext": { "someKey": "hello-world" },
                "states": {
                    "check": {
                        "goal": "Current key is {{ $.context.someKey }}",
                        "guidance": "Value from context: {{ $.context.someKey }}",
                        "transitions": {
                            "proceed": {
                                "target": "done",
                                "actor": "agent"
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    let (runtime, _) = build_runtime(cfg);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    assert_eq!(resp["workflow"]["state"], "check");

    let goal = resp["guidance"]["goal"].as_str().expect("goal present");
    assert!(
        !goal.contains("{{"),
        "goal must not contain raw placeholder, got: {goal}"
    );
    assert!(
        goal.contains("hello-world"),
        "goal must contain interpolated value, got: {goal}"
    );

    let instructions = resp["guidance"]["instructions"]
        .as_str()
        .expect("instructions present");
    assert!(
        !instructions.contains("{{"),
        "instructions must not contain raw placeholder, got: {instructions}"
    );
    assert!(
        instructions.contains("hello-world"),
        "instructions must contain interpolated value, got: {instructions}"
    );
}

// ── test 2 ────────────────────────────────────────────────────────────────────
// An unresolved placeholder renders as a marked stub; response is still produced.

#[tokio::test]
async fn unresolved_placeholder_renders_stub_not_error() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "check",
                "states": {
                    "check": {
                        "guidance": "Count is {{ $.context.missingKey }} items",
                        "transitions": {
                            "proceed": {
                                "target": "done",
                                "actor": "agent"
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    let (runtime, _) = build_runtime(cfg);
    // Must not return an error even though the context key is absent.
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .expect("response must be produced even with unresolved placeholder");

    assert_eq!(resp["workflow"]["state"], "check");

    let instructions = resp["guidance"]["instructions"]
        .as_str()
        .expect("guidance instructions present");

    // Stub format: (lastSegment: unset)
    assert!(
        instructions.contains("(missingKey: unset)"),
        "unresolved placeholder should render as stub, got: {instructions}"
    );
    // The raw placeholder must not appear verbatim.
    assert!(
        !instructions.contains("{{"),
        "raw placeholder must not appear, got: {instructions}"
    );
}

// ── test 3 ────────────────────────────────────────────────────────────────────
// A context value that itself looks like a template is NOT re-expanded.

#[tokio::test]
async fn template_value_not_re_expanded() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "check",
                "initialContext": {
                    "x": "42",
                    "tricky": "{{ $.context.x }}"
                },
                "states": {
                    "check": {
                        "guidance": "Tricky value is: {{ $.context.tricky }}",
                        "transitions": {
                            "proceed": {
                                "target": "done",
                                "actor": "agent"
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    let (runtime, _) = build_runtime(cfg);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    let instructions = resp["guidance"]["instructions"]
        .as_str()
        .expect("guidance instructions present");

    // The substituted value "{{ $.context.x }}" must appear literally —
    // it must NOT be recursively expanded to "42".
    assert!(
        instructions.contains("{{ $.context.x }}"),
        "substituted template-like value must appear verbatim, got: {instructions}"
    );
    assert!(
        !instructions.contains("42"),
        "templated value must not be recursively re-expanded: {instructions}"
    );
    // The outer placeholder for 'tricky' must be gone.
    // After substitution the string contains the literal "{{ $.context.x }}"
    // which looks like a placeholder — but that's the VALUE, not a residual
    // unrendered placeholder. We verify the instructions don't contain
    // the outer "$.context.tricky" raw token.
    assert!(
        !instructions.contains("$.context.tricky"),
        "outer placeholder must be consumed, got: {instructions}"
    );
}

// ── test 4 ────────────────────────────────────────────────────────────────────
// SPEC v2 §5.5: a state referencing a `skills:` entry surfaces a
// `guidance.refs` entry `{verb, subject}`; `gateway.describe(subject)` returns
// the body.

#[tokio::test]
async fn response_surfaces_guidance_refs() {
    use mcp_flowgate_core::discovery::{DiscoveryIndex, InMemoryDiscoveryIndex};

    let cfg = json!({
        "version": "1.0.0",
        "skills": {
            "house-voice": {
                "verb": "apply",
                "body": "Lead with the reader's problem. Short sentences."
            },
            "editorial-checklist": {
                "verb": "follow",
                "body": "1. Verify facts. 2. Cite sources."
            }
        },
        "workflows": {
            "wf": {
                "initialState": "draft",
                "skills": ["house-voice"],
                "states": {
                    "draft": {
                        "goal": "Write the draft",
                        "skills": ["editorial-checklist"],
                        "transitions": {
                            "submit": { "target": "done", "actor": "agent" }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    // The runtime needs `skills:` in the snapshot — driven through `resolve` so
    // the resolve-time stamping path is exercised.
    let resolved = mcp_flowgate_core::config::resolve(cfg.clone()).expect("config should resolve");

    let (runtime, _) = build_runtime(resolved.clone());
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
        })
        .await
        .unwrap();

    // The response carries a refs list that includes both scopes' refs.
    let refs = resp["guidance"]["refs"]
        .as_array()
        .expect("guidance.refs must be present");
    let subjects: Vec<&str> = refs
        .iter()
        .filter_map(|r| r["subject"].as_str())
        .collect();
    assert!(
        subjects.contains(&"house-voice"),
        "workflow-scope ref must be surfaced; got: {subjects:?}"
    );
    assert!(
        subjects.contains(&"editorial-checklist"),
        "state-scope ref must be surfaced; got: {subjects:?}"
    );

    // Each ref carries the verb from the top-level library.
    for r in refs {
        let subj = r["subject"].as_str().unwrap();
        let verb = r["verb"]
            .as_str()
            .unwrap_or_else(|| panic!("ref must carry verb; got {r}"));
        match subj {
            "house-voice" => assert_eq!(verb, "apply"),
            "editorial-checklist" => assert_eq!(verb, "follow"),
            other => panic!("unexpected ref subject: {other}"),
        }
    }

    // `gateway.describe(subject)` returns the body via the discovery layer.
    let discovery = InMemoryDiscoveryIndex::from_config(&resolved);
    let item = discovery
        .describe("house-voice")
        .await
        .unwrap()
        .expect("house-voice should be discoverable");
    let body = serde_json::to_value(&item).unwrap();
    let body_str = body["body"].as_str().expect("describe must return body");
    assert!(
        body_str.contains("reader's problem"),
        "body should be surfaced; got: {body_str}"
    );
    assert_eq!(body["verb"].as_str(), Some("apply"));
}
