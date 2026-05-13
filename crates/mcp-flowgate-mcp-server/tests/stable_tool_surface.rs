//! Invariant 9 from INIT-1 §17:
//! "MCP-facing tools remain stable."
//!
//! No matter what's in the gateway config, the rmcp tool list is exactly
//! `workflow.start`, `workflow.get`, `workflow.submit`, `workflow.explain`.
//! All capability surfacing happens through HATEOAS links inside response
//! payloads, never through new MCP tools.

use mcp_flowgate_mcp_server::{tool_definitions, STABLE_TOOL_NAMES};

#[test]
fn tool_list_matches_stable_names_exactly() {
    let tools = tool_definitions();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, STABLE_TOOL_NAMES);
}

#[test]
fn stable_tool_names_are_the_documented_seven() {
    assert_eq!(
        STABLE_TOOL_NAMES,
        &[
            "gateway.home",
            "gateway.search",
            "gateway.describe",
            "workflow.start",
            "workflow.get",
            "workflow.submit",
            "workflow.explain",
        ]
    );
}

#[test]
fn every_tool_has_an_input_schema() {
    for tool in tool_definitions() {
        assert!(
            !tool.input_schema.is_empty(),
            "tool '{}' missing inputSchema",
            tool.name
        );
        assert_eq!(
            tool.input_schema.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "tool '{}' inputSchema must be type=object",
            tool.name
        );
    }
}
