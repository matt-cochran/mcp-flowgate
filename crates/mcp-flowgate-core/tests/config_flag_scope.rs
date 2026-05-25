//! SPEC §8.4 + §20.2 — `flowgate.*` flags are runtime-only and MUST be
//! rejected when nested inside any `workflows:` definition. Otherwise an
//! LLM-authored workflow could embed a key intending to flip the bypass
//! flag on for itself.

use mcp_flowgate_core::config;
use serde_json::{json, Value};

// ── Top-level flowgate.* is fine ────────────────────────────────────────────

#[test]
fn top_level_flowgate_block_accepted() {
    let cfg = json!({
        "version": "1.0.0",
        "flowgate": { "authoring": { "write_enabled": true } },
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": { "s": { "terminal": true } }
            }
        }
    });
    config::resolve(cfg).expect("top-level flowgate block must be accepted");
}

#[test]
fn top_level_flowgate_strict_namespacing_accepted() {
    let cfg = json!({
        "version": "1.0.0",
        "flowgate": { "strict_namespacing": false },
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": { "s": { "terminal": true } }
            }
        }
    });
    config::resolve(cfg).expect("top-level strict_namespacing must be accepted");
}

// ── Nested under workflows.* → reject ───────────────────────────────────────

#[test]
fn flowgate_object_inside_workflow_rejected() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "s",
                "flowgate": { "authoring": { "write_enabled": true } },
                "states": { "s": { "terminal": true } }
            }
        }
    });
    let err = config::resolve(cfg).expect_err("must reject");
    let msg = format!("{err}");
    assert!(
        msg.contains("CONFIG_FLAG_NOT_RUNTIME_MUTABLE"),
        "expected CONFIG_FLAG_NOT_RUNTIME_MUTABLE in error; got: {msg}"
    );
    assert!(
        msg.contains("flowgate"),
        "error must name the offending key; got: {msg}"
    );
}

#[test]
fn flowgate_dot_key_inside_workflow_rejected() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "s",
                "flowgate.authoring.write_enabled": true,
                "states": { "s": { "terminal": true } }
            }
        }
    });
    let err = config::resolve(cfg).expect_err("must reject dotted form too");
    assert!(format!("{err}").contains("CONFIG_FLAG_NOT_RUNTIME_MUTABLE"));
}

#[test]
fn flowgate_nested_under_state_rejected() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": {
                    "s": {
                        "terminal": true,
                        "flowgate": { "strict_namespacing": false }
                    }
                }
            }
        }
    });
    let err = config::resolve(cfg).expect_err("nested under state must reject");
    let msg = format!("{err}");
    assert!(msg.contains("CONFIG_FLAG_NOT_RUNTIME_MUTABLE"));
    // Path must reference the state where the flag was found.
    assert!(
        msg.contains("/workflows/demo/states/s"),
        "error must include JSON-Pointer path naming the location; got: {msg}"
    );
}

#[test]
fn flowgate_nested_deep_inside_transition_rejected() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": {
                    "s": {
                        "transitions": {
                            "go": {
                                "target": "done",
                                "flowgate": { "authoring": { "write_enabled": true } }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let err = config::resolve(cfg).expect_err("must reject in transition");
    let msg = format!("{err}");
    assert!(msg.contains("CONFIG_FLAG_NOT_RUNTIME_MUTABLE"));
    assert!(msg.contains("/workflows/demo/states/s/transitions/go"));
}

// ── Confounders: legitimate keys that contain "flowgate" elsewhere ──────────

#[test]
fn workflow_id_named_flowgate_accepted() {
    // The validator scans for `flowgate` as an OBJECT KEY inside a
    // workflow def, not as a workflow id. A workflow literally named
    // "flowgate" should still load.
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "flowgate": {
                "initialState": "s",
                "states": { "s": { "terminal": true } }
            }
        }
    });
    config::resolve(cfg).expect("workflow id 'flowgate' must be accepted");
}

#[test]
fn unrelated_key_containing_flowgate_substring_accepted() {
    // `flowgate-style` is not a flowgate.* runtime flag — just a label
    // someone might use.
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": {
                    "s": {
                        "terminal": true,
                        "flowgate-style-marker": "ok"
                    }
                }
            }
        }
    });
    config::resolve(cfg).expect("unrelated 'flowgate-...' key must be accepted (no dot)");
}

// ── Edge: empty workflows block + no flowgate keys → no-op ──────────────────

#[test]
fn no_workflows_block_at_all_accepted() {
    let cfg: Value = json!({
        "version": "1.0.0",
        "proxy": { "expose": [{ "name": "hello", "executor": { "kind": "noop" } }] }
    });
    config::resolve(cfg).expect("no workflows: block, nothing to validate");
}
