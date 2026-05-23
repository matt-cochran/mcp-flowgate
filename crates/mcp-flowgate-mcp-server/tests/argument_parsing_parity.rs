//! Parity tests for per-tool argument parsing.
//!
//! Locks down the runtime-observable behavior of each tool's argument
//! extraction layer so the upcoming schemars / typed-args refactor can't
//! quietly regress:
//!
//! 1. Required-field errors return the exact "<field> is required" message
//!    the current handlers produce (callers and audit consumers may key on
//!    these).
//! 2. Lenient defaults are preserved — fields the current handlers treat
//!    as optional (e.g. `workflow.start`'s `definitionId`, `gateway.search`'s
//!    `query`/`limit`/`kind`, `workflow.submit`'s `arguments`) must keep
//!    falling through to the runtime/discovery layer without a parse error.
//! 3. Unknown tool names route through the same `invalid_params` path with
//!    the same message.
//!
//! Tests go through `FlowgateServer::dispatch_call`, which is the same
//! dispatch table `ServerHandler::call_tool` uses minus the transport
//! plumbing.

use std::sync::Arc;

use async_trait::async_trait;
use mcp_flowgate_core::audit::NullAuditSink;
use mcp_flowgate_core::discovery::{
    DiscoveryItem, DiscoveryKind, DiscoveryLink, InMemoryDiscoveryIndex,
};
use mcp_flowgate_core::error::ExecutorError;
use mcp_flowgate_core::guards::DefaultGuardEvaluator;
use mcp_flowgate_core::model::{ExecuteRequest, ExecuteResult};
use mcp_flowgate_core::ports::{Executor, ExecutorRegistry};
use mcp_flowgate_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use mcp_flowgate_core::WorkflowRuntime;
use mcp_flowgate_mcp_server::{
    FlowgateServer, TOOL_DESCRIBE, TOOL_EXPLAIN, TOOL_GET, TOOL_HOME, TOOL_SEARCH, TOOL_START,
    TOOL_SUBMIT,
};
use rmcp::model::{CallToolRequestParams, ErrorCode, JsonObject};
use rmcp::ErrorData as McpError;
use serde_json::{json, Value};

struct InertExecutors;
#[async_trait]
impl Executor for InertExecutors {
    async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult::default())
    }
}
impl ExecutorRegistry for InertExecutors {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        None
    }
}

fn build_runtime() -> WorkflowRuntime {
    // Empty definitions: any `workflow.start` falls through to a runtime
    // error of "workflow definition '...' not found". That's exactly what
    // lets us tell "parse succeeded but runtime rejected" apart from
    // "handler rejected before reaching the runtime."
    WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::default()),
        Arc::new(InMemoryWorkflowStore::default()),
        Arc::new(InertExecutors),
        Arc::new(DefaultGuardEvaluator::new()),
        Arc::new(NullAuditSink),
    )
}

fn build_discovery() -> Arc<InMemoryDiscoveryIndex> {
    Arc::new(InMemoryDiscoveryIndex::new(vec![
        DiscoveryItem {
            id: "wf.alpha".into(),
            kind: DiscoveryKind::Workflow,
            title: "Alpha workflow".into(),
            description: "alpha description".into(),
            tags: vec!["alpha".into()],
            examples: vec![],
            aliases: vec![],
            text: "alpha text".into(),
            links: vec![DiscoveryLink {
                rel: "start".into(),
                title: None,
                description: None,
                method: "workflow.start".into(),
                args: json!({ "definitionId": "wf.alpha", "input": {} }),
                input_schema: None,
            }],
            verb: None,
            body: None,
        },
        DiscoveryItem {
            id: "cap.beta".into(),
            kind: DiscoveryKind::Capability,
            title: "Beta capability".into(),
            description: "beta description".into(),
            tags: vec![],
            examples: vec![],
            aliases: vec![],
            text: "beta text".into(),
            links: vec![],
            verb: None,
            body: None,
        },
    ]))
}

fn build_server() -> FlowgateServer {
    FlowgateServer::new(build_runtime()).with_discovery(build_discovery())
}

fn call_args(name: &'static str, args: Value) -> CallToolRequestParams {
    let map: JsonObject = args
        .as_object()
        .cloned()
        .expect("test args must be an object");
    CallToolRequestParams::new(name).with_arguments(map)
}

async fn dispatch(
    server: &FlowgateServer,
    name: &'static str,
    args: Value,
) -> Result<Value, McpError> {
    server.dispatch_call(call_args(name, args)).await
}

// ---------- gateway.home --------------------------------------------------

