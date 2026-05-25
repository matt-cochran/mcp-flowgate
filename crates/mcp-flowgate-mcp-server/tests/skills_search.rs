//! SPEC §17.6 — `gateway.skills.search` MCP tool. Authoring-time only;
//! advertised conditionally; returns refs only (progressive disclosure).

use std::sync::Arc;

use mcp_flowgate_core::audit::{AuditSink, NullAuditSink};
use mcp_flowgate_core::discovery::{
    DiscoveryItem, DiscoveryKind, DiscoveryLink, InMemoryDiscoveryIndex,
};
use mcp_flowgate_core::guards::DefaultGuardEvaluator;
use mcp_flowgate_core::ports::ExecutorRegistry;
use mcp_flowgate_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use mcp_flowgate_core::WorkflowRuntime;
use mcp_flowgate_mcp_server::{FlowgateServer, TOOL_SKILLS_SEARCH};
use rmcp::model::{CallToolRequestParams, JsonObject};
use serde_json::{json, Value};

struct NoopRegistry;
impl ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn mcp_flowgate_core::Executor>> {
        None
    }
}

fn build_runtime() -> WorkflowRuntime {
    WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::default()),
        Arc::new(InMemoryWorkflowStore::default()),
        Arc::new(NoopRegistry),
        Arc::new(DefaultGuardEvaluator::new()),
        Arc::new(NullAuditSink) as Arc<dyn AuditSink>,
    )
}

fn fixture_item(subject: &str, verb: &str) -> DiscoveryItem {
    fixture_item_with_source(subject, verb, "config")
}

fn fixture_item_with_source(subject: &str, verb: &str, source: &str) -> DiscoveryItem {
    DiscoveryItem {
        id: subject.into(),
        kind: DiscoveryKind::Guidance,
        title: format!("title for {subject}"),
        description: String::new(),
        tags: vec![],
        examples: vec![],
        aliases: vec![],
        text: String::new(),
        links: vec![DiscoveryLink {
            rel: "home".into(),
            title: None,
            description: None,
            method: "gateway.home".into(),
            args: json!({}),
            input_schema: None,
        }],
        verb: Some(verb.into()),
        body: Some("body content the test must never see".into()),
        source: Some(source.into()),
    }
}

fn build_discovery() -> Arc<InMemoryDiscoveryIndex> {
    Arc::new(InMemoryDiscoveryIndex::new(vec![
        fixture_item("review.style.house-voice", "review"),
        fixture_item("review.editorial.checklist", "review"),
        fixture_item("debug.repro.standard", "diagnose"),
        fixture_item("authoring.skill.writing-rubric", "review"),
    ]))
}

fn enabled_server() -> FlowgateServer {
    FlowgateServer::new(build_runtime())
        .with_discovery(build_discovery())
        .with_skills_search(true)
}

fn disabled_server() -> FlowgateServer {
    FlowgateServer::new(build_runtime()).with_discovery(build_discovery())
}

fn call_search(args: Value) -> CallToolRequestParams {
    let m: JsonObject = args.as_object().cloned().expect("object");
    CallToolRequestParams::new(TOOL_SKILLS_SEARCH).with_arguments(m)
}

// ── Flag-off: tool absent from list_tools AND call rejected ─────────────────

#[tokio::test]
async fn tool_not_advertised_when_flag_off() {
    use mcp_flowgate_mcp_server::tool_definitions;
    let _server = disabled_server();
    // The default `tool_definitions()` does NOT include skills.search.
    let names: Vec<String> = tool_definitions().into_iter().map(|t| t.name.to_string()).collect();
    assert!(
        !names.contains(&TOOL_SKILLS_SEARCH.to_string()),
        "skills.search must NOT appear in default tool list; got: {names:?}"
    );
}

#[tokio::test]
async fn call_rejected_when_flag_off() {
    let server = disabled_server();
    let err = server
        .dispatch_call(call_search(json!({})))
        .await
        .expect_err("call must be rejected when flag off");
    assert!(format!("{err:?}").contains("disabled"));
}

// ── Flag-on: returns refs (NO body field present) ───────────────────────────

#[tokio::test]
async fn returns_items_array_when_flag_on() {
    let server = enabled_server();
    let resp = server.dispatch_call(call_search(json!({}))).await.expect("succeeds");
    assert!(resp["items"].is_array());
}

