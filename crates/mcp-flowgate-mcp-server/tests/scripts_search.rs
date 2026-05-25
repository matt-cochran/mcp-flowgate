//! SPEC §22 — `gateway.scripts.search` MCP tool. Authoring-time only;
//! advertised conditionally; returns refs only (progressive disclosure).
//! Mirror of the skills_search test file with kind-specific assertions.

use std::sync::Arc;

use mcp_flowgate_core::audit::{AuditSink, NullAuditSink};
use mcp_flowgate_core::discovery::{
    DiscoveryItem, DiscoveryKind, DiscoveryLink, InMemoryDiscoveryIndex,
};
use mcp_flowgate_core::guards::DefaultGuardEvaluator;
use mcp_flowgate_core::ports::ExecutorRegistry;
use mcp_flowgate_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use mcp_flowgate_core::WorkflowRuntime;
use mcp_flowgate_mcp_server::{FlowgateServer, TOOL_SCRIPTS_SEARCH};
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

fn script_item(subject: &str, verb: &str, source: &str) -> DiscoveryItem {
    DiscoveryItem {
        id: subject.into(),
        kind: DiscoveryKind::Script,
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
        body: Some("script body the test must never see".into()),
        source: Some(source.into()),
    }
}

fn build_discovery() -> Arc<InMemoryDiscoveryIndex> {
    Arc::new(InMemoryDiscoveryIndex::new(vec![
        script_item("build.cargo.release", "build", "config"),
        script_item("build.cargo.workspace", "build", "config"),
        script_item("test.cargo.workspace", "test", "config"),
        script_item(
            "lint.rust.clippy-strict",
            "lint",
            "cognitive-architectures",
        ),
    ]))
}

fn enabled_server() -> FlowgateServer {
    FlowgateServer::new(build_runtime())
        .with_discovery(build_discovery())
        .with_scripts_search(true)
}

fn disabled_server() -> FlowgateServer {
    FlowgateServer::new(build_runtime()).with_discovery(build_discovery())
}

fn call_search(args: Value) -> CallToolRequestParams {
    let m: JsonObject = args.as_object().cloned().expect("object");
    CallToolRequestParams::new(TOOL_SCRIPTS_SEARCH).with_arguments(m)
}

// ── Flag off: tool absent from list_tools + call rejected ─────────────────

#[tokio::test]
async fn tool_not_advertised_when_flag_off() {
    use mcp_flowgate_mcp_server::tool_definitions;
    let _server = disabled_server();
    let names: Vec<String> = tool_definitions()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    assert!(
        !names.contains(&TOOL_SCRIPTS_SEARCH.to_string()),
        "scripts.search must NOT appear in default tool list; got: {names:?}"
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

// ── Flag on: returns refs (NO body) ───────────────────────────────────────

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
        .dispatch_call(call_search(json!({ "verb": "build" })))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    assert!(!items.is_empty(), "expected build-tagged items");
    for item in items {
        assert_eq!(item["verb"].as_str(), Some("build"));
    }
}

// ── Subject-root filter ────────────────────────────────────────────────────

#[tokio::test]
async fn subject_root_filter_includes_only_matching_root() {
    let server = enabled_server();
    let resp = server
        .dispatch_call(call_search(json!({ "subject_root": "build" })))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    assert!(!items.is_empty());
    for item in items {
        let subj = item["subject"].as_str().unwrap_or("");
        assert!(subj.starts_with("build."), "got: {subj}");
    }
}

// ── Source filter ──────────────────────────────────────────────────────────

#[tokio::test]
async fn source_filter_matches_exact_source() {
    let server = enabled_server();
    let resp = server
        .dispatch_call(call_search(json!({ "source": "cognitive-architectures" })))
        .await
        .expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    assert!(!items.is_empty());
    for item in items {
        assert_eq!(item["source"].as_str(), Some("cognitive-architectures"));
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

// ── Empty match returns [] not error ──────────────────────────────────────

#[tokio::test]
async fn empty_filter_match_returns_empty_array() {
    let server = enabled_server();
    let resp = server
        .dispatch_call(call_search(json!({ "verb": "compose" })))
        .await
        .expect("empty result is OK");
    let items = resp["items"].as_array().expect("items array present");
    assert!(items.is_empty());
}

// ── Listings exclude DiscoveryKind::Guidance items ────────────────────────

#[tokio::test]
async fn scripts_search_excludes_guidance_items() {
    // Add a guidance item to the index alongside scripts; scripts.search
    // must NOT return it (the listing is kind-filtered).
    let mut items: Vec<DiscoveryItem> = vec![
        script_item("build.cargo.release", "build", "config"),
    ];
    items.push(DiscoveryItem {
        id: "review.code.adversarial".into(),
        kind: DiscoveryKind::Guidance,
        title: "guidance".into(),
        description: String::new(),
        tags: vec![],
        examples: vec![],
        aliases: vec![],
        text: String::new(),
        links: vec![],
        verb: Some("review".into()),
        body: Some("should NOT leak through scripts.search".into()),
        source: Some("config".into()),
    });
    let discovery = Arc::new(InMemoryDiscoveryIndex::new(items));
    let server = FlowgateServer::new(build_runtime())
        .with_discovery(discovery)
        .with_scripts_search(true);

    let resp = server.dispatch_call(call_search(json!({}))).await.expect("succeeds");
    let items = resp["items"].as_array().unwrap();
    for item in items {
        let subj = item["subject"].as_str().unwrap_or("");
        assert!(
            !subj.starts_with("review."),
            "scripts.search must NOT return guidance items; got: {subj}"
        );
    }
}
