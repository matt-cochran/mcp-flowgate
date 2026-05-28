//! Sparse-args deserialization tests for the new `flowgate.query` and
//! `flowgate.command` dispatch boundary structs. Every field is
//! optional; the runtime selects the operation by which required-field
//! shape is present.

use mcp_flowgate_mcp_server::args::{CommandArgs, QueryArgs};
use serde_json::json;

#[test]
fn query_args_admits_empty() {
    let a: QueryArgs = serde_json::from_value(json!({})).unwrap();
    assert!(a.query.is_none());
    assert!(a.subject.is_none());
    assert!(a.workflow_id.is_none());
    assert!(a.transition.is_none());
    assert!(a.kind.is_none());
    assert!(a.limit.is_none());
}

#[test]
fn query_args_admits_search_shape() {
    let a: QueryArgs = serde_json::from_value(json!({
        "query": "swe",
        "kind": "workflow",
        "limit": 10
    })).unwrap();
    assert_eq!(a.query.as_deref(), Some("swe"));
    assert_eq!(a.kind.as_deref(), Some("workflow"));
    assert_eq!(a.limit, Some(10u64));
}

#[test]
fn query_args_admits_describe_in_workflow_shape() {
    let a: QueryArgs = serde_json::from_value(json!({
        "subject": "plan.specify.change-request",
        "workflowId": "wf_01H"
    })).unwrap();
    assert_eq!(a.subject.as_deref(), Some("plan.specify.change-request"));
    assert_eq!(a.workflow_id.as_deref(), Some("wf_01H"));
}

#[test]
fn command_args_admits_start_shape() {
    let a: CommandArgs = serde_json::from_value(json!({
        "definitionId": "swe_agent",
        "input": { "issue": "x" },
        "runId": "r-1"
    })).unwrap();
    assert_eq!(a.definition_id.as_deref(), Some("swe_agent"));
    assert!(a.workflow_id.is_none());
    assert_eq!(a.run_id.as_deref(), Some("r-1"));
}

#[test]
fn command_args_admits_submit_shape_with_summary() {
    // SPEC §6.3: submit can carry a model-authored summary; CommandArgs
    // must accept it so the wire shape for flowgate.command preserves it.
    let a: CommandArgs = serde_json::from_value(json!({
        "workflowId": "wf_01H",
        "expectedVersion": 3,
        "transition": "approve",
        "arguments": { "note": "fine" },
        "summary": "Approved after risk review"
    })).unwrap();
    assert_eq!(a.workflow_id.as_deref(), Some("wf_01H"));
    assert_eq!(a.expected_version, Some(3));
    assert_eq!(a.transition.as_deref(), Some("approve"));
    assert_eq!(a.summary.as_deref(), Some("Approved after risk review"));
}

#[test]
fn command_args_admits_define_shape() {
    let a: CommandArgs = serde_json::from_value(json!({
        "subject": "lexicon:churn",
        "definition": {
            "definition": "Loss of paying customer in a billing period.",
            "boundedContext": "billing"
        }
    })).unwrap();
    assert_eq!(a.subject.as_deref(), Some("lexicon:churn"));
    assert!(a.definition.is_some());
    assert!(a.definition_id.is_none());
    assert!(a.workflow_id.is_none());
}
