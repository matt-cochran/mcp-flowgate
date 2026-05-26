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
//! Module layout:
//! - `args` — argument structs + JSON Schema helpers (typed `*Args` per tool).
//! - `tools` — tool-list construction + free-form helpers (`parse_kind`,
//!   `instructions`).
//! - `handlers` — per-tool handler bodies (sibling `impl FlowgateServer`).
//!
//! Tool input schemas and per-handler argument parsing share one Rust source
//! of truth: the typed `*Args` structs in `args`. `schemars` derives the
//! published JSON Schema from those structs; `serde` deserializes incoming
//! arguments into the same shape. Whatever divergence remains between "what
//! the schema says is required" and "what the runtime tolerates as missing"
//! is encoded explicitly: lenient fields stay `Option<T>` and are unwrapped
//! with handler-side defaults, while strict fields stay `Option<T>` with an
//! explicit `is required` check so the error message matches what callers
//! (and audit consumers) already see today.

mod args;
mod handlers;
mod tools;

use std::sync::Arc;

use mcp_flowgate_core::audit::AuditEvent;
use mcp_flowgate_core::discovery::{DiscoveryIndex, InMemoryDiscoveryIndex};
use mcp_flowgate_core::model::Principal;
use mcp_flowgate_core::runtime::WorkflowRuntime;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Implementation, InitializeRequestParams,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use serde_json::{json, Value};

pub use tools::{scripts_search_tool_definition, skills_search_tool_definition, tool_definitions};

pub const TOOL_HOME: &str = "gateway.home";
pub const TOOL_SEARCH: &str = "gateway.search";
pub const TOOL_DESCRIBE: &str = "gateway.describe";
pub const TOOL_START: &str = "workflow.start";
pub const TOOL_GET: &str = "workflow.get";
pub const TOOL_SUBMIT: &str = "workflow.submit";
pub const TOOL_EXPLAIN: &str = "workflow.explain";
/// SPEC §17.6 — authoring-time skills discovery. Tool is only advertised when
/// `FlowgateServer::with_skills_search(true)` is set; default off so runtime
/// workflows use the push-not-pull guidance surface (§5.4).
pub const TOOL_SKILLS_SEARCH: &str = "gateway.skills.search";
/// SPEC §22 — authoring-time scripts discovery. Mirror of
/// `gateway.skills.search` for the scripts library. Advertised only when
/// `FlowgateServer::with_scripts_search(true)` is set; default off (same
/// reasoning as skills).
pub const TOOL_SCRIPTS_SEARCH: &str = "gateway.scripts.search";
/// SPEC §30 — lexicon search (always advertised; lexicon is a runtime
/// concept used INSIDE workflows, not authoring-time-only).
pub const TOOL_LEXICON_SEARCH: &str = "gateway.lexicon.search";
/// SPEC §30 — lexicon lookup.
pub const TOOL_LEXICON_LOOKUP: &str = "gateway.lexicon.lookup";
/// SPEC §30 — lexicon define. Governance-gated; agents calling against
/// a `human-only` term get `LEXICON_DEFINE_REQUIRES_HUMAN`.
pub const TOOL_LEXICON_DEFINE: &str = "gateway.lexicon.define";

/// The complete set of MCP tool names this server exposes by default
/// (without authoring-time flags). Stable across configs by design — see
/// invariant 9 in the README.
pub const STABLE_TOOL_NAMES: &[&str] = &[
    TOOL_HOME,
    TOOL_SEARCH,
    TOOL_DESCRIBE,
    TOOL_START,
    TOOL_GET,
    TOOL_SUBMIT,
    TOOL_EXPLAIN,
    TOOL_LEXICON_SEARCH,
    TOOL_LEXICON_LOOKUP,
    TOOL_LEXICON_DEFINE,
];

