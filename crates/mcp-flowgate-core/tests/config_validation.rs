//! Config validation tests — version field requirements, etc.

use mcp_flowgate_core::config;
use mcp_flowgate_core::validate::validate_workflows;
use serde_json::json;

#[test]
fn valid_config_with_version_field() {
    let yaml = r#"
version: "1.0.0"
proxy:
  expose:
    - name: echo
      executor: { kind: noop }
"#;
    let result = config::resolve_str(yaml);
    assert!(result.is_ok(), "config with version field should parse OK");
}

#[test]
fn config_without_version_field_still_parses() {
    // The resolver doesn't require version; the `check` subcommand does.
    // This test verifies that resolve_str doesn't reject it — the
    // requirement is at the binary layer.
    let yaml = r#"
proxy:
  expose:
    - name: echo
      executor: { kind: noop }
"#;
    let result = config::resolve_str(yaml);
    assert!(
        result.is_ok(),
        "config without version should still resolve"
    );
}

// ── Blackboard slot-check tests ──────────────────────────────────────────────

#[test]
fn undeclared_output_slot_warns() {
    let config = json!({
        "workflows": {
            "ci": {
                "initialState": "lint",
                "blackboard": ["lintPassed"],
                "states": {
                    "lint": {
                        "transitions": {
                            "done": {
                                "target": "finish",
                                "output": { "typo": "$.result.value" }
                            }
                        }
                    },
                    "finish": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let warnings: Vec<_> = diags
        .iter()
        .filter(|d| !d.is_error() && d.message().contains("typo") && d.message().contains("blackboard"))
        .collect();
    assert!(
        !warnings.is_empty(),
        "expected a warning naming 'typo' and referencing 'blackboard', got: {diags:?}"
    );
}

#[test]
fn undeclared_output_slot_warns_object_form() {
    // blackboard declared as object form { "lintPassed": {} }; transition writes
    // undeclared key "typo" — should produce the same warning as the array form.
    let config = json!({
        "workflows": {
            "ci": {
                "initialState": "lint",
                "blackboard": { "lintPassed": {} },
                "states": {
                    "lint": {
                        "transitions": {
                            "done": {
                                "target": "finish",
                                "output": { "typo": "$.result.value" }
                            }
                        }
                    },
                    "finish": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let warnings: Vec<_> = diags
        .iter()
        .filter(|d| !d.is_error() && d.message().contains("typo") && d.message().contains("blackboard"))
        .collect();
    assert!(
        !warnings.is_empty(),
        "expected a warning naming 'typo' and referencing 'blackboard' (object-form blackboard), got: {diags:?}"
    );
}

#[test]
fn declared_blackboard_accepted() {
    let config = json!({
        "workflows": {
            "ci": {
                "initialState": "lint",
                "blackboard": ["lintPassed"],
                "states": {
                    "lint": {
                        "transitions": {
                            "done": {
                                "target": "finish",
                                "output": { "lintPassed": "$.result.value" }
                            }
                        }
                    },
                    "finish": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let slot_warnings: Vec<_> = diags
        .iter()
        .filter(|d| !d.is_error() && d.message().contains("blackboard"))
        .collect();
    assert!(
        slot_warnings.is_empty(),
        "expected no blackboard slot warnings for declared key 'lintPassed', got: {slot_warnings:?}"
    );
}

#[test]
fn no_blackboard_declared_no_warning() {
    let config = json!({
        "workflows": {
            "ci": {
                "initialState": "lint",
                "states": {
                    "lint": {
                        "transitions": {
                            "done": {
                                "target": "finish",
                                "output": { "anyKey": "$.result.value" }
                            }
                        }
                    },
                    "finish": { "terminal": true }
                }
            }
        }
    });
    let diags = validate_workflows(&config);
    let slot_warnings: Vec<_> = diags
        .iter()
        .filter(|d| !d.is_error() && d.message().contains("blackboard"))
        .collect();
    assert!(
        slot_warnings.is_empty(),
        "expected no blackboard slot warnings when no blackboard is declared, got: {slot_warnings:?}"
    );
}

// ── Workflow definition version discriminator tests ─────────────────────────

/// A workflow definition with an explicit `version` retains that value after resolution.
#[test]
fn workflow_definition_explicit_version_is_preserved() {
    let yaml = r#"
proxy:
  expose:
    - name: echo
      executor: { kind: noop }
workflows:
  ci:
    version: "2026-05-22"
    initialState: lint
    states:
      lint:
        terminal: true
"#;
    let resolved = config::resolve_str(yaml).expect("should resolve");
    let version = resolved
        .pointer("/workflows/ci/version")
        .and_then(|v| v.as_str())
        .expect("workflows.ci.version should be present");
    assert_eq!(
        version, "2026-05-22",
        "explicit version should be preserved; got: {version:?}"
    );
}

/// A workflow definition with no `version` has `version == "0"` after resolution.
#[test]
fn workflow_definition_missing_version_gets_default() {
    let yaml = r#"
proxy:
  expose:
    - name: echo
      executor: { kind: noop }
workflows:
  ci:
    initialState: lint
    states:
      lint:
        terminal: true
"#;
    let resolved = config::resolve_str(yaml).expect("should resolve");
    let version = resolved
        .pointer("/workflows/ci/version")
        .and_then(|v| v.as_str())
        .expect("workflows.ci.version should be present after resolution");
    assert_eq!(
        version, "0",
        "missing version should default to \"0\"; got: {version:?}"
    );
}

/// Sanity: the top-level config `version` field is unaffected by per-workflow defaulting.
#[test]
fn top_level_config_version_unchanged_after_workflow_defaulting() {
    let yaml = r#"
version: "1.0.0"
proxy:
  expose:
    - name: echo
      executor: { kind: noop }
workflows:
  ci:
    initialState: lint
    states:
      lint:
        terminal: true
"#;
    let resolved = config::resolve_str(yaml).expect("should resolve");
    let top_version = resolved
        .pointer("/version")
        .and_then(|v| v.as_str())
        .expect("top-level version should be present");
    assert_eq!(
        top_version, "1.0.0",
        "top-level config version must be unchanged; got: {top_version:?}"
    );
    // Workflow version should still get its default.
    let wf_version = resolved
        .pointer("/workflows/ci/version")
        .and_then(|v| v.as_str())
        .expect("workflows.ci.version should be present after resolution");
    assert_eq!(wf_version, "0", "workflow version should default to \"0\"");
}
