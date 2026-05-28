//! Tool-list construction. The seven stable tools plus the optional
//! `gateway.skills.search` (advertised only when the server is configured
//! with `with_skills_search(true)`).

use std::borrow::Cow;
use std::sync::Arc;

use mcp_flowgate_core::discovery::DiscoveryKind;
use rmcp::model::{JsonObject, Tool};
use serde_json::json;

use crate::args::{
    empty_object_schema, schema_for_args, DescribeArgs, ExplainArgs, GetArgs, SearchArgs,
    StartArgs, SubmitArgs,
};
use crate::{
    TOOL_DESCRIBE, TOOL_EXPLAIN, TOOL_GET, TOOL_HOME, TOOL_LEXICON_DEFINE,
    TOOL_LEXICON_LOOKUP, TOOL_LEXICON_SEARCH, TOOL_SCRIPTS_SEARCH, TOOL_SEARCH,
    TOOL_SKILLS_SEARCH, TOOL_START, TOOL_SUBMIT,
};

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
            Cow::Borrowed(TOOL_HOME),
            Cow::Borrowed(
                "Get the gateway's discovery home: HATEOAS links to search and list capabilities.",
            ),
            empty_object_schema(),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_SEARCH),
            Cow::Borrowed(
                "Search workflows and proxy capabilities by free-text query. Returns hits with start_workflow links.",
            ),
            schema_for_args::<SearchArgs>(&["query"]),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_DESCRIBE),
            Cow::Borrowed("Describe a workflow or capability by id, including its inputSchema."),
            schema_for_args::<DescribeArgs>(&["id"]),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_START),
            Cow::Borrowed("Start a workflow. Use definitionId 'proxy_default' for proxy mode."),
            schema_for_args::<StartArgs>(&["definitionId", "input"]),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_GET),
            Cow::Borrowed("Get current workflow state and valid next HATEOAS actions."),
            schema_for_args::<GetArgs>(&["workflowId"]),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_SUBMIT),
            Cow::Borrowed(
                "Submit one transition listed in the latest links array of a workflow response.",
            ),
            schema_for_args::<SubmitArgs>(&[
                "workflowId",
                "expectedVersion",
                "transition",
                "arguments",
            ]),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_EXPLAIN),
            Cow::Borrowed("Explain whether a transition is currently allowed."),
            schema_for_args::<ExplainArgs>(&["workflowId", "transition"]),
        ),
        lexicon_search_tool_definition(),
        lexicon_lookup_tool_definition(),
        lexicon_define_tool_definition(),
    ]
}

/// SPEC §30.5 — gateway.lexicon.search definition.
pub fn lexicon_search_tool_definition() -> Tool {
    let schema = json!({
        "type": "object",
        "properties": {
            "query":            { "type": "string", "description": "substring match against term + definition" },
            "bounded_context":  { "type": "string", "description": "optional DDD bounded context filter" },
            "limit":            { "type": "integer", "description": "max hits (default 10)" }
        }
    });
    Tool::new(
        Cow::Borrowed(TOOL_LEXICON_SEARCH),
        Cow::Borrowed(
            "Search the lexicon (ubiquitous language store) for terms matching a query. \
             Returns hits with definitions, refs, and governance level.",
        ),
        Arc::new(schema.as_object().cloned().expect("invariant: json!({ ... }) literal is an object")),
    )
}

/// SPEC §30.5 — gateway.lexicon.lookup definition.
pub fn lexicon_lookup_tool_definition() -> Tool {
    let schema = json!({
        "type": "object",
        "required": ["term"],
        "properties": {
            "term":            { "type": "string", "description": "exact term name" },
            "bounded_context": { "type": "string", "description": "optional filter; rejects entries with a different bounded_context" }
        }
    });
    Tool::new(
        Cow::Borrowed(TOOL_LEXICON_LOOKUP),
        Cow::Borrowed(
            "Exact lexicon lookup by term name. Returns the term's entry \
             (definition, examples, refs, governance) or null when absent.",
        ),
        Arc::new(schema.as_object().cloned().expect("invariant: json!({ ... }) literal is an object")),
    )
}