#[derive(Clone)]
pub struct FlowgateServer {
    pub(crate) runtime: WorkflowRuntime,
    pub(crate) discovery: Arc<dyn DiscoveryIndex>,
    server_name: String,
    server_version: String,
    /// SPEC §5.9 — optional store that records `gateway.describe` calls per
    /// workflow + subject, consumed by the `guidance_acknowledged` guard.
    /// When `None`, describes still emit audit records but the guard cannot
    /// be satisfied (returns false).
    pub(crate) ack_store: Option<Arc<dyn mcp_flowgate_core::ports::GuidanceAcknowledgmentStore>>,
    /// SPEC §17.6 — when true, the `gateway.skills.search` tool is
    /// advertised in `list_tools`. Default false; authoring-time only.
    skills_search_enabled: bool,
    /// SPEC §22 — when true, `gateway.scripts.search` is advertised in
    /// `list_tools`. Default false; authoring-time only. Same rationale
    /// as skills_search_enabled.
    scripts_search_enabled: bool,
    /// SPEC §22 — optional store that records `gateway.describe` calls
    /// for SCRIPT subjects per workflow, consumed by the
    /// `script_acknowledged` guard. When `None`, describes still emit
    /// audit records but the guard cannot be satisfied (returns false).
    pub(crate) script_ack_store:
        Option<Arc<dyn mcp_flowgate_core::ports::ScriptAcknowledgmentStore>>,
    /// SPEC §30.5 — runtime overlay over the config-stamped lexicon.
    /// `gateway.lexicon.define` writes here; `search` / `lookup` read
    /// the union (overlay wins on collision). Survives only for the
    /// runtime's lifetime — operators persist by editing
    /// `flowgate.yaml` and reloading.
    pub(crate) lexicon_overlay:
        Arc<std::sync::RwLock<std::collections::HashMap<String, Value>>>,
    /// SPEC §30 — the config-loaded lexicon block (the persistent base).
    /// Empty when no `lexicon:` block was declared in the config.
    /// `search` / `lookup` read `lexicon_base` ∪ `lexicon_overlay`;
    /// overlay wins on collision.
    pub(crate) lexicon_base: Arc<Value>,
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
            ack_store: None,
            skills_search_enabled: false,
            scripts_search_enabled: false,
            script_ack_store: None,
            lexicon_overlay: Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            lexicon_base: Arc::new(json!({})),
        }
    }

    /// SPEC §30 — wire the persistent (config-loaded) lexicon base.
    /// Callers pass the resolved config's `lexicon:` block (or an empty
    /// object when none was declared). Runtime writes via
    /// `gateway.lexicon.define` go into a separate overlay; reads
    /// merge both.
    pub fn with_lexicon(mut self, lexicon: Value) -> Self {
        self.lexicon_base = Arc::new(lexicon);
        self
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

    /// SPEC §5.9 — wire a guidance-acknowledgment store. Required for
    /// workflows that use the `guidance_acknowledged` guard.
    pub fn with_ack_store(
        mut self,
        ack_store: Arc<dyn mcp_flowgate_core::ports::GuidanceAcknowledgmentStore>,
    ) -> Self {
        self.ack_store = Some(ack_store);
        self
    }

    /// SPEC §17.6 — enable the `gateway.skills.search` tool. Default off.
    /// Authoring-time only — the runtime guidance surface uses push-not-pull
    /// (§5.4). Enabling this for runtime workflows reintroduces the
    /// pull-discovery anti-pattern.
    pub fn with_skills_search(mut self, enabled: bool) -> Self {
        self.skills_search_enabled = enabled;
        self
    }

    /// SPEC §22 — enable the `gateway.scripts.search` tool. Default off,
    /// same authoring-time-only rationale as `with_skills_search`.
    pub fn with_scripts_search(mut self, enabled: bool) -> Self {
        self.scripts_search_enabled = enabled;
        self
    }

    /// SPEC §22 — wire a script-acknowledgment store. Required for
    /// workflows that use the `script_acknowledged` guard.
    pub fn with_script_ack_store(
        mut self,
        store: Arc<dyn mcp_flowgate_core::ports::ScriptAcknowledgmentStore>,
    ) -> Self {
        self.script_ack_store = Some(store);
        self
    }

    fn principal(_request: &CallToolRequestParams) -> Principal {
        // MVP: derive principal from transport later (auth headers, identity
        // claims). For now, every caller is the same anonymous principal.
        Principal::anonymous()
    }

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
            TOOL_DESCRIBE => self.handle_describe(args, principal.clone()).await,
            TOOL_START => self.handle_start(args, principal).await,
            TOOL_GET => self.handle_get(args, principal).await,
            TOOL_SUBMIT => self.handle_submit(args, principal).await,
            TOOL_EXPLAIN => self.handle_explain(args).await,
            TOOL_SKILLS_SEARCH => {
                if !self.skills_search_enabled {
                    return Err(McpError::invalid_params(
                        "gateway.skills.search is disabled. Enable with \
                         FlowgateServer::with_skills_search(true) — authoring-time only."
                            .to_string(),
                        None,
                    ));
                }
                self.handle_skills_search(args).await
            }
            TOOL_SCRIPTS_SEARCH => {
                if !self.scripts_search_enabled {
                    return Err(McpError::invalid_params(
                        "gateway.scripts.search is disabled. Enable with \
                         FlowgateServer::with_scripts_search(true) — authoring-time only."
                            .to_string(),
                        None,
                    ));
                }
                self.handle_scripts_search(args).await
            }
            TOOL_LEXICON_SEARCH => self.handle_lexicon_search(args).await,
            TOOL_LEXICON_LOOKUP => self.handle_lexicon_lookup(args).await,
            TOOL_LEXICON_DEFINE => self.handle_lexicon_define(args, principal).await,
            other => {
                return Err(McpError::invalid_params(
                    format!("Unknown tool '{other}'. Use list_tools to discover."),
                    None,
                ));
            }
        };

        result.map_err(|e| McpError::internal_error(e.to_string(), None))
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
        info.instructions = Some(tools::instructions().to_string());
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
        let mut tools = tool_definitions();
        if self.skills_search_enabled {
            tools.push(skills_search_tool_definition());
        }
        if self.scripts_search_enabled {
            tools.push(scripts_search_tool_definition());
        }
        Ok(ListToolsResult::with_all_items(tools))
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
