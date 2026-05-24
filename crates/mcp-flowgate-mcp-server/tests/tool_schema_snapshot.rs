//! Parity snapshots for the MCP tool surface.
//!
//! Pins down the exact JSON Schema and human description published for each
//! of the seven stable tools. Any change to either is a visible MCP surface
//! change and must be intentional. These tests exist so that the upcoming
//! schemars-based generation refactor can't drift the published schemas
//! without a test failure.

use mcp_flowgate_mcp_server::{
    tool_definitions, TOOL_DESCRIBE, TOOL_EXPLAIN, TOOL_GET, TOOL_HOME, TOOL_SEARCH, TOOL_START,
    TOOL_SUBMIT,
};
use serde_json::{json, Value};

fn schema_of(name: &str) -> Value {
    let tool = tool_definitions()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("tool '{name}' not found"));
    Value::Object((*tool.input_schema).clone())
}

fn description_of(name: &str) -> String {
    tool_definitions()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("tool '{name}' not found"))
        .description
        .as_deref()
        .unwrap_or_else(|| panic!("tool '{name}' has no description"))
        .to_string()
}

#[test]
fn home_schema_snapshot() {
    assert_eq!(
        schema_of(TOOL_HOME),
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    );
}

#[test]
fn search_schema_snapshot() {
    assert_eq!(
        schema_of(TOOL_SEARCH),
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "kind": { "type": "string", "enum": ["workflow", "capability", "connection"] },
                "limit": { "type": "integer", "default": 10 }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    );
}

#[test]
fn describe_schema_snapshot() {
    assert_eq!(
        schema_of(TOOL_DESCRIBE),
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "workflowId": { "type": "string" }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    );
}

#[test]
fn start_schema_snapshot() {
    assert_eq!(
        schema_of(TOOL_START),
        json!({
            "type": "object",
            "properties": {
                "definitionId": { "type": "string" },
                "input": { "type": "object" }
            },
            "required": ["definitionId", "input"],
            "additionalProperties": false
        })
    );
}

#[test]
fn get_schema_snapshot() {
    assert_eq!(
        schema_of(TOOL_GET),
        json!({
            "type": "object",
            "properties": { "workflowId": { "type": "string" } },
            "required": ["workflowId"],
            "additionalProperties": false
        })
    );
}

#[test]
fn submit_schema_snapshot() {
    assert_eq!(
        schema_of(TOOL_SUBMIT),
        json!({
            "type": "object",
            "properties": {
                "workflowId": { "type": "string" },
                "expectedVersion": { "type": "integer" },
                "transition": { "type": "string" },
                "arguments": { "type": "object" },
                "summary": { "type": "string" }
            },
            "required": ["workflowId", "expectedVersion", "transition", "arguments"],
            "additionalProperties": false
        })
    );
}

#[test]
fn explain_schema_snapshot() {
    assert_eq!(
        schema_of(TOOL_EXPLAIN),
        json!({
            "type": "object",
            "properties": {
                "workflowId": { "type": "string" },
                "transition": { "type": "string" }
            },
            "required": ["workflowId", "transition"],
            "additionalProperties": false
        })
    );
}

#[test]
fn descriptions_snapshot() {
    let want: &[(&str, &str)] = &[
        (
            TOOL_HOME,
            "Get the gateway's discovery home: HATEOAS links to search and list capabilities.",
        ),
        (
            TOOL_SEARCH,
            "Search workflows and proxy capabilities by free-text query. Returns hits with start_workflow links.",
        ),
        (
            TOOL_DESCRIBE,
            "Describe a workflow or capability by id, including its inputSchema.",
        ),
        (
            TOOL_START,
            "Start a workflow. Use definitionId 'proxy_default' for proxy mode.",
        ),
        (
            TOOL_GET,
            "Get current workflow state and valid next HATEOAS actions.",
        ),
        (
            TOOL_SUBMIT,
            "Submit one transition listed in the latest links array of a workflow response.",
        ),
        (
            TOOL_EXPLAIN,
            "Explain whether a transition is currently allowed.",
        ),
    ];
    for (name, expected) in want {
        assert_eq!(
            description_of(name),
            *expected,
            "description drift for tool {name}"
        );
    }
}
