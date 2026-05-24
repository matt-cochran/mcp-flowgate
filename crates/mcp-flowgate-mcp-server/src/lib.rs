//! MCP server tool surface for mcp-flowgate.
//!
//! The tool list is stable across configs (invariant 9). It splits into two
//! HATEOAS layers:
//!
//! - **Gateway layer** — `gateway.home`, `gateway.search`, `gateway.describe`
//!   help a model find the right workflow or capability to start.
//! - **Workflow layer** — `workflow.start`, `workflow.get`, `workflow.submit`,
//!   `workflow.explain` drive a single workflow forward through links in
//!   each response.
//!
//! Tool input schemas and per-handler argument parsing share one Rust source
//! of truth: a typed `*Args` struct per tool (see the **Argument structs**
//! section below). `schemars` derives the published JSON Schema from those
//! structs; `serde` deserializes incoming arguments into the same shape.
//! Whatever divergence remains between "what the schema says is required" and
//! "what the runtime tolerates as missing" is encoded explicitly: lenient
//! fields stay `Option<T>` and are unwrapped with handler-side defaults,
//! while strict fields stay `Option<T>` with an explicit `is required`
//! check so the error message matches what callers (and audit consumers)
//! already see today.

use std::borrow::Cow;
use std::sync::Arc;

use mcp_flowgate_core::audit::AuditEvent;
use mcp_flowgate_core::discovery::{
    DiscoveryIndex, DiscoveryKind, InMemoryDiscoveryIndex, SearchRequest,
};
use mcp_flowgate_core::model::{GetWorkflow, Principal, StartWorkflow, SubmitTransition};
use mcp_flowgate_core::runtime::WorkflowRuntime;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Implementation, InitializeRequestParams,
    InitializeResult, JsonObject, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use schemars::gen::{SchemaGenerator, SchemaSettings};
use schemars::schema::{InstanceType, Schema, SchemaObject};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

pub const TOOL_HOME: &str = "gateway.home";
pub const TOOL_SEARCH: &str = "gateway.search";
pub const TOOL_DESCRIBE: &str = "gateway.describe";
pub const TOOL_START: &str = "workflow.start";
pub const TOOL_GET: &str = "workflow.get";
pub const TOOL_SUBMIT: &str = "workflow.submit";
pub const TOOL_EXPLAIN: &str = "workflow.explain";

/// The complete set of MCP tool names this server exposes. Stable across
/// configs by design — see invariant 9 in the README.
pub const STABLE_TOOL_NAMES: &[&str] = &[
    TOOL_HOME,
    TOOL_SEARCH,
    TOOL_DESCRIBE,
    TOOL_START,
    TOOL_GET,
    TOOL_SUBMIT,
    TOOL_EXPLAIN,
];

#[derive(Clone)]
pub struct FlowgateServer {
    runtime: WorkflowRuntime,
    discovery: Arc<dyn DiscoveryIndex>,
    server_name: String,
    server_version: String,
}