/// SPEC §30.6 — gateway.lexicon.define definition.
/// Agents calling against `human-only` terms get rejected.
pub fn lexicon_define_tool_definition() -> Tool {
    let schema = json!({
        "type": "object",
        "required": ["term", "definition"],
        "properties": {
            "term":             { "type": "string", "minLength": 1 },
            "definition":       { "type": "string", "minLength": 1 },
            "bounded_context":  { "type": "string" },
            "refs":             { "type": "array", "items": { "type": "string" } },
            "governance":       { "type": "string", "enum": ["human-only", "agent-may-propose"] }
        }
    });
    Tool::new(
        Cow::Borrowed(TOOL_LEXICON_DEFINE),
        Cow::Borrowed(
            "Propose or set a lexicon term. Governance-gated: agents writing \
             against a `human-only` term are rejected with \
             LEXICON_DEFINE_REQUIRES_HUMAN. Writes land in the runtime overlay; \
             operators persist by editing flowgate.yaml.",
        ),
        Arc::new(schema.as_object().cloned().expect("invariant: json!({ ... }) literal is an object")),
    )
}

/// SPEC §22 — definition for the authoring-time `gateway.scripts.search`
/// tool. Appended to the tool list only when
/// `FlowgateServer::with_scripts_search(true)` is configured. Mirrors the
/// skills-search tool: returns refs only, never bodies. Filterable by
/// verb / subject_root / source.
pub fn scripts_search_tool_definition() -> Tool {
    let schema = json!({
        "type": "object",
        "properties": {
            "verb":         { "type": "string", "description": "one of the eight closed script verbs: build, test, deploy, format, lint, install, verify, run" },
            "subject_root": { "type": "string", "description": "first dotted segment of a subject (e.g. `build`, `test`, `ci`)" },
            "source":       { "type": "string", "description": "provenance filter (e.g. config, cognitive-architectures@v0.1.0)" },
            "limit":        { "type": "integer", "description": "max items (default 50, max 200)" }
        }
    });
    let schema_obj: JsonObject = schema
        .as_object()
        .cloned()
        .expect("scripts_search schema is an object");
    Tool::new(
        Cow::Borrowed(TOOL_SCRIPTS_SEARCH),
        Cow::Borrowed(
            "Authoring-time script search. Returns script refs filterable by verb/subject_root/source. \
             Bodies are fetched separately via gateway.describe.",
        ),
        Arc::new(schema_obj),
    )
}

/// SPEC §17.6 — definition for the authoring-time `gateway.skills.search`
/// tool. Appended to the tool list only when
/// `FlowgateServer::with_skills_search(true)` is configured. Returns refs,
/// never bodies (progressive disclosure, §5.4).
pub fn skills_search_tool_definition() -> Tool {
    let schema = json!({
        "type": "object",
        "properties": {
            "verb":         { "type": "string", "description": "one of the eight closed cognitive verbs" },
            "subject_root": { "type": "string", "description": "first dotted segment of a subject" },
            "source":       { "type": "string", "description": "provenance filter (e.g. config, git+https://...)" },
            "limit":        { "type": "integer", "description": "max items (default 50, max 200)" }
        }
    });
    let schema_obj: JsonObject = schema
        .as_object()
        .cloned()
        .expect("skills_search schema is an object");
    Tool::new(
        Cow::Borrowed(TOOL_SKILLS_SEARCH),
        Cow::Borrowed(
            "Authoring-time skill search. Returns guidance refs filterable by verb/subject_root/source. \
             Bodies are fetched separately via gateway.describe.",
        ),
        Arc::new(schema_obj),
    )
}

pub(crate) fn instructions() -> &'static str {
    r#"This is the mcp-flowgate gateway.

The tool surface is stable across configs:
  Discovery — gateway.home, gateway.search, gateway.describe
  Workflow  — workflow.start, workflow.get, workflow.submit, workflow.explain

Typical flow:
1. Call gateway.home to find search and list-capabilities links.
2. Call gateway.search with a free-text query to find workflows or proxy capabilities.
3. Pick a hit, follow its `start` or `start_proxy_session` link to call workflow.start.
4. Read the workflow response's `links` array — each is a legal next transition.
5. Use workflow.submit with the link's args plus your arguments. Repeat.
6. Stop when result.status is 'completed'.

Invalid calls always return the current legal links so you can recover."#
}
