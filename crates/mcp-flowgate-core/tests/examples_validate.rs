//! Audit-resolution C.3 — every shipped example YAML in examples/ must
//! resolve cleanly under the v0.2 validator stack. This is the regression
//! guard against publishing broken reference configs that users would
//! copy-paste.

use mcp_flowgate_core::config;
use std::path::PathBuf;

fn examples_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/mcp-flowgate-core; walk up two parents
    // to the workspace root, then into examples/.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("examples");
    p
}

fn resolve_example(rel: &str) -> serde_json::Value {
    let path = examples_dir().join(rel);
    assert!(
        path.exists(),
        "example file must exist at {}",
        path.display()
    );
    config::load_resolved(&path).unwrap_or_else(|e| {
        panic!("example '{rel}' failed to resolve cleanly: {e}");
    })
}

// ── swe-agent reference workflow ────────────────────────────────────────────

#[test]
fn swe_agent_yaml_resolves_cleanly() {
    let _ = resolve_example("swe-agent.yaml");
}

#[test]
fn swe_agent_workflow_id_is_registered() {
    let resolved = resolve_example("swe-agent.yaml");
    let workflows = resolved
        .pointer("/workflows")
        .and_then(serde_json::Value::as_object)
        .expect("/workflows object");
    assert!(
        workflows.contains_key("swe_agent"),
        "swe_agent workflow id must be present; got keys: {:?}",
        workflows.keys().collect::<Vec<_>>()
    );
}

#[test]
fn swe_agent_declares_all_six_pipeline_states() {
    let resolved = resolve_example("swe-agent.yaml");
    let states = resolved
        .pointer("/workflows/swe_agent/states")
        .and_then(serde_json::Value::as_object)
        .expect("states");
    for expected in &[
        "intake",
        "planning",
        "retrieving",
        "editing",
        "verifying",
        "critiquing",
        "human_review",
        "completed",
    ] {
        assert!(
            states.contains_key(*expected),
            "missing state: {expected}; got: {:?}",
            states.keys().collect::<Vec<_>>()
        );
    }
}

#[test]
fn swe_agent_skill_subjects_all_have_library_entries() {
    let resolved = resolve_example("swe-agent.yaml");
    // Every referenced subject must appear in the top-level `skills:`.
    let skills = resolved
        .pointer("/skills")
        .and_then(serde_json::Value::as_object)
        .expect("/skills object");
    let referenced = [
        "plan.specify.change-request",
        "diagnose.codebase.search",
        "implement.edit.constrained",
        "review.code.adversarial",
        "review.code.final-approval",
    ];
    for s in &referenced {
        assert!(
            skills.contains_key(*s),
            "referenced skill '{s}' missing from `skills:` map"
        );
    }
}

// ── SPEC §21 — delegate fields on the 4 model-driven states ────────────────

#[test]
fn swe_agent_delegate_fields_present_on_four_model_driven_states() {
    let resolved = resolve_example("swe-agent.yaml");
    let states = resolved
        .pointer("/workflows/swe_agent/states")
        .and_then(serde_json::Value::as_object)
        .expect("states");
    // These four states delegate to TUI sub-agents per WIP.md Phase 3.
    let delegated = &[
        ("planning", "planning-agent"),
        ("retrieving", "retrieval-agent"),
        ("editing", "editing-agent"),
        ("critiquing", "critique-agent"),
    ];
    for (state_name, expected_agent) in delegated {
        let state = states
            .get(*state_name)
            .unwrap_or_else(|| panic!("missing state '{state_name}'"));
        let actual = state
            .get("delegate")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| {
                panic!("state '{state_name}' missing delegate field; got: {state}")
            });
        assert_eq!(actual, *expected_agent, "wrong delegate for '{state_name}'");
    }
}

#[test]
fn swe_agent_delegate_absent_on_deterministic_and_human_states() {
    let resolved = resolve_example("swe-agent.yaml");
    let states = resolved
        .pointer("/workflows/swe_agent/states")
        .and_then(serde_json::Value::as_object)
        .expect("states");
    // Verifying = deterministic executor (no LLM); human_review = actor:human.
    // Neither should carry a `delegate` — that field is for sub-agent-driven
    // states only (SPEC §21).
    for state_name in &["verifying", "human_review", "completed"] {
        let state = states
            .get(*state_name)
            .unwrap_or_else(|| panic!("missing state '{state_name}'"));
        assert!(
            state.get("delegate").is_none(),
            "state '{state_name}' MUST NOT carry a delegate; got: {state}"
        );
    }
}

// ── Other shipped examples must continue to validate ───────────────────────

#[test]
fn authoring_workflow_yaml_resolves_cleanly() {
    let _ = resolve_example("authoring-workflow.yaml");
}

#[test]
fn governed_change_yaml_resolves_cleanly() {
    let _ = resolve_example("governed-change.yaml");
}

#[test]
fn simple_proxy_yaml_resolves_cleanly() {
    let _ = resolve_example("simple-proxy.yaml");
}

// ── Regression guard: every *.yaml at examples/ top level must resolve ─────

#[test]
fn every_top_level_yaml_in_examples_resolves() {
    let dir = examples_dir();
    let entries = std::fs::read_dir(&dir).expect("examples/ dir readable");
    let mut failed: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        if ext != "yaml" && ext != "yml" {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Err(e) = config::load_resolved(&path) {
            failed.push(format!("{name}: {e}"));
        }
    }
    assert!(
        failed.is_empty(),
        "top-level example YAML(s) failed to resolve:\n  {}",
        failed.join("\n  ")
    );
}