impl FlowgateServer {
    /// Build a server with a default empty in-memory discovery index. The
    /// gateway.* tools still work but return no items.
    pub fn new(runtime: WorkflowRuntime) -> Self {
        Self {
            runtime,
            discovery: Arc::new(InMemoryDiscoveryIndex::default()),
            server_name: "mcp-flowgate".to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    pub fn with_discovery(mut self, discovery: Arc<dyn DiscoveryIndex>) -> Self {
        self.discovery = discovery;
        self
    }

    pub fn with_identity(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.server_name = name.into();
        self.server_version = version.into();
        self
    }

    fn principal(_request: &CallToolRequestParams) -> Principal {
        // MVP: derive principal from transport later (auth headers, identity
        // claims). For now, every caller is the same anonymous principal.
        Principal::anonymous()
    }
}

impl ServerHandler for FlowgateServer {
    fn get_info(&self) -> ServerInfo {
        let mut server_info =
            Implementation::new(self.server_name.clone(), self.server_version.clone());
        server_info.title = Some("mcp-flowgate".to_string());
        server_info.description =
            Some("Configurable MCP gateway with HATEOAS workflow governance".to_string());

        let mut info = InitializeResult::default();
        info.protocol_version = ProtocolVersion::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = server_info;
        info.instructions = Some(instructions().to_string());
        info
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        let _ = self
            .runtime
            .audit()
            .record(AuditEvent::new("server.initialized").with_payload(json!({
                "name": self.server_name,
                "version": self.server_version,
            })))
            .await;
        Ok(self.get_info())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tool_definitions()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.dispatch_call(request)
            .await
            .map(CallToolResult::structured)
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tool_definitions().into_iter().find(|t| t.name == name)
    }

    async fn on_initialized(&self, _context: NotificationContext<RoleServer>) {
        tracing::info!("mcp-flowgate client initialized");
    }
}

impl FlowgateServer {
    /// Transport-free entry point that mirrors what `ServerHandler::call_tool`
    /// does, minus the `CallToolResult` wrapping. Lets parity tests assert on
    /// per-tool argument parsing and response shape without spinning up an
    /// rmcp transport. Behaviorally identical to `call_tool` — same dispatch
    /// table, same error mapping.
    pub async fn dispatch_call(&self, request: CallToolRequestParams) -> Result<Value, McpError> {
        let principal = Self::principal(&request);
        let args: Value = request
            .arguments
            .as_ref()
            .map(|m| Value::Object(m.clone()))
            .unwrap_or_else(|| json!({}));

        let result = match request.name.as_ref() {
            TOOL_HOME => self.handle_home().await,
            TOOL_SEARCH => self.handle_search(args).await,
            TOOL_DESCRIBE => self.handle_describe(args).await,
            TOOL_START => self.handle_start(args, principal).await,
            TOOL_GET => self.handle_get(args, principal).await,
            TOOL_SUBMIT => self.handle_submit(args, principal).await,
            TOOL_EXPLAIN => self.handle_explain(args).await,
            other => {
                return Err(McpError::invalid_params(
                    format!("Unknown tool '{other}'. Use list_tools to discover."),
                    None,
                ));
            }
        };

        result.map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    async fn handle_home(&self) -> anyhow::Result<Value> {
        self.discovery.home().await
    }

    async fn handle_search(&self, args: Value) -> anyhow::Result<Value> {
        let parsed: SearchArgs = serde_json::from_value(args)?;
        let query = parsed.query.unwrap_or_default();
        let kind = parsed.kind.as_deref().and_then(parse_kind);
        let limit = parsed.limit as usize;

        let hits = self
            .discovery
            .search(SearchRequest {
                query: query.clone(),
                kind,
                limit,
            })
            .await?;

        Ok(json!({
            "query": query,
            "kind": kind.map(|k| k.as_str()),
            "items": hits,
            "links": [
                { "rel": "home", "method": "gateway.home", "args": {} }
            ]
        }))
    }

    async fn handle_describe(&self, args: Value) -> anyhow::Result<Value> {
        let parsed: DescribeArgs = serde_json::from_value(args)?;
        let id = parsed.id.ok_or_else(|| anyhow::anyhow!("id is required"))?;
        let item = self.discovery.describe(&id).await?;
        Ok(json!({
            "id": id,
            "item": item,
            "links": [
                { "rel": "home", "method": "gateway.home", "args": {} },
                { "rel": "search", "method": "gateway.search", "args": { "query": "" } }
            ]
        }))
    }

    async fn handle_start(&self, args: Value, principal: Principal) -> anyhow::Result<Value> {
        let parsed: StartArgs = serde_json::from_value(args)?;
        let definition_id = parsed
            .definition_id
            .unwrap_or_else(|| mcp_flowgate_core::DEFAULT_PROXY_WORKFLOW_ID.to_string());
        let input = parsed.input.unwrap_or_else(|| json!({}));

        self.runtime
            .start(StartWorkflow {
                definition_id,
                input,
                principal,
            })
            .await
    }

    async fn handle_get(&self, args: Value, principal: Principal) -> anyhow::Result<Value> {
        let parsed: GetArgs = serde_json::from_value(args)?;
        let workflow_id = parsed
            .workflow_id
            .ok_or_else(|| anyhow::anyhow!("workflowId is required"))?;

        self.runtime
            .get(GetWorkflow {
                workflow_id,
                principal,
            })
            .await
    }

    async fn handle_submit(&self, args: Value, principal: Principal) -> anyhow::Result<Value> {
        let parsed: SubmitArgs = serde_json::from_value(args)?;
        let workflow_id = parsed
            .workflow_id
            .ok_or_else(|| anyhow::anyhow!("workflowId is required"))?;
        let expected_version = parsed
            .expected_version
            .ok_or_else(|| anyhow::anyhow!("expectedVersion is required"))?;
        let transition = parsed
            .transition
            .ok_or_else(|| anyhow::anyhow!("transition is required"))?;
        let arguments = parsed.arguments.unwrap_or_else(|| json!({}));

        self.runtime
            .submit(SubmitTransition {
                workflow_id,
                expected_version,
                transition,
                arguments,
                principal,
                summary: parsed.summary,
            })
            .await
    }

    async fn handle_explain(&self, args: Value) -> anyhow::Result<Value> {
        let parsed: ExplainArgs = serde_json::from_value(args)?;
        let workflow_id = parsed
            .workflow_id
            .ok_or_else(|| anyhow::anyhow!("workflowId is required"))?;
        let transition = parsed
            .transition
            .ok_or_else(|| anyhow::anyhow!("transition is required"))?;
        self.runtime.explain(&workflow_id, &transition).await
    }
}

fn parse_kind(s: &str) -> Option<DiscoveryKind> {
    match s {
        "workflow" => Some(DiscoveryKind::Workflow),
        "capability" => Some(DiscoveryKind::Capability),
        "connection" => Some(DiscoveryKind::Connection),
        _ => None,
    }
}

// ---------- Argument structs --------------------------------------------
//
// One `*Args` struct per tool. Both the published JSON Schema (via
// `schemars::JsonSchema`) and the per-handler argument extraction (via
// `serde::Deserialize`) come from these definitions.
//
// Required-field policy is encoded twice on purpose: the per-call required
// list passed to `schema_for_args` controls what the published schema
// advertises; the handler's `.ok_or_else(... "is required")` controls what
// the runtime rejects. They're maintained as a pair because the published
// surface and the runtime have diverged historically (some schema-required
// fields are silently defaulted by the runtime), and the parity tests fix
// that contract in place. Every field is `Option<T>` so the deserializer
// never produces serde's default missing-field error — handlers raise the
// canonical "<field> is required" message instead.
//
// Tool-specific schema shims (`integer_schema`, `object_schema`,
// `discovery_kind_schema`) override the default schemars output so the
// published schema matches what callers see today. See those functions for
// the per-field rationale.

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SearchArgs {
    query: Option<String>,
    #[schemars(schema_with = "discovery_kind_schema")]
    kind: Option<String>,
    #[serde(default = "default_limit")]
    #[schemars(schema_with = "limit_schema")]
    limit: u64,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct DescribeArgs {
    id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct StartArgs {
    definition_id: Option<String>,
    #[schemars(schema_with = "object_schema")]
    input: Option<Value>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct GetArgs {
    workflow_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SubmitArgs {
    workflow_id: Option<String>,
    #[schemars(schema_with = "integer_schema")]
    expected_version: Option<u64>,
    transition: Option<String>,
    #[schemars(schema_with = "object_schema")]
    arguments: Option<Value>,
    /// SPEC §6.3 — optional model-authored summary. Stored to
    /// `context.summary` on commit; surfaced in every response.
    summary: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ExplainArgs {
    workflow_id: Option<String>,
    transition: Option<String>,
}

fn default_limit() -> u64 {
    10
}

// ---------- per-field schema overrides ----------------------------------
//
// Schemars's default schemas for `u64`/`Option<Value>` carry extra hints
// (`format: uint64`, `minimum: 0`, `additionalProperties: true`) that the
// previous hand-written schemas didn't. These shims keep the published
// schema byte-equivalent to the pre-refactor surface.

fn integer_schema(_: &mut SchemaGenerator) -> Schema {
    SchemaObject {
        instance_type: Some(InstanceType::Integer.into()),
        ..Default::default()
    }
    .into()
}

fn limit_schema(gen: &mut SchemaGenerator) -> Schema {
    let mut schema = match integer_schema(gen) {
        Schema::Object(o) => o,
        Schema::Bool(_) => unreachable!("integer_schema always returns Schema::Object"),
    };
    schema.metadata().default = Some(json!(default_limit()));
    schema.into()
}

fn object_schema(_: &mut SchemaGenerator) -> Schema {
    SchemaObject {
        instance_type: Some(InstanceType::Object.into()),
        ..Default::default()
    }
    .into()
}

fn discovery_kind_schema(_: &mut SchemaGenerator) -> Schema {
    SchemaObject {
        instance_type: Some(InstanceType::String.into()),
        enum_values: Some(vec![
            json!("workflow"),
            json!("capability"),
            json!("connection"),
        ]),
        ..Default::default()
    }
    .into()
}

// ---------- tool table ---------------------------------------------------

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
    ]
}

/// Build the rmcp `Tool.input_schema` for a typed `*Args` struct. The
/// `required` list is supplied explicitly because some schema-required
/// fields are silently defaulted by the runtime — see the args-struct
/// comment block above.
fn schema_for_args<T: JsonSchema>(required: &[&'static str]) -> Arc<JsonObject> {
    let generator = SchemaSettings::draft07()
        .with(|s| {
            s.option_add_null_type = false;
            s.inline_subschemas = true;
            s.meta_schema = None;
        })
        .into_generator();
    let root = generator.into_root_schema_for::<T>();
    let mut value =
        serde_json::to_value(&root).expect("schemars produces JSON-serializable schema");
    let obj = value
        .as_object_mut()
        .expect("root schema is always an object");
    obj.remove("$schema");
    obj.remove("title");
    obj.remove("definitions");
    obj.remove("description");

    if let Some(Value::Object(props)) = obj.get_mut("properties") {
        for (_, v) in props.iter_mut() {
            if let Value::Object(field) = v {
                // Strip schemars hints the legacy hand-written schemas
                // didn't carry: numeric `format`/`minimum`, the recursive
                // `additionalProperties: true` schemars stamps on
                // `Map<String, Value>`, and field doc-comments.
                field.remove("format");
                field.remove("minimum");
                field.remove("additionalProperties");
                field.remove("description");
            }
        }
    }

    if required.is_empty() {
        obj.remove("required");
    } else {
        obj.insert("required".into(), json!(required));
    }
    obj.insert("additionalProperties".into(), Value::Bool(false));
    Arc::new(value.as_object().cloned().expect("still an object"))
}

/// Hand-built schema for `gateway.home`, which takes no arguments. Going
/// through schemars for a struct with zero fields works but emits an empty
/// `properties` map and no `required` key — same result, but a one-liner
/// here is cleaner than spelling out a `struct HomeArgs;` derive just to
/// produce `{}`.
fn empty_object_schema() -> Arc<JsonObject> {
    let mut obj = serde_json::Map::new();
    obj.insert("type".into(), Value::String("object".into()));
    obj.insert("properties".into(), Value::Object(serde_json::Map::new()));
    obj.insert("additionalProperties".into(), Value::Bool(false));
    Arc::new(obj)
}

fn instructions() -> &'static str {
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