#[tokio::test]
async fn home_returns_home_value_with_links() {
    let server = build_server();
    let resp = dispatch(&server, TOOL_HOME, json!({})).await.unwrap();
    assert!(
        resp.get("links").and_then(Value::as_array).is_some(),
        "home response must include `links`: {resp}"
    );
}

#[tokio::test]
async fn home_ignores_extra_args() {
    let server = build_server();
    let resp = dispatch(&server, TOOL_HOME, json!({ "stray": "ignored" }))
        .await
        .expect("home tolerates extra args");
    assert!(resp.get("links").is_some());
}

// ---------- gateway.search ------------------------------------------------

#[tokio::test]
async fn search_defaults_query_to_empty_string() {
    // Schema says `query` is required, but the current handler accepts a
    // missing one and defaults to "" — the runtime treats empty as
    // "match all". Refactor must preserve this lenient default.
    let server = build_server();
    let resp = dispatch(&server, TOOL_SEARCH, json!({})).await.unwrap();
    assert_eq!(resp["query"], json!(""));
    assert_eq!(resp["kind"], Value::Null);
    let items = resp["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2, "default search returns all indexed items");
}

#[tokio::test]
async fn search_default_limit_is_ten() {
    let server = build_server();
    let resp = dispatch(&server, TOOL_SEARCH, json!({ "query": "" }))
        .await
        .unwrap();
    // Two items in the index; default limit of 10 doesn't truncate.
    assert_eq!(resp["items"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn search_respects_explicit_limit() {
    let server = build_server();
    let resp = dispatch(&server, TOOL_SEARCH, json!({ "query": "", "limit": 1 }))
        .await
        .unwrap();
    assert_eq!(resp["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn search_kind_unknown_string_is_silently_ignored() {
    // Current parse_kind returns None for unrecognized values, which the
    // runtime then treats as "no filter" — equivalent to omitting `kind`.
    // The published schema would reject this, but the runtime doesn't
    // validate it. Refactor must keep accepting unknown kinds.
    let server = build_server();
    let resp = dispatch(
        &server,
        TOOL_SEARCH,
        json!({ "query": "", "kind": "garbage" }),
    )
    .await
    .unwrap();
    assert_eq!(resp["kind"], Value::Null);
    assert_eq!(resp["items"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn search_kind_workflow_filters_to_workflows_only() {
    let server = build_server();
    let resp = dispatch(
        &server,
        TOOL_SEARCH,
        json!({ "query": "", "kind": "workflow" }),
    )
    .await
    .unwrap();
    assert_eq!(resp["kind"], json!("workflow"));
    let items = resp["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["item"]["id"], json!("wf.alpha"));
}

// ---------- gateway.describe ---------------------------------------------

#[tokio::test]
async fn describe_without_id_returns_required_error() {
    let server = build_server();
    let err = dispatch(&server, TOOL_DESCRIBE, json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
    assert!(
        err.message.contains("id is required"),
        "expected 'id is required', got: {}",
        err.message
    );
}

#[tokio::test]
async fn describe_with_known_id_returns_item() {
    let server = build_server();
    let resp = dispatch(&server, TOOL_DESCRIBE, json!({ "id": "wf.alpha" }))
        .await
        .unwrap();
    assert_eq!(resp["id"], json!("wf.alpha"));
    assert_eq!(resp["item"]["id"], json!("wf.alpha"));
}

#[tokio::test]
async fn describe_with_unknown_id_returns_null_item() {
    let server = build_server();
    let resp = dispatch(&server, TOOL_DESCRIBE, json!({ "id": "nope" }))
        .await
        .unwrap();
    assert_eq!(resp["item"], Value::Null);
}

// ---------- workflow.start -----------------------------------------------

#[tokio::test]
async fn start_without_definition_id_defaults_to_proxy_default() {
    // Schema says definitionId is required; runtime accepts missing and
    // falls back to `proxy_default`. With no proxy definition registered
    // the runtime error names `proxy_default` — confirming the default
    // landed before the runtime call, not "definitionId is required".
    let server = build_server();
    let err = dispatch(&server, TOOL_START, json!({})).await.unwrap_err();
    assert!(
        !err.message.contains("is required"),
        "start should not raise a parse-level required error: {}",
        err.message
    );
    assert!(
        err.message.contains("proxy_default"),
        "expected fallback to proxy_default, got: {}",
        err.message
    );
}

#[tokio::test]
async fn start_with_explicit_definition_id_passes_through() {
    let server = build_server();
    let err = dispatch(
        &server,
        TOOL_START,
        json!({ "definitionId": "explicit.id", "input": { "k": "v" } }),
    )
    .await
    .unwrap_err();
    assert!(
        err.message.contains("explicit.id"),
        "expected runtime error to name 'explicit.id', got: {}",
        err.message
    );
}

#[tokio::test]
async fn start_without_input_defaults_to_empty_object() {
    // Schema marks `input` required; runtime accepts missing and supplies
    // `{}`. The handler doesn't return "input is required"; it falls
    // through to the same runtime error as the with-input case.
    let server = build_server();
    let err = dispatch(&server, TOOL_START, json!({ "definitionId": "x" }))
        .await
        .unwrap_err();
    assert!(
        !err.message.contains("input is required"),
        "input should default to {{}}, got: {}",
        err.message
    );
    assert!(err.message.contains('x'));
}

// ---------- workflow.get -------------------------------------------------

#[tokio::test]
async fn get_without_workflow_id_returns_required_error() {
    let server = build_server();
    let err = dispatch(&server, TOOL_GET, json!({})).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
    assert!(
        err.message.contains("workflowId is required"),
        "expected 'workflowId is required', got: {}",
        err.message
    );
}

#[tokio::test]
async fn get_with_workflow_id_passes_through_to_runtime() {
    let server = build_server();
    let err = dispatch(&server, TOOL_GET, json!({ "workflowId": "wf-1" }))
        .await
        .unwrap_err();
    assert!(
        !err.message.contains("is required"),
        "should not raise a required-field error: {}",
        err.message
    );
    assert!(err.message.contains("wf-1"));
}

// ---------- workflow.submit ----------------------------------------------

#[tokio::test]
async fn submit_without_workflow_id_returns_required_error() {
    let server = build_server();
    let err = dispatch(&server, TOOL_SUBMIT, json!({})).await.unwrap_err();
    assert!(
        err.message.contains("workflowId is required"),
        "got: {}",
        err.message
    );
}

#[tokio::test]
async fn submit_without_expected_version_returns_required_error() {
    let server = build_server();
    let err = dispatch(&server, TOOL_SUBMIT, json!({ "workflowId": "x" }))
        .await
        .unwrap_err();
    assert!(
        err.message.contains("expectedVersion is required"),
        "got: {}",
        err.message
    );
}

#[tokio::test]
async fn submit_without_transition_returns_required_error() {
    let server = build_server();
    let err = dispatch(
        &server,
        TOOL_SUBMIT,
        json!({ "workflowId": "x", "expectedVersion": 0 }),
    )
    .await
    .unwrap_err();
    assert!(
        err.message.contains("transition is required"),
        "got: {}",
        err.message
    );
}

#[tokio::test]
async fn submit_without_arguments_defaults_to_empty_object() {
    // Schema marks `arguments` required; runtime accepts missing and uses
    // `{}`. Handler must not return "arguments is required".
    let server = build_server();
    let err = dispatch(
        &server,
        TOOL_SUBMIT,
        json!({ "workflowId": "x", "expectedVersion": 0, "transition": "t" }),
    )
    .await
    .unwrap_err();
    assert!(
        !err.message.contains("is required"),
        "arguments should default to {{}}: {}",
        err.message
    );
    assert!(err.message.contains('x'));
}

// ---------- workflow.explain ---------------------------------------------

#[tokio::test]
async fn explain_without_workflow_id_returns_required_error() {
    let server = build_server();
    let err = dispatch(&server, TOOL_EXPLAIN, json!({}))
        .await
        .unwrap_err();
    assert!(
        err.message.contains("workflowId is required"),
        "got: {}",
        err.message
    );
}

#[tokio::test]
async fn explain_without_transition_returns_required_error() {
    let server = build_server();
    let err = dispatch(&server, TOOL_EXPLAIN, json!({ "workflowId": "x" }))
        .await
        .unwrap_err();
    assert!(
        err.message.contains("transition is required"),
        "got: {}",
        err.message
    );
}

#[tokio::test]
async fn explain_with_both_passes_through_to_runtime() {
    let server = build_server();
    let err = dispatch(
        &server,
        TOOL_EXPLAIN,
        json!({ "workflowId": "wf-x", "transition": "t" }),
    )
    .await
    .unwrap_err();
    assert!(!err.message.contains("is required"), "got: {}", err.message);
}

// ---------- unknown tool -------------------------------------------------

#[tokio::test]
async fn unknown_tool_returns_invalid_params_with_named_tool() {
    let server = build_server();
    let err = dispatch(&server, "bogus.tool", json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    assert!(
        err.message.contains("bogus.tool"),
        "error should name the unknown tool: {}",
        err.message
    );
}
