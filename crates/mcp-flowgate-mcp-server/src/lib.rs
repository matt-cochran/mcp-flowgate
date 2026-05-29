// T26 — restriction-category lint on production code only. See
// mcp-flowgate-core/src/lib.rs for the rationale.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]

//! MCP server tool surface for mcp-flowgate.
//!
//! SPEC §32 — the public MCP surface is exactly **two tools** (`flowgate.query`
//! and `flowgate.command`), stable across configs by design (README invariant
//! 9). All workflow and discovery operations are reached by varying the args,
//! not the tool name.
//!
//! Module layout:
//! - `args` — sparse argument structs (`QueryArgs`, `CommandArgs`) + JSON
//!   Schema helpers.
//! - `tools` — two-tool-list construction + `parse_kind` + `instructions`.
//! - `handlers` — per-operation handler bodies (sibling `impl FlowgateServer`)
//!   plus shape-routers `dispatch_query` / `dispatch_command`.

pub mod args;
mod handlers;
mod tools;

use handlers::{run_id_already_running, subject_needs_definition};

use std::sync::Arc;

use mcp_flowgate_core::audit::AuditEvent;
use mcp_flowgate_core::discovery::{DiscoveryIndex, InMemoryDiscoveryIndex};
use mcp_flowgate_core::embeddings::{EmbeddingProvider, NoopEmbedder};
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

/// SPEC §32 — read tool. Args dispatched by present-field shape via
/// `handlers::dispatch_query`. See SPEC §32 for the full dispatch table.
pub const TOOL_QUERY: &str = "flowgate.query";

/// SPEC §32 — write tool. Args dispatched by present-field shape via
/// `handlers::dispatch_command`. See SPEC §32 for the full dispatch table.
pub const TOOL_COMMAND: &str = "flowgate.command";