#[tokio::test]
async fn response_items_carry_no_body_field() {
    let server = enabled_server();
    let resp = server.dispatch_call(call_search(json!({}))).await.expect("succeeds");
    let items = resp["items"].as_array().expect("items array");
    for item in items {
        assert!(
            item.get("body").is_none(),
            "progressive disclosure violation: ref {item} carries body"
        );
    }
}

// ── Verb filter ────────────────────────────────────────────────────────────

#[tokio::test]
async fn verb_filter_includes_only_matching_verb() {
    let server = enabled_server();
    let resp = server
        .dispatch_call(call_search(json!({ "verb": "diagnose" })))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    assert!(!items.is_empty(), "expected at least one diagnose-tagged item");
    for item in items {
        assert_eq!(item["verb"].as_str(), Some("diagnose"));
    }
}

// ── Subject-root filter ────────────────────────────────────────────────────

#[tokio::test]
async fn subject_root_filter_includes_only_matching_root() {
    let server = enabled_server();
    let resp = server
        .dispatch_call(call_search(json!({ "subject_root": "review" })))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    assert!(!items.is_empty(), "expected items under review.*");
    for item in items {
        let subj = item["subject"].as_str().unwrap_or("");
        assert!(subj.starts_with("review."), "subject must start with review.; got: {subj}");
    }
}

// ── Limit ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn limit_caps_result_count() {
    let server = enabled_server();
    let resp = server
        .dispatch_call(call_search(json!({ "limit": 2 })))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    assert!(items.len() <= 2);
}

// ── Edge: empty result returns items: [] not error ─────────────────────────

#[tokio::test]
async fn empty_filter_match_returns_empty_array_not_error() {
    let server = enabled_server();
    let resp = server
        .dispatch_call(call_search(json!({ "verb": "compose" })))
        .await
        .expect("empty result is OK, not error");
    let items = resp["items"].as_array().expect("items array present");
    assert!(items.is_empty(), "expected empty list; got: {items:?}");
}

// ── Source filter (SPEC §5.3) ──────────────────────────────────────────────

fn mixed_source_server() -> FlowgateServer {
    let discovery = Arc::new(InMemoryDiscoveryIndex::new(vec![
        fixture_item_with_source("review.style.house-voice", "review", "config"),
        fixture_item_with_source("review.editorial.checklist", "review", "config"),
        fixture_item_with_source(
            "debug.repro.standard",
            "diagnose",
            "git+https://github.com/org/skills@abc123",
        ),
        fixture_item_with_source(
            "authoring.skill.writing-rubric",
            "review",
            "git+https://github.com/org/skills@abc123",
        ),
    ]));
    FlowgateServer::new(build_runtime())
        .with_discovery(discovery)
        .with_skills_search(true)
}

#[tokio::test]
async fn source_filter_config_returns_only_config_declared_fragments() {
    let server = mixed_source_server();
    let resp = server
        .dispatch_call(call_search(json!({ "source": "config" })))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    assert!(!items.is_empty(), "expected config-declared fragments");
    for item in items {
        assert_eq!(
            item["source"].as_str(),
            Some("config"),
            "source filter must exclude non-config items; got: {item}"
        );
    }
}

#[tokio::test]
async fn source_filter_git_url_returns_only_matching_ingested_fragments() {
    let server = mixed_source_server();
    let resp = server
        .dispatch_call(call_search(json!({
            "source": "git+https://github.com/org/skills@abc123"
        })))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    assert!(!items.is_empty(), "expected git-ingested fragments");
    for item in items {
        assert_eq!(
            item["source"].as_str(),
            Some("git+https://github.com/org/skills@abc123")
        );
    }
}

#[tokio::test]
async fn source_filter_absent_returns_all_sources() {
    let server = mixed_source_server();
    let resp = server.dispatch_call(call_search(json!({}))).await.expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    let sources: std::collections::HashSet<&str> = items
        .iter()
        .filter_map(|i| i["source"].as_str())
        .collect();
    assert!(sources.contains("config"), "missing config items: {sources:?}");
    assert!(
        sources.contains("git+https://github.com/org/skills@abc123"),
        "missing git items: {sources:?}"
    );
}

#[tokio::test]
async fn source_filter_unmatched_returns_empty_not_error() {
    let server = mixed_source_server();
    let resp = server
        .dispatch_call(call_search(json!({ "source": "git+https://other/repo@deadbeef" })))
        .await
        .expect("unmatched filter is OK, not error");
    let items = resp["items"].as_array().expect("items array present");
    assert!(items.is_empty(), "expected empty list; got: {items:?}");
}
