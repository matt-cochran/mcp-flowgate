//! SPEC §6.1 / V12 — the `use:` binding contract for `kind: workflow`
//! executors that invoke a capability. The runtime executor branch is
//! covered by `walk_examples::scoped_capability_io_roundtrip` (M2
//! acceptance); this file exercises the load-time shape validator.

use mcp_flowgate_core::config::resolve_str;
use mcp_flowgate_core::validate::validate_workflows;

fn diagnostics_for(yaml: &str) -> Vec<String> {
    let config = resolve_str(yaml).expect("yaml resolves");
    validate_workflows(&config)
        .into_iter()
        .map(|d| d.message().to_string())
        .collect()
}

fn has_error_containing(diags: &[String], needle: &str) -> bool {
    diags.iter().any(|m| m.contains(needle))
}

// ---------- V12 — kind: workflow → cap.* requires use: ----------

#[test]
fn v12_accepts_cap_invocation_with_use_block() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    initialState: ready
    snippet:
      inputs:  { plan: { type: object } }
      outputs: { verdict: { type: string } }
    states:
      ready: { terminal: true }
  flow.add-feature:
    initialState: planning
    states:
      planning:
        transitions:
          plan_drafted:
            target: done
            executor:
              kind: workflow
              definitionId: cap.plan.vet
              use:
                inputs:
                  plan: "$.context.draft_plan"
                outputs:
                  "$.context.vet_verdict": verdict
      done: { terminal: true }
"#;
    let diags = diagnostics_for(yaml);
    assert!(
        !has_error_containing(&diags, "MISSING_USE"),
        "should accept cap call with use: {diags:?}"
    );
}

#[test]
fn v12_rejects_cap_invocation_missing_use_block() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    initialState: ready
    snippet:
      inputs:  {}
      outputs: {}
    states:
      ready: { terminal: true }
  flow.add-feature:
    initialState: planning
    states:
      planning:
        transitions:
          plan_drafted:
            target: done
            executor:
              kind: workflow
              definitionId: cap.plan.vet
      done: { terminal: true }
"#;
    let diags = diagnostics_for(yaml);
    assert!(
        has_error_containing(&diags, "MISSING_USE"),
        "expected MISSING_USE: {diags:?}"
    );
}

// ---------- V12 (shape half) — use.outputs path constraints ----------

#[test]
fn v12_accepts_simple_single_segment_host_paths() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    initialState: ready
    snippet:
      inputs:  {}
      outputs: { verdict: { type: string } }
    states:
      ready: { terminal: true }
  flow.add-feature:
    initialState: planning
    states:
      planning:
        transitions:
          plan_drafted:
            target: done
            executor:
              kind: workflow
              definitionId: cap.plan.vet
              use:
                outputs:
                  "$.context.vet_verdict": verdict
      done: { terminal: true }
"#;
    let diags = diagnostics_for(yaml);
    assert!(
        !has_error_containing(&diags, "INVALID_USE_OUTPUT_PATH"),
        "should accept $.context.vet_verdict: {diags:?}"
    );
}

#[test]
fn v12_rejects_nested_host_path() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    initialState: ready
    snippet:
      inputs:  {}
      outputs: { verdict: { type: string } }
    states:
      ready: { terminal: true }
  flow.add-feature:
    initialState: planning
    states:
      planning:
        transitions:
          plan_drafted:
            target: done
            executor:
              kind: workflow
              definitionId: cap.plan.vet
              use:
                outputs:
                  "$.context.deeply.nested.path": verdict
      done: { terminal: true }
"#;
    let diags = diagnostics_for(yaml);
    assert!(
        has_error_containing(&diags, "INVALID_USE_OUTPUT_PATH"),
        "expected INVALID_USE_OUTPUT_PATH: {diags:?}"
    );
}

#[test]
fn v12_rejects_non_context_root_host_path() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    initialState: ready
    snippet:
      inputs:  {}
      outputs: { verdict: { type: string } }
    states:
      ready: { terminal: true }
  flow.add-feature:
    initialState: planning
    states:
      planning:
        transitions:
          plan_drafted:
            target: done
            executor:
              kind: workflow
              definitionId: cap.plan.vet
              use:
                outputs:
                  "$.workflow.input.bogus": verdict
      done: { terminal: true }
"#;
    let diags = diagnostics_for(yaml);
    assert!(
        has_error_containing(&diags, "INVALID_USE_OUTPUT_PATH"),
        "should reject non-context root: {diags:?}"
    );
}

// ---------- Legacy callers (kind: workflow → non-cap.*) skip V12 ----------

#[test]
fn v12_does_not_fire_on_legacy_workflow_to_workflow_invocation() {
    let yaml = r#"
version: "1.0.0"
workflows:
  with_artifact_lock:
    initialState: ready
    states:
      ready: { terminal: true }
  parent_workflow:
    initialState: working
    states:
      working:
        transitions:
          fan_out:
            target: done
            executor:
              kind: workflow
              definitionId: with_artifact_lock
              input:
                artifact: "$.context.artifact_name"
      done: { terminal: true }
"#;
    let diags = diagnostics_for(yaml);
    assert!(
        !has_error_containing(&diags, "MISSING_USE"),
        "legacy non-cap workflow targets should not trigger V12: {diags:?}"
    );
}

// ---------- expand_use_bindings synthesizes the transition output ----------

#[test]
fn use_block_expansion_synthesizes_transition_output() {
    // After resolve_str, the transition should carry an `output:` map
    // derived from use.outputs. This is what makes merge_output project
    // cap-declared outputs back to host context slots.
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    initialState: ready
    snippet:
      inputs:  {}
      outputs: { verdict: { type: string } }
    states:
      ready: { terminal: true }
  flow.add-feature:
    initialState: planning
    states:
      planning:
        transitions:
          plan_drafted:
            target: done
            executor:
              kind: workflow
              definitionId: cap.plan.vet
              use:
                outputs:
                  "$.context.vet_verdict": verdict
      done: { terminal: true }
"#;
    let config = resolve_str(yaml).expect("yaml resolves");
    let synthesized = config
        .pointer("/workflows/flow.add-feature/states/planning/transitions/plan_drafted/output/vet_verdict")
        .and_then(|v| v.as_str())
        .expect("synthesized output should be present");
    assert_eq!(synthesized, "$.output.verdict");
}

#[test]
fn use_block_expansion_embeds_snippet_outputs_on_executor() {
    let yaml = r#"
version: "1.0.0"
workflows:
  cap.plan.vet:
    initialState: ready
    snippet:
      inputs:  {}
      outputs: { verdict: { type: string, enum: [pass, fail] } }
    states:
      ready: { terminal: true }
  flow.add-feature:
    initialState: planning
    states:
      planning:
        transitions:
          plan_drafted:
            target: done
            executor:
              kind: workflow
              definitionId: cap.plan.vet
              use:
                outputs:
                  "$.context.vet_verdict": verdict
      done: { terminal: true }
"#;
    let config = resolve_str(yaml).expect("yaml resolves");
    let embedded = config
        .pointer("/workflows/flow.add-feature/states/planning/transitions/plan_drafted/executor/_snippetOutputs/verdict/enum")
        .expect("_snippetOutputs should be embedded");
    assert!(embedded.is_array(), "got {embedded:?}");
}
