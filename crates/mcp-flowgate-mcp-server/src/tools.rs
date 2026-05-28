//! Tool-list construction. SPEC §32 — the stable MCP surface is exactly two
//! tools: `flowgate.query` and `flowgate.command`. All operations are reached
//! by varying args, not the tool name.
//!
//! The optional skills/scripts search tools are no longer separate MCP tools;
//! they are gated-access paths within `flowgate.query` (kind="skill" /
//! kind="script"). The exported helper functions are kept for compatibility
//! with existing builder code but may be cleaned up in a follow-up.

use std::borrow::Cow;
use std::sync::Arc;

use mcp_flowgate_core::discovery::DiscoveryKind;
use rmcp::model::{JsonObject, Tool};
use serde_json::json;

use crate::args::{schema_for_args, CommandArgs, QueryArgs};
use crate::{TOOL_COMMAND, TOOL_QUERY};

pub(crate) fn parse_kind(s: &str) -> Option<DiscoveryKind> {
    match s {
        "workflow" => Some(DiscoveryKind::Workflow),
        "capability" => Some(DiscoveryKind::Capability),
        "connection" => Some(DiscoveryKind::Connection),
        _ => None,
    }
}

pub fn tool_definitions() -> Vec<Tool> {
    vec![
        Tool::new(
            Cow::Borrowed(TOOL_QUERY),
            Cow::Borrowed(
                "SPEC §32 read tool. Dispatches by present-field shape: \
                 {} → home; query → search; subject → describe; \
                 workflowId → get; workflowId+transition → explain. \
                 Add kind='skill'|'script'|'lexicon' to scope search results.",
            ),
            schema_for_args::<QueryArgs>(&[]),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_COMMAND),
            Cow::Borrowed(
                "SPEC §32 write tool. Dispatches by present-field shape: \
                 definitionId → start; workflowId+expectedVersion+transition → submit; \
                 subject='lexicon:<term>'+definition → define (requires lexicon writes enabled).",
            ),
            schema_for_args::<CommandArgs>(&[]),
        ),
    ]
}

/// SPEC §22 — definition for the authoring-time scripts search path.
/// Kept for compatibility; under §32 this is reached via flowgate.query
/// with kind="script" rather than a separate tool. Returns a stub
/// definition — callers building older tool lists may still reference this.
pub fn scripts_search_tool_definition() -> Tool {
    let schema = json!({
        "type": "object",
        "properties": {
            "verb":         { "type": "string" },
            "subject_root": { "type": "string" },
            "source":       { "type": "string" },
            "limit":        { "type": "integer" }
        }
    });
    let schema_obj: JsonObject = schema
        .as_object()
        .cloned()
        .expect("scripts_search schema is an object");
    Tool::new(
        Cow::Borrowed("flowgate.query"),
        Cow::Borrowed(
            "Authoring-time script search. Use flowgate.query with kind='script'.",
        ),
        Arc::new(schema_obj),
    )
}

/// SPEC §17.6 — definition for the authoring-time skills search path.
/// Kept for compatibility; under §32 this is reached via flowgate.query
/// with kind="skill" rather than a separate tool. Returns a stub
/// definition — callers building older tool lists may still reference this.
pub fn skills_search_tool_definition() -> Tool {
    let schema = json!({
        "type": "object",
        "properties": {
            "verb":         { "type": "string" },
            "subject_root": { "type": "string" },
            "source":       { "type": "string" },
            "limit":        { "type": "integer" }
        }
    });
    let schema_obj: JsonObject = schema
        .as_object()
        .cloned()
        .expect("skills_search schema is an object");
    Tool::new(
        Cow::Borrowed("flowgate.query"),
        Cow::Borrowed(
            "Authoring-time skill search. Use flowgate.query with kind='skill'.",
        ),
        Arc::new(schema_obj),
    )
}

pub(crate) fn instructions() -> &'static str {
    r#"This is the mcp-flowgate gateway. SPEC §32 two-tool surface.

The tool surface is exactly two tools, stable across configs:
  flowgate.query   — read: home, search, describe, get, explain
  flowgate.command — write: start, submit, define

Dispatch by present-field shape:
  flowgate.query {}                          → home (HATEOAS links)
  flowgate.query { query }                   → search (add kind= to filter)
  flowgate.query { subject }                 → describe
  flowgate.query { workflowId }              → get
  flowgate.query { workflowId, transition }  → explain

  flowgate.command { definitionId }                                    → start
  flowgate.command { workflowId, expectedVersion, transition }         → submit
  flowgate.command { subject: "lexicon:<term>", definition: { ... } }  → define

Typical flow:
1. Call flowgate.query {} to get the discovery home with HATEOAS links.
2. Call flowgate.query { query: "..." } to find workflows or capabilities.
3. Follow a start link: flowgate.command { definitionId: "...", input: {} }.
4. Read the workflow response's `links` array — each is a legal next transition.
5. Call flowgate.command { workflowId, expectedVersion, transition, arguments }.
6. Stop when result.status is 'completed'.

Invalid calls always return the current legal links so you can recover."#
}