/// SPEC §32 — the public MCP surface is exactly two tools, stable
/// across configs by design (README invariant 9). All workflow and
/// discovery operations are reached by varying the args, not the tool
/// name.
pub const STABLE_TOOL_NAMES: &[&str] = &[TOOL_QUERY, TOOL_COMMAND];

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
    /// SPEC §22 — when true, `flowgate.query` with `kind: "script"` is
    /// enabled. Default false; authoring-time only. Same rationale
    /// as skills_search_enabled.
    scripts_search_enabled: bool,
    /// SPEC §32 — when true, the `flowgate.command` dispatch accepts
    /// `subject: "lexicon:<term>"` + `definition` shape (lexicon writes
    /// via MCP). Default OFF: production runtimes typically curate lexicon
    /// via the CLI or out-of-band processes. Authoring builds opt in.
    lexicon_writes_enabled: bool,
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
    /// SPEC §30.10.3 — runtime-mutable set of subject names that were
    /// detected as PENDING_DEFINITION placeholders at config-load time.
    /// Resolution handlers (link_as_alias, define_new, cancel) remove
    /// entries from this set when they resolve a subject. Cancel uses it
    /// to distinguish "is a placeholder" from "is a real entry"
    /// (SPEC §30.10.9).
    pub(crate) pending_subjects:
        Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    /// SPEC §30.10.10 — optional Tier 3 embedding backend. Defaults to
    /// `NoopEmbedder` (disabled). Set via `with_embedder(...)`. When a
    /// non-noop backend is configured, `handle_lexicon_define` computes and
    /// stores the embedding vector on each written entry, and
    /// `rank_candidates_with_embedding` fires Tier 3 in the
    /// SUBJECT_NEEDS_DEFINITION candidate response.
    pub(crate) embedder: Arc<dyn EmbeddingProvider>,
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
            lexicon_writes_enabled: false,
            script_ack_store: None,
            lexicon_overlay: Arc::new(std::sync::RwLock::new(
                std::collections::HashMap::new(),
            )),
            lexicon_base: Arc::new(json!({})),
            pending_subjects: Arc::new(std::sync::RwLock::new(
                std::collections::HashSet::new(),
            )),
            embedder: Arc::new(NoopEmbedder),
        }
    }

    /// SPEC §30.10.10 — wire an embedding backend. Default is `NoopEmbedder`
    /// (Tier 3 disabled). Pass an `Arc<HttpEmbedder>` or any custom
    /// `EmbeddingProvider` to enable semantic candidate ranking.
    pub fn with_embedder(mut self, embedder: Arc<dyn EmbeddingProvider>) -> Self {
        self.embedder = embedder;
        self
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

    /// SPEC §22 — enable scripts search via `flowgate.query` with
    /// `kind: "script"`. Default off, same authoring-time-only rationale
    /// as `with_skills_search`.
    pub fn with_scripts_search(mut self, enabled: bool) -> Self {
        self.scripts_search_enabled = enabled;
        self
    }

    /// SPEC §32 — enable lexicon-define commands via MCP. Default OFF.
    /// Mirror of the `with_skills_search` / `with_scripts_search` opt-ins.
    pub fn with_lexicon_writes(mut self, enabled: bool) -> Self {
        self.lexicon_writes_enabled = enabled;
        self
    }

    /// SPEC §30.10.3 — seed the set of pending (PENDING_DEFINITION) subjects
    /// detected at config-load time. Callers pass the list returned by
    /// `mcp_flowgate_core::lexicon::pending_subjects_from_resolved(config)`.
    /// Resolution handlers remove entries from this set; cancel uses it to
    /// distinguish bookkeeping placeholders from authored entries.
    ///
    /// The same `Arc` is shared into the embedded `WorkflowRuntime` so that
    /// the runtime's pre-start subject walk reflects resolved state immediately
    /// when a resolution handler removes an entry from the set — no config
    /// reload needed (SPEC §30.10.4, Gap 2 fix).
    ///
    /// When `subjects` is empty, the server still wires the live set into the
    /// runtime (as an empty `Some(Arc)`) so that Phase 1 (live-set check) is
    /// used and future additions to the set are observable. This is correct:
    /// a config with no pending subjects should start workflows without the
    /// snapshot fallback blocking them.
    pub fn with_pending_subjects(mut self, subjects: Vec<String>) -> Self {
        let shared: Arc<std::sync::RwLock<std::collections::HashSet<String>>> =
            Arc::new(std::sync::RwLock::new(subjects.into_iter().collect()));
        self.pending_subjects = shared.clone();
        // Share the same Arc into the runtime. WorkflowRuntime::with_pending_subjects
        // sets pending_subjects to Some(arc), switching the runtime to Phase 1
        // (live-set) subject checks (SPEC §30.10.4 Gap 2 fix).
        self.runtime = self.runtime.with_pending_subjects(shared);
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

    /// SPEC §30.10.10 — config-load embedding backfill.
    ///
    /// Walks every entry in `lexicon_base` (and the current overlay). For
    /// each entry that is missing `_embedding`, computes and stores the
    /// vector. Failures are logged as warnings and do NOT abort — backfill
    /// is best-effort.
    ///
    /// No-ops when the active embedder is `NoopEmbedder`. Callers should
    /// invoke this once after `FlowgateServer::new(...).with_lexicon(...)
    /// .with_embedder(...)` before serving requests.
    pub async fn backfill_lexicon_embeddings(&self) {
        if self.embedder.backend_name() == "noop" {
            return;
        }

        // Collect (term, entry) pairs that are missing _embedding.
        // We read base and overlay independently then merge for the full picture.
        let base_entries: Vec<(String, serde_json::Value)> = {
            self.lexicon_base
                .as_object()
                .map(|obj| obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default()
        };

        // Process base entries first.
        for (term, entry) in base_entries {
            if entry
                .get("_embedding")
                .is_some()
            {
                continue; // already has embedding
            }
            if entry.get("state").and_then(serde_json::Value::as_str)
                == Some("PENDING_DEFINITION")
            {
                continue; // skip placeholders
            }
            let definition_short = entry
                .get("definition_short")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let aliases: Vec<String> = entry
                .get("aliases")
                .and_then(serde_json::Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let text = mcp_flowgate_core::embeddings::entry_embed_text(
                &term,
                &aliases,
                definition_short,
                None,
            );
            match self.embedder.embed(&text).await {
                Ok(vec) => {
                    let mut updated = entry.clone();
                    if let Some(obj) = updated.as_object_mut() {
                        obj.insert("_embedding".to_string(), json!(vec));
                    }
                    let mut overlay = self
                        .lexicon_overlay
                        .write()
                        .expect("lexicon overlay lock poisoned");
                    // Only write to overlay if not already present there
                    // (overlay would have a more-current version).
                    overlay.entry(term.clone()).or_insert(updated);
                }
                Err(e) => {
                    tracing::warn!(
                        term = %term,
                        error = %e,
                        "backfill_lexicon_embeddings: failed to embed term '{}'; skipping",
                        term
                    );
                }
            }
        }

        // Process overlay entries (may have been added at runtime, also missing _embedding).
        let overlay_snapshot: Vec<(String, serde_json::Value)> = {
            let overlay = self
                .lexicon_overlay
                .read()
                .expect("lexicon overlay lock poisoned");
            overlay.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        };

        let mut overlay_updates: Vec<(String, serde_json::Value)> = Vec::new();
        for (term, entry) in overlay_snapshot {
            if entry.get("_embedding").is_some() {
                continue;
            }
            if entry.get("state").and_then(serde_json::Value::as_str)
                == Some("PENDING_DEFINITION")
            {
                continue;
            }
            let definition_short = entry
                .get("definition_short")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let aliases: Vec<String> = entry
                .get("aliases")
                .and_then(serde_json::Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let text = mcp_flowgate_core::embeddings::entry_embed_text(
                &term,
                &aliases,
                definition_short,
                None,
            );
            match self.embedder.embed(&text).await {
                Ok(vec) => {
                    let mut updated = entry.clone();
                    if let Some(obj) = updated.as_object_mut() {
                        obj.insert("_embedding".to_string(), json!(vec));
                    }
                    overlay_updates.push((term.clone(), updated));
                }
                Err(e) => {
                    tracing::warn!(
                        term = %term,
                        error = %e,
                        "backfill_lexicon_embeddings: failed to embed overlay term '{}'; skipping",
                        term
                    );
                }
            }
        }

        // Batch-write overlay updates.
        if !overlay_updates.is_empty() {
            let mut overlay = self
                .lexicon_overlay
                .write()
                .expect("lexicon overlay lock poisoned");
            for (term, updated) in overlay_updates {
                overlay.insert(term, updated);
            }
        }
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

        // Retain a clone of the original args so the error-handler block below
        // can echo them back in structured error responses (e.g.
        // SUBJECT_NEEDS_DEFINITION queued_command.args) even after `args` has
        // been moved into a dispatch call.
        let original_args = args.clone();

        let result = match request.name.as_ref() {
            TOOL_QUERY => {
                // §32: Some `kind` values and `subject: "lexicon:..."` need
                // specialized routing before the generic shape-router:
                //
                //  kind="skill"    → handle_skills_search (flag-gated)
                //  kind="script"   → handle_scripts_search (flag-gated)
                //  kind="lexicon"  → handle_lexicon_search
                //  subject="lexicon:<term>" (no query/wid/tr) → handle_lexicon_lookup
                //
                // All other args fall through to dispatch_query.
                let kind = args
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let subject_is_lexicon = args
                    .get("subject")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s.starts_with("lexicon:"));
                let has_query = args.get("query").is_some();
                let has_wid = args.get("workflowId").is_some();
                let has_tr = args.get("transition").is_some();

                match kind.as_deref() {
                    Some("skill") => {
                        if !self.skills_search_enabled {
                            return Err(McpError::invalid_params(
                                "flowgate.query with kind='skill' is disabled. \
                                 Enable with FlowgateServer::with_skills_search(true) \
                                 — authoring-time only."
                                    .to_string(),
                                None,
                            ));
                        }
                        self.handle_skills_search(args).await
                    }
                    Some("script") => {
                        if !self.scripts_search_enabled {
                            return Err(McpError::invalid_params(
                                "flowgate.query with kind='script' is disabled. \
                                 Enable with FlowgateServer::with_scripts_search(true) \
                                 — authoring-time only."
                                    .to_string(),
                                None,
                            ));
                        }
                        self.handle_scripts_search(args).await
                    }
                    Some("lexicon") => {
                        // Lexicon search: pass query + limit through.
                        self.handle_lexicon_search(args).await
                    }
                    _ if subject_is_lexicon && !has_query && !has_wid && !has_tr => {
                        // Lexicon lookup: subject = "lexicon:<term>". Reshape
                        // to the expected { term } arg shape.
                        let term = args["subject"]
                            .as_str()
                            .and_then(|s| s.strip_prefix("lexicon:"))
                            .unwrap_or("")
                            .to_string();
                        self.handle_lexicon_lookup(json!({ "term": term })).await
                    }
                    _ => self.dispatch_query(args, principal).await,
                }
            }
            TOOL_COMMAND => {
                // §32: `define` shape (subject namespaced + definition) is gated
                // by with_lexicon_writes(true). Default-off in production (safe
                // by construction); authoring builds opt in via the builder.
                let parsed: crate::args::CommandArgs =
                    serde_json::from_value(args.clone()).unwrap_or(crate::args::CommandArgs {
                        definition_id: None,
                        input: None,
                        workflow_id: None,
                        expected_version: None,
                        transition: None,
                        arguments: None,
                        subject: None,
                        definition: None,
                        summary: None,
                        trace_id: None,
                        run_id: None,
                        intent: None,
                        unknown_subject: None,
                    });
                let is_lexicon_define = parsed
                    .subject
                    .as_deref()
                    .is_some_and(|s| s.starts_with("lexicon:"))
                    && parsed.definition.is_some();
                if is_lexicon_define && !self.lexicon_writes_enabled {
                    Ok(json!({
                        "error": {
                            "code": "LEXICON_WRITES_DISABLED",
                            "message": "This runtime does not accept lexicon define commands.",
                            "hint": "Operators add lexicon terms via the `flowgate lexicon define` CLI subcommand."
                        },
                        "links": [
                            {
                                "rel": "operator_path",
                                "method": "cli",
                                "args": { "command": "flowgate lexicon define <term> <definition>" }
                            },
                            {
                                "rel": "lookup",
                                "method": "flowgate.query",
                                "args": { "subject": parsed.subject.unwrap_or_default() }
                            }
                        ]
                    }))
                } else {
                    self.dispatch_command(args, principal).await
                }
            }
            other => {
                return Err(McpError::invalid_params(
                    format!(
                        "Unknown tool '{other}'. Available: {} (see SPEC §32).",
                        STABLE_TOOL_NAMES.join(", ")
                    ),
                    None,
                ));
            }
        };

        match result {
            Ok(v) => Ok(v),
            Err(e) => {
                // SPEC §32 — RUN_ID_ALREADY_RUNNING is a structured response
                // at the MCP boundary (per the AMBIGUOUS_INTENT /
                // LEXICON_WRITES_DISABLED pattern). Downcast before falling
                // through to the generic internal_error mapper.
                if let Some(mcp_flowgate_core::RuntimeError::RunIdAlreadyRunning {
                    run_id,
                    existing_workflow_id,
                }) = e.downcast_ref::<mcp_flowgate_core::RuntimeError>()
                {
                    return Ok(run_id_already_running(run_id, existing_workflow_id));
                }

                // SPEC §30.10.5 — SUBJECT_NEEDS_DEFINITION is a structured
                // interaction response. The original `original_args` (the full
                // CommandArgs JSON) are echoed back as `queued_command.args`
                // so the caller can retry unchanged once the subject is defined.
                if let Some(mcp_flowgate_core::RuntimeError::SubjectNeedsDefinition {
                    unknown_subject,
                    bounded_context,
                    workflow_id_context,
                }) = e.downcast_ref::<mcp_flowgate_core::RuntimeError>()
                {
                    let merged = self.lexicon_merged_definition();
                    return Ok(subject_needs_definition(
                        unknown_subject,
                        bounded_context.as_deref(),
                        workflow_id_context,
                        &original_args,
                        Some(&merged),
                        Some(self.embedder.as_ref()),
                    )
                    .await);
                }

                Err(McpError::internal_error(e.to_string(), None))
            }
        }
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
        // §32 — always exactly two tools. Skills / scripts search are gated
        // paths within flowgate.query (kind="skill" / kind="script"), not
        // separate tool entries. The skills_search_enabled /
        // scripts_search_enabled flags govern dispatch, not tool advertising.
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
