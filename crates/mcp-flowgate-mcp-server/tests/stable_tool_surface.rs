//! Invariant 9 from INIT-1 §17 (extended through SPEC §30):
//! "MCP-facing tools remain stable."
//!
//! The original seven (gateway.* + workflow.*) are joined by three v0.4.x
//! lexicon tools (SPEC §30.5). The invariant is preserved: the surface
//! grows ADDITIVELY via SPEC amendments; no removals, no shape changes
//! to existing tools. All capability surfacing for configs still happens
//! through HATEOAS links inside response payloads — the gateway.* and
//! workflow.* tools never multiply per config.

use mcp_flowgate_mcp_server::{tool_definitions, STABLE_TOOL_NAMES};

#[test]
fn tool_list_matches_stable_names_exactly() {
    let tools = tool_definitions();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, STABLE_TOOL_NAMES);
}

#[test]
fn stable_tool_names_are_the_documented_ten() {
    assert_eq!(
        STABLE_TOOL_NAMES,
        &[
            // Original 7 — INIT-1 §17, locked by deprecation cycle (Tier 1).
            "gateway.home",
            "gateway.search",
            "gateway.describe",
            "workflow.start",
            "workflow.get",
            "workflow.submit",
            "workflow.explain",
            // Lexicon trio — SPEC §30.5 (v0.4.x).
            "gateway.lexicon.search",
            "gateway.lexicon.lookup",
            "gateway.lexicon.define",
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
