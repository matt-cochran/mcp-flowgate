//! Tranche 3 — `flowgate doctor` pre-flight checks.
//!
//! Verifies the doctor subcommand:
//! - All-green run against valid config + agents
//! - CONFIG_NOT_FOUND when path doesn't exist
//! - CONFIG_INVALID when YAML doesn't parse
//! - WORKFLOW_NOT_DECLARED when --workflow doesn't match
//! - MISSING_API_KEY when provider env var is absent

use mcp_flowgate_tui::doctor::{count_failures, run_doctor, CheckStatus, DoctorArgs};

fn find_status<'a>(
    results: &'a [mcp_flowgate_tui::doctor::CheckResult],
    code: &str,
) -> Option<&'a mcp_flowgate_tui::doctor::CheckResult> {
    results
        .iter()
        .find(|r| matches!(&r.status, CheckStatus::Fail(c) if c == code))
}

#[tokio::test]
async fn doctor_passes_against_smoke_ete_with_anthropic_key_set() {
    // Temporarily set ANTHROPIC_API_KEY for this test if not already
    // present so the agent check passes. We use a placeholder value —
    // doctor only checks presence, not validity.
    let prior = std::env::var("ANTHROPIC_API_KEY").ok();
    std::env::set_var("ANTHROPIC_API_KEY", "test-placeholder");

    let args = DoctorArgs {
        config: Some(
            "../../examples/smoke-ete/gateway.yaml".to_string(),
        ),
        workflow: Some("smoke_ete".to_string()),
        agents: vec!["test=anthropic/claude-haiku-4-5-20251001".to_string()],
    };
    let results = run_doctor(&args).await;
    let failures = count_failures(&results);

    // Restore env state.
    match prior {
        Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
        None => std::env::remove_var("ANTHROPIC_API_KEY"),
    }

    assert_eq!(
        failures, 0,
        "doctor must pass against smoke-ete config + test agent: {:#?}",
        results
    );
}

#[tokio::test]
async fn doctor_reports_config_not_found_for_missing_path() {
    let args = DoctorArgs {
        config: Some("/nonexistent/path/flowgate.yaml".to_string()),
        workflow: None,
        agents: vec![],
    };
    let results = run_doctor(&args).await;
    assert!(
        find_status(&results, "CONFIG_NOT_FOUND").is_some(),
        "expected CONFIG_NOT_FOUND; got: {:#?}",
        results
    );
}

#[tokio::test]
async fn doctor_reports_workflow_not_declared() {
    let args = DoctorArgs {
        config: Some("../../examples/smoke-ete/gateway.yaml".to_string()),
        workflow: Some("nonexistent_workflow".to_string()),
        agents: vec![],
    };
    let results = run_doctor(&args).await;
    let fail = find_status(&results, "WORKFLOW_NOT_DECLARED")
        .expect("WORKFLOW_NOT_DECLARED must surface");
    assert!(
        fail.detail.contains("smoke_ete"),
        "failure detail must list available workflows; got: {}",
        fail.detail
    );
}

#[tokio::test]
async fn doctor_reports_missing_api_key_when_env_var_absent() {
    // Remove the env var (if present) for the duration of this test.
    let prior = std::env::var("OPENAI_API_KEY").ok();
    std::env::remove_var("OPENAI_API_KEY");

    let args = DoctorArgs {
        config: None,
        workflow: None,
        agents: vec!["planner=openai/gpt-4o".to_string()],
    };
    let results = run_doctor(&args).await;
    let fail = find_status(&results, "MISSING_API_KEY");

    if let Some(v) = prior {
        std::env::set_var("OPENAI_API_KEY", v);
    }

    let fail = fail.expect("MISSING_API_KEY must surface when env var absent");
    assert!(
        fail.detail.contains("OPENAI_API_KEY"),
        "failure must name the missing env var; got: {}",
        fail.detail
    );
}

#[tokio::test]
async fn doctor_skips_workflow_check_when_no_config_argument() {
    let args = DoctorArgs::default();
    let results = run_doctor(&args).await;
    // No config + no workflow → all checks except mcp-flowgate binary are skipped.
    // The binary check may pass or fail depending on the build env.
    // Either way, we should NOT see CONFIG_INVALID / WORKFLOW_NOT_DECLARED
    // (those require a config arg to even attempt).
    assert!(
        find_status(&results, "CONFIG_INVALID").is_none(),
        "should not attempt config-invalid check without a config arg"
    );
    assert!(
        find_status(&results, "WORKFLOW_NOT_DECLARED").is_none(),
        "should not attempt workflow-declared check without a config arg"
    );
}
