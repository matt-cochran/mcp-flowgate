//! FMECA T1 test: `--agent` CLI flag + on-disk `agents.yaml` must be
//! mutually exclusive at startup. Exercises the pure validator the
//! `flowgate walk` wiring delegates to.

use mcp_flowgate_tui::agent_resolver::{
    validate_agent_source_exclusivity, AmbiguousAgentSourceError,
};

#[test]
fn cli_flag_and_yaml_both_present_fails_startup() {
    let err = validate_agent_source_exclusivity(true, true)
        .expect_err("yaml + --agent simultaneously must be an error");
    let msg = err.to_string();
    assert!(
        msg.contains("ambiguous"),
        "error message must name the ambiguity: {msg}"
    );
    assert!(
        msg.contains("--agent"),
        "error message must mention --agent: {msg}"
    );
    assert!(
        msg.contains("agents.yaml"),
        "error message must mention agents.yaml: {msg}"
    );
}

#[test]
fn yaml_only_passes() {
    validate_agent_source_exclusivity(true, false).expect("yaml without --agent is fine");
}

#[test]
fn cli_flag_only_passes() {
    // Legacy v0.2 path — deprecated but still allowed.
    validate_agent_source_exclusivity(false, true).expect("--agent without yaml is fine");
}

#[test]
fn neither_present_passes() {
    // Caller's job to handle the "no agents" case; the exclusivity
    // check itself is fine with neither.
    validate_agent_source_exclusivity(false, false).expect("neither set is fine");
}

#[test]
fn error_type_is_zero_sized() {
    // The error carries no data — all the context is in the static
    // message. Pin the struct shape so future contributors don't
    // accidentally bloat it.
    assert_eq!(std::mem::size_of::<AmbiguousAgentSourceError>(), 0);
}
