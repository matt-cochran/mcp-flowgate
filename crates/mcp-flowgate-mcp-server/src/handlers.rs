//! Per-tool handler bodies. Methods live in a sibling `impl FlowgateServer`
//! block (same crate, same type — see `lib.rs` for the struct definition
//! and `ServerHandler` trait impl).

use mcp_flowgate_core::audit::AuditEvent;
use mcp_flowgate_core::discovery::{DiscoveryKind, SearchRequest};
use mcp_flowgate_core::model::{GetWorkflow, Principal, StartWorkflow, SubmitTransition};
use serde_json::{json, Value};

use crate::args::{
    CommandArgs, DescribeArgs, ExplainArgs, GetArgs, QueryArgs, SearchArgs, StartArgs, SubmitArgs,
};
use crate::tools::parse_kind;
use crate::FlowgateServer;

impl FlowgateServer {
    pub(crate) async fn handle_home(&self) -> anyhow::Result<Value> {
        self.discovery.home().await
    }

    pub(crate) async fn handle_search(&self, args: Value) -> anyhow::Result<Value> {
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
                { "rel": "home", "method": "flowgate.query", "args": {} }
            ]
        }))
    }

    pub(crate) async fn handle_describe(
        &self,
        args: Value,
        principal: Principal,
    ) -> anyhow::Result<Value> {
        let parsed: DescribeArgs = serde_json::from_value(args)?;
        let id = parsed.id.ok_or_else(|| anyhow::anyhow!("id is required"))?;

        // SPEC §5.8 — every `gateway.describe` call emits an audit record so
        // the authoring trail captures *which* guidance the model fetched.
        // Non-critical-path audit (per §7.3 terminology): sink failure does
        // NOT abort the describe, but emits an `audit.write_failed`
        // self-event so the failure is observable. The describe outcome
        // (ok/failed) is recorded after the lookup completes.
        let workflow_id_for_audit = parsed.workflow_id.clone();

        // SPEC §8.2: if the caller is acting inside a workflow, resolve
        // guidance bodies from the instance's pinned snapshot — the live
        // config could have drifted since `workflow.start`. Falls back to
        // the live discovery index when no workflowId is given or when the
        // subject is not in the snapshot (e.g. it's a workflow/capability
        // lookup, not a guidance fragment).
        //
        // Guidance responses use the SPEC §12 flat wire format:
        //   { kind: "guidance", subject, verb, body, lifecycle, hash }
        // Workflow / capability / connection lookups keep the existing
        // `{ id, item, links }` wrapper since they need the HATEOAS links
        // to drive the next call.
        if let Some(workflow_id) = parsed.workflow_id.as_deref() {
            // SPEC §22 — try scripts library first. If the subject lives
            // there, record the script-ack and return early. This is
            // checked BEFORE guidance because the two namespaces are
            // disjoint by design (skills use cognitive verbs, scripts use
            // action verbs); a subject in scripts can't also be in
            // skills, so the order is a clean fast path, not a precedence
            // decision.
            if let Some(mut body) = self
                .runtime
                .describe_script_for_workflow(workflow_id, &id)
                .await?
            {
                if let Some(ack) = self.script_ack_store.as_ref() {
                    if let Some(h) = body.get("hash").and_then(Value::as_str) {
                        let _ = ack.record(workflow_id, &id, h).await;
                    }
                }
                self.emit_describe_audit(
                    &id,
                    body.get("verb").and_then(Value::as_str),
                    workflow_id_for_audit.as_deref(),
                    &principal,
                    "ok",
                    None,
                )
                .await;
                if let Some(obj) = body.as_object_mut() {
                    obj.insert(
                        "links".into(),
                        json!([
                            { "rel": "home", "method": "flowgate.query", "args": {} },
                            {
                                "rel": "get",
                                "method": "flowgate.query",
                                "args": { "workflowId": workflow_id }
                            }
                        ]),
                    );
                }
                return Ok(body);
            }

            if let Some(mut body) = self
                .runtime
                .describe_guidance_for_workflow(workflow_id, &id)
                .await?
            {
                // SPEC §5.9 — record this fetch into the ack store, keyed
                // by (workflow_id, subject, body-hash). Hash-flip
                // invalidation makes the guard meaningful: a future edit
                // to the body changes the hash and the prior ack stops
                // satisfying the guard.
                if let Some(ack) = self.ack_store.as_ref() {
                    if let Some(h) = body.get("hash").and_then(Value::as_str) {
                        let _ = ack.record(workflow_id, &id, h).await;
                    }
                }
                self.emit_describe_audit(
                    &id,
                    body.get("verb").and_then(Value::as_str),
                    workflow_id_for_audit.as_deref(),
                    &principal,
                    "ok",
                    None,
                )
                .await;
                // body is already SPEC §12 shape — just attach next-step
                // links alongside (preserves HATEOAS without breaking the
                // top-level shape).
                if let Some(obj) = body.as_object_mut() {
                    obj.insert(
                        "links".into(),
                        json!([
                            { "rel": "home", "method": "flowgate.query", "args": {} },
                            {
                                "rel": "get",
                                "method": "flowgate.query",
                                "args": { "workflowId": workflow_id }
                            }
                        ]),
                    );
                }
                return Ok(body);
            }
        }

        let item = match self.discovery.describe(&id).await {
            Ok(item) => item,
            Err(e) => {
                self.emit_describe_audit(
                    &id,
                    None,
                    workflow_id_for_audit.as_deref(),
                    &principal,
                    "failed",
                    Some("GUIDANCE_DESCRIBE_FAILED"),
                )
                .await;
                return Err(e);
            }
        };

        // If the discovery layer surfaced a guidance fragment, reshape it
        // to SPEC §12 flat form. `DiscoveryKind::Guidance` items carry
        // `verb` and `body` directly on the item.
        if let Some(item) = &item {
            if matches!(item.kind, DiscoveryKind::Guidance) {
                self.emit_describe_audit(
                    &id,
                    item.verb.as_deref(),
                    workflow_id_for_audit.as_deref(),
                    &principal,
                    "ok",
                    None,
                )
                .await;
                return Ok(json!({
                    "kind": "guidance",
                    "subject": item.id,
                    "verb": item.verb.as_deref().unwrap_or_default(),
                    "body": item.body.as_deref().unwrap_or_default(),
                    "links": [
                        { "rel": "home", "method": "flowgate.query", "args": {} },
                        { "rel": "search", "method": "flowgate.query", "args": { "query": "" } }
                    ]
                }));
            }
            // SPEC §22 — non-workflow-context script describe: surface
            // body from the live indexer. (For workflow-context script
            // describes, the snapshot path above is used and an ack
            // recorded.)
            if matches!(item.kind, DiscoveryKind::Script) {
                self.emit_describe_audit(
                    &id,
                    item.verb.as_deref(),
                    workflow_id_for_audit.as_deref(),
                    &principal,
                    "ok",
                    None,
                )
                .await;
                return Ok(json!({
                    "kind": "script",
                    "subject": item.id,
                    "verb": item.verb.as_deref().unwrap_or_default(),
                    "body": item.body.as_deref().unwrap_or_default(),
                    "links": [
                        { "rel": "home", "method": "flowgate.query", "args": {} },
                        { "rel": "search", "method": "flowgate.query", "args": { "query": "" } }
                    ]
                }));
            }
        }

        // Non-guidance describe (workflow/capability/connection) — audit as
        // a successful describe regardless of whether the item resolved.
        self.emit_describe_audit(
            &id,
            None,
            workflow_id_for_audit.as_deref(),
            &principal,
            "ok",
            None,
        )
        .await;

        Ok(json!({
            "id": id,
            "item": item,
            "links": [
                { "rel": "home", "method": "flowgate.query", "args": {} },
                { "rel": "search", "method": "flowgate.query", "args": { "query": "" } }
            ]
        }))
    }

    /// SPEC §5.8 — emit a `guidance.describe_requested` audit record for a
    /// `gateway.describe` call. **Non-critical-path audit** (§7.3): a sink
    /// failure during emission does NOT abort the describe — the body has
    /// already been fetched and is about to be returned to the caller. The
    /// failure is observable via an `audit.write_failed` self-event so
    /// silent loss is impossible.
    pub(crate) async fn emit_describe_audit(
        &self,
        subject: &str,
        verb: Option<&str>,
        workflow_id: Option<&str>,
        principal: &Principal,
        outcome: &str,
        error_code: Option<&str>,
    ) {
        let event = AuditEvent::new("guidance.describe_requested")
            .with_actor(&principal.subject)
            .with_payload(json!({
                "subject":    subject,
                "verb":       verb,
                "workflowId": workflow_id,
                "outcome":    outcome,
                "errorCode":  error_code,
            }));
        let event = if let Some(wf_id) = workflow_id {
            event.with_workflow(wf_id)
        } else {
            event
        };
        if let Err(e) = self.runtime.audit().record(event).await {
            // Self-event so the loss is observable. If this also fails, we
            // log via tracing — last-resort but not silent.
            let self_event = AuditEvent::new("audit.write_failed")
                .with_actor(&principal.subject)
                .with_payload(json!({
                    "originalEvent": "guidance.describe_requested",
                    "subject":       subject,
                    "error":         e.to_string(),
                }));
            if let Err(inner) = self.runtime.audit().record(self_event).await {
                tracing::warn!(
                    subject = %subject,
                    primary_err = %e,
                    selfevt_err = %inner,
                    "guidance.describe audit emission failed and self-event also failed"
                );
            }
        }
    }

    /// SPEC §22 — gateway.scripts.search. Mirror of [`handle_skills_search`]
    /// but lists DiscoveryKind::Script items. Same progressive-disclosure
    /// invariant: returns refs (verb, subject, source), never bodies.
    /// Bodies are fetched on demand via gateway.describe.
    pub(crate) async fn handle_scripts_search(&self, args: Value) -> anyhow::Result<Value> {
        let verb_filter = args.get("verb").and_then(Value::as_str).map(str::to_string);
        let subject_root_filter = args
            .get("subject_root")
            .and_then(Value::as_str)
            .map(str::to_string);
        let source_filter = args
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .min(200) as usize;

        let items = self.discovery.list(Some(DiscoveryKind::Script)).await?;

        let mut refs: Vec<Value> = Vec::with_capacity(items.len());
        for item in items {
            if let Some(want) = &verb_filter {
                if item.verb.as_deref() != Some(want.as_str()) {
                    continue;
                }
            }
            if let Some(want_root) = &subject_root_filter {
                let root = item.id.split('.').next().unwrap_or("");
                if root != want_root {
                    continue;
                }
            }
            if let Some(want_src) = &source_filter {
                if item.source.as_deref() != Some(want_src.as_str()) {
                    continue;
                }
            }
            // Progressive disclosure: never emit `body`.
            refs.push(json!({
                "verb":    item.verb,
                "subject": item.id,
                "title":   if item.title.is_empty() { Value::Null } else { Value::String(item.title) },
                "source":  item.source,
            }));
            if refs.len() >= limit {
                break;
            }
        }

        Ok(json!({ "items": refs }))
    }

    /// SPEC §17.6 — gateway.skills.search. Returns refs (`{verb, subject,
    /// hash, source?}`), never bodies (progressive disclosure, §5.4).
    /// Authoring-time only; tool is not advertised unless
    /// `with_skills_search(true)` was set on the server.
    pub(crate) async fn handle_skills_search(&self, args: Value) -> anyhow::Result<Value> {
        let verb_filter = args.get("verb").and_then(Value::as_str).map(str::to_string);
        let subject_root_filter = args
            .get("subject_root")
            .and_then(Value::as_str)
            .map(str::to_string);
        let source_filter = args
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .min(200) as usize;

        let items = self
            .discovery
            .list(Some(DiscoveryKind::Guidance))
            .await?;

        let mut refs: Vec<Value> = Vec::with_capacity(items.len());
        for item in items {
            // Filter by verb (closed enum, no synonym matching).
            if let Some(want) = &verb_filter {
                if item.verb.as_deref() != Some(want.as_str()) {
                    continue;
                }
            }
            // Filter by subject root: first dotted segment.
            if let Some(want_root) = &subject_root_filter {
                let root = item.id.split('.').next().unwrap_or("");
                if root != want_root {
                    continue;
                }
            }
            // SPEC §5.3 — DiscoveryItem.source carries the fragment's
            // provenance (`config`, `git+https://...`, etc.). Filter is
            // exact match. Items without a source field never match a
            // source-filtered query.
            if let Some(want_src) = &source_filter {
                if item.source.as_deref() != Some(want_src.as_str()) {
                    continue;
                }
            }

            // Progressive-disclosure invariant: NEVER emit body content
            // in the listing.
            refs.push(json!({
                "verb":    item.verb,
                "subject": item.id,
                "title":   if item.title.is_empty() { Value::Null } else { Value::String(item.title) },
                "source":  item.source,
            }));

            if refs.len() >= limit {
                break;
            }
        }

        Ok(json!({ "items": refs }))
    }

    pub(crate) async fn handle_start(
        &self,
        args: Value,
        principal: Principal,
    ) -> anyhow::Result<Value> {
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
                // SPEC §20.2 — caller-supplied trace/run propagate to every
                // audit event for this workflow. Persisted on the instance.
                trace_id: parsed.trace_id,
                run_id: parsed.run_id,
            })
            .await
    }

    pub(crate) async fn handle_get(
        &self,
        args: Value,
        principal: Principal,
    ) -> anyhow::Result<Value> {
        let parsed: GetArgs = serde_json::from_value(args)?;
        let workflow_id = parsed
            .workflow_id
            .ok_or_else(|| anyhow::anyhow!("workflowId is required"))?;

        self.runtime
            .get(GetWorkflow {
                workflow_id,
                principal,
                trace_id: parsed.trace_id,
                run_id: parsed.run_id,
            })
            .await
    }

    pub(crate) async fn handle_submit(
        &self,
        args: Value,
        principal: Principal,
    ) -> anyhow::Result<Value> {
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
                trace_id: parsed.trace_id,
                run_id: parsed.run_id,
            })
            .await
    }

    pub(crate) async fn handle_explain(&self, args: Value) -> anyhow::Result<Value> {
        let parsed: ExplainArgs = serde_json::from_value(args)?;
        let workflow_id = parsed
            .workflow_id
            .ok_or_else(|| anyhow::anyhow!("workflowId is required"))?;
        let transition = parsed
            .transition
            .ok_or_else(|| anyhow::anyhow!("transition is required"))?;
        self.runtime.explain(&workflow_id, &transition).await
    }

    // ── SPEC §30 — Lexicon tools ──────────────────────────────────────────

    /// SPEC §30.5 — keyword search across the merged lexicon
    /// (base ∪ overlay). Substring match on term + definition.
    pub(crate) async fn handle_lexicon_search(&self, args: Value) -> anyhow::Result<Value> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let bounded_context = args
            .get("bounded_context")
            .and_then(Value::as_str)
            .map(String::from);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let merged = self.lexicon_merged_definition();
        let hits = mcp_flowgate_core::lexicon::search_terms(
            &merged,
            &query,
            bounded_context.as_deref(),
            limit,
        );
        Ok(json!({ "hits": hits }))
    }

    /// SPEC §30.5 — exact term lookup. Returns the entry or null.
    pub(crate) async fn handle_lexicon_lookup(&self, args: Value) -> anyhow::Result<Value> {
        let term = args
            .get("term")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("lexicon.lookup requires `term`"))?
            .to_string();
        let bounded_context = args
            .get("bounded_context")
            .and_then(Value::as_str)
            .map(String::from);
        let merged = self.lexicon_merged_definition();
        let entry = mcp_flowgate_core::lexicon::lookup_term(
            &merged,
            &term,
            bounded_context.as_deref(),
        )
        .cloned()
        .unwrap_or(Value::Null);
        Ok(json!({ "term": term, "entry": entry }))
    }

    /// SPEC §30.6 — propose / set a term. Governance-gated: agent
    /// callers writing against `human-only` terms are rejected with
    /// `LEXICON_DEFINE_REQUIRES_HUMAN`. Successful writes land in the
    /// in-memory overlay (operators persist by editing flowgate.yaml).
    pub(crate) async fn handle_lexicon_define(
        &self,
        args: Value,
        principal: Principal,
    ) -> anyhow::Result<Value> {
        let term = args
            .get("term")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("lexicon.define requires `term`"))?
            .to_string();
        let definition = args
            .get("definition_short")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("lexicon.define requires `definition_short`"))?;
        let bounded_context = args
            .get("bounded_context")
            .and_then(Value::as_str)
            .map(String::from);
        let refs: Option<Vec<String>> = args
            .get("refs")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            });
        let governance = args
            .get("governance")
            .and_then(Value::as_str)
            .map(String::from);

        // Governance gate. If the term EXISTS in base/overlay with a
        // governance: human-only marker, agents (non-human principals)
        // must be rejected. New terms inherit the DEFAULT_GOVERNANCE
        // (human-only); agent must go through a human transition.
        //
        // Exception (SPEC §30.10.7B): when the term is a PENDING_DEFINITION
        // placeholder (i.e., it appears in `pending_subjects`), the resolver
        // is filling in a gap — not overwriting a human-curated entry. The
        // governance gate is skipped so the agent that received
        // SUBJECT_NEEDS_DEFINITION can complete the `define_new` resolution.
        let is_pending = {
            let pending = self
                .pending_subjects
                .read()
                .expect("pending_subjects lock poisoned");
            pending.contains(&term)
        };
        if !is_pending {
            let merged = self.lexicon_merged_definition();
            if let Err(msg) = mcp_flowgate_core::lexicon::define_allowed(
                &merged,
                &term,
                principal.is_human(),
            ) {
                return Err(anyhow::anyhow!("{msg}"));
            }
        }

        let entry = mcp_flowgate_core::lexicon::build_entry(
            definition,
            bounded_context.as_deref(),
            refs.as_ref(),
            governance.as_deref(),
        )?;
        {
            let mut overlay = self
                .lexicon_overlay
                .write()
                .expect("lexicon overlay lock poisoned");
            overlay.insert(term.clone(), entry.clone());
        }
        // Audit the define so operators can replay vocabulary changes.
        let _ = self
            .runtime
            .audit()
            .record(
                AuditEvent::new("lexicon.defined")
                    .with_actor(&principal.subject)
                    .with_payload(json!({
                        "term":            term,
                        "bounded_context": bounded_context,
                        "by_human":        principal.is_human(),
                    })),
            )
            .await;
        Ok(json!({ "term": term, "entry": entry, "persisted_to": "overlay" }))
    }

    // ── SPEC §32 — shape-routing dispatchers ─────────────────────────────────

    /// Shape-route a `flowgate.query` call to the appropriate handler.
    /// See SPEC §32 for the dispatch table.
    ///
    /// Dispatch table (first match wins):
    /// - `(none)`               → home
    /// - `query` present        → search
    /// - `subject` only         → describe (browse-time, no audit)
    /// - `subject + workflowId` → describe-in-workflow (audit fires)
    /// - `workflowId + transition` → explain
    /// - `workflowId` alone     → get
    /// - anything else          → AMBIGUOUS_INTENT error
    pub async fn dispatch_query(
        &self,
        args: Value,
        principal: Principal,
    ) -> anyhow::Result<Value> {
        let parsed: QueryArgs = serde_json::from_value(args.clone())?;
        let q   = parsed.query.is_some();
        let s   = parsed.subject.is_some();
        let wid = parsed.workflow_id.is_some();
        let tr  = parsed.transition.is_some();

        // Detect ambiguity: `query` (search intent) alongside subject/workflow
        // fields (describe/get/explain intent) is unresolvable.
        if q && (s || wid || tr) {
            return Ok(ambiguous_intent_query());
        }

        match (q, s, wid, tr) {
            (false, false, false, false) => self.handle_home().await,
            (true, false, false, false) => {
                // Search: pass through only the search-relevant fields.
                // Omit null optionals so SearchArgs default kicks in for
                // `limit` (which has a `#[serde(default)]` but not Option).
                let mut search_args = serde_json::Map::new();
                if let Some(qv) = parsed.query {
                    search_args.insert("query".into(), Value::String(qv));
                }
                if let Some(k) = parsed.kind {
                    search_args.insert("kind".into(), Value::String(k));
                }
                if let Some(l) = parsed.limit {
                    search_args.insert("limit".into(), json!(l));
                }
                self.handle_search(Value::Object(search_args)).await
            }
            (false, true, false, false) => {
                // Browse-time describe: reshape subject → id.
                let describe_args = json!({
                    "id": parsed.subject,
                });
                self.handle_describe(describe_args, principal).await
            }
            (false, true, true, false) => {
                // Describe-in-workflow: subject + workflowId → audit fires.
                let describe_args = json!({
                    "id":         parsed.subject,
                    "workflowId": parsed.workflow_id,
                });
                self.handle_describe(describe_args, principal).await
            }
            (false, false, true, true) => {
                // Explain: workflowId + transition.
                let explain_args = json!({
                    "workflowId": parsed.workflow_id,
                    "transition": parsed.transition,
                });
                self.handle_explain(explain_args).await
            }
            (false, false, true, false) => {
                // Get: workflowId alone.
                let get_args = json!({
                    "workflowId": parsed.workflow_id,
                });
                self.handle_get(get_args, principal).await
            }
            _ => Ok(ambiguous_intent_query()),
        }
    }

    /// Shape-route a `flowgate.command` call to the appropriate handler.
    /// See SPEC §32 for the dispatch table.
    ///
    /// Dispatch table (exclusive shapes):
    /// - `definitionId` only (no workflowId, no subject)                           → start
    /// - `workflowId + transition + expectedVersion` (no subject)                   → submit
    /// - `subject` with `:` namespace + `definition` (no workflowId, no definitionId) → define
    /// - `intent == "cancel_pending_subject"` + `unknown_subject`                   → cancel
    /// - anything else                                                               → AMBIGUOUS_INTENT
    pub async fn dispatch_command(
        &self,
        args: Value,
        principal: Principal,
    ) -> anyhow::Result<Value> {
        let parsed: CommandArgs = serde_json::from_value(args.clone())?;

        let is_start = parsed.definition_id.is_some()
            && parsed.workflow_id.is_none()
            && parsed.subject.is_none();
        let is_submit = parsed.workflow_id.is_some()
            && parsed.transition.is_some()
            && parsed.expected_version.is_some()
            && parsed.subject.is_none();
        let is_define = parsed.subject.as_deref().is_some_and(|s| s.contains(':'))
            && parsed.definition.is_some()
            && parsed.workflow_id.is_none()
            && parsed.definition_id.is_none();
        let is_cancel = parsed.intent.as_deref() == Some("cancel_pending_subject")
            && parsed.unknown_subject.is_some();

        match (is_start, is_submit, is_define, is_cancel) {
            (true, false, false, false) => {
                // Start: reshape CommandArgs → StartArgs wire shape.
                let start_args = json!({
                    "definitionId": parsed.definition_id,
                    "input":        parsed.input,
                    "traceId":      parsed.trace_id,
                    "runId":        parsed.run_id,
                });
                self.handle_start(start_args, principal).await
            }
            (false, true, false, false) => {
                // Submit: reshape CommandArgs → SubmitArgs wire shape.
                let submit_args = json!({
                    "workflowId":      parsed.workflow_id,
                    "expectedVersion": parsed.expected_version,
                    "transition":      parsed.transition,
                    "arguments":       parsed.arguments,
                    "summary":         parsed.summary,
                    "traceId":         parsed.trace_id,
                    "runId":           parsed.run_id,
                });
                self.handle_submit(submit_args, principal).await
            }
            (false, false, true, false) => self.dispatch_lexicon_define(args, principal).await,
            (false, false, false, true) => {
                // Cancel pending subject placeholder.
                let subject = parsed.unknown_subject.expect("checked above");
                self.handle_cancel_pending_subject(&subject, principal).await
            }
            _ => Ok(ambiguous_intent_command()),
        }
    }

    /// Shim: extract `<term>` from `subject: "lexicon:<term>"` and delegate
    /// to the appropriate handler. Detects `aliases_add` in the definition
    /// body (SPEC §30.10.7A) and routes to `handle_alias_add`; otherwise
    /// falls through to the normal define path. Other subject namespaces
    /// (`script:`, `workflow:`, `skill:`) are reserved but have no writable
    /// primitive today — they return AMBIGUOUS_INTENT.
    async fn dispatch_lexicon_define(
        &self,
        args: Value,
        principal: Principal,
    ) -> anyhow::Result<Value> {
        let parsed: CommandArgs = serde_json::from_value(args)?;
        let subject = parsed.subject.as_deref().unwrap_or("");
        match parse_subject_namespace(subject) {
            (Some("lexicon"), term) => {
                let def_obj = parsed.definition.as_ref();

                // SPEC §30.10.7A — alias-add path: definition carries
                // `aliases_add` array, not `definition_short`.
                if let Some(aliases_add) = def_obj
                    .and_then(|d| d.get("aliases_add"))
                    .and_then(Value::as_array)
                {
                    let aliases: Vec<String> = aliases_add
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect();
                    return self
                        .handle_alias_add(term, &aliases, principal)
                        .await;
                }

                // Normal define path (define_new).
                // handle_lexicon_define expects: { term, definition_short (string),
                // bounded_context?, refs?, governance? }.
                // CommandArgs.definition is an object with primary field
                // `definition_short` (SPEC §30.10.1).
                //   { definition_short: "...", boundedContext: "...", refs: [...], governance: "..." }
                let definition_str = def_obj
                    .and_then(|d| d.get("definition_short"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let bounded_context = def_obj
                    .and_then(|d| d.get("boundedContext"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let refs = def_obj
                    .and_then(|d| d.get("refs"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let governance = def_obj
                    .and_then(|d| d.get("governance"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let reshape = json!({
                    "term":             term,
                    "definition_short": definition_str,
                    "bounded_context":  bounded_context,
                    "refs":             refs,
                    "governance":       governance,
                });
                let result = self.handle_lexicon_define(reshape, principal).await?;
                // SPEC §30.10.7B — if this was a PENDING_DEFINITION subject,
                // remove it from the pending set now that it has a real entry.
                {
                    let mut pending = self
                        .pending_subjects
                        .write()
                        .expect("pending_subjects lock poisoned");
                    pending.remove(term);
                }
                Ok(result)
            }
            _ => Ok(ambiguous_intent_command()),
        }
    }

    /// SPEC §30.10.7A — add one or more aliases to an existing lexicon entry.
    ///
    /// Checks for same-bounded-context collision across the full overlay+base.
    /// On success, appends aliases to the entry in the overlay, removes any
    /// of the added aliases from the pending-subjects set, and emits
    /// `lexicon.alias_added` per alias.
    async fn handle_alias_add(
        &self,
        target_term: &str,
        aliases_to_add: &[String],
        principal: Principal,
    ) -> anyhow::Result<Value> {
        // Load the current entry for the target term.
        let merged = self.lexicon_merged_definition();
        let existing = merged
            .get("_lexiconLibrary")
            .and_then(Value::as_object)
            .and_then(|lib| lib.get(target_term))
            .cloned();
        let mut entry = match existing {
            Some(e) if e.get("state").and_then(Value::as_str) != Some("PENDING_DEFINITION") => {
                // Real entry — proceed.
                e.as_object().cloned().unwrap_or_default()
            }
            _ => {
                return Ok(json!({
                    "error": {
                        "code": "LEXICON_ENTRY_NOT_FOUND",
                        "message": format!(
                            "LEXICON_ENTRY_NOT_FOUND: no real entry for term '{target_term}'. \
                             link_as_alias requires an existing authored entry as target."
                        ),
                        "hint": "Use define_new to create the target term first."
                    }
                }));
            }
        };

        // Collision check: build the combined index for the target's bounded
        // context and verify none of the new aliases appear there already.
        let lib = merged
            .get("_lexiconLibrary")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let target_ctx = entry
            .get("bounded_context")
            .and_then(Value::as_str)
            .unwrap_or("");
        match mcp_flowgate_core::lexicon::build_combined_index(&lib, target_ctx) {
            Err(collision_msg) => {
                // Collision already exists in the index — check if any of our
                // new aliases would conflict. Rerun with candidate aliases
                // added to a scratch map.
                return Ok(json!({
                    "error": {
                        "code": "LEXICON_ALIAS_COLLISION",
                        "message": collision_msg.to_string(),
                    }
                }));
            }
            Ok(index) => {
                // Check each new alias against the existing index.
                for alias in aliases_to_add {
                    if let Some(existing_entry) = index.get(alias.as_str()) {
                        // Alias is already taken by a term in this context.
                        let owner = existing_entry
                            .get("definition_short")
                            .and_then(Value::as_str)
                            .unwrap_or("?");
                        let _ = owner;
                        return Ok(json!({
                            "error": {
                                "code": "LEXICON_ALIAS_COLLISION",
                                "message": format!(
                                    "LEXICON_ALIAS_COLLISION: within bounded_context \
                                     '{target_ctx}', key '{alias}' is already claimed. \
                                     Aliases must be unique within a bounded context. \
                                     (SPEC §30.10.1)"
                                ),
                            }
                        }));
                    }
                }
            }
        }

        // Append aliases to the entry.
        let current_aliases = entry
            .get("aliases")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut new_aliases = current_aliases;
        for alias in aliases_to_add {
            let v = serde_json::Value::String(alias.clone());
            if !new_aliases.contains(&v) {
                new_aliases.push(v);
            }
        }
        entry.insert("aliases".to_string(), serde_json::Value::Array(new_aliases));

        // Persist into the overlay.
        {
            let mut overlay = self
                .lexicon_overlay
                .write()
                .expect("lexicon overlay lock poisoned");
            overlay.insert(target_term.to_string(), serde_json::Value::Object(entry));
        }

        // Remove added aliases from pending-subjects set and emit audit events.
        {
            let mut pending = self
                .pending_subjects
                .write()
                .expect("pending_subjects lock poisoned");
            for alias in aliases_to_add {
                pending.remove(alias.as_str());
            }
        }
        for alias in aliases_to_add {
            let _ = self
                .runtime
                .audit()
                .record(
                    AuditEvent::new("lexicon.alias_added")
                        .with_actor(&principal.subject)
                        .with_payload(json!({
                            "term":      target_term,
                            "alias":     alias,
                            "principal": principal.subject,
                        })),
                )
                .await;
        }

        Ok(json!({
            "term":    target_term,
            "aliases": aliases_to_add,
            "persisted_to": "overlay"
        }))
    }

    /// SPEC §30.10.7C — drop a PENDING_DEFINITION placeholder without creating
    /// or modifying a lexicon entry. Returns INVALID_RESOLUTION when the
    /// named subject is not in the known pending set (i.e., it is a real
    /// authored entry or unknown). Emits `lexicon.pending_cancelled`.
    async fn handle_cancel_pending_subject(
        &self,
        subject: &str,
        principal: Principal,
    ) -> anyhow::Result<Value> {
        // Check: the subject must be in the pending set.
        let was_pending = {
            let pending = self
                .pending_subjects
                .read()
                .expect("pending_subjects lock poisoned");
            pending.contains(subject)
        };

        if !was_pending {
            return Ok(json!({
                "error": {
                    "code": "INVALID_RESOLUTION",
                    "message": format!(
                        "INVALID_RESOLUTION: subject '{subject}' is not a pending \
                         placeholder. Cancel applies only to PENDING_DEFINITION \
                         subjects. (SPEC §30.10.9)"
                    ),
                    "hint": "Use flowgate.query to inspect the lexicon entry."
                }
            }));
        }

        // Remove from pending set.
        {
            let mut pending = self
                .pending_subjects
                .write()
                .expect("pending_subjects lock poisoned");
            pending.remove(subject);
        }

        // Emit audit event.
        let _ = self
            .runtime
            .audit()
            .record(
                AuditEvent::new("lexicon.pending_cancelled")
                    .with_actor(&principal.subject)
                    .with_payload(json!({
                        "term":         subject,
                        "cancelled_by": principal.subject,
                    })),
            )
            .await;

        Ok(json!({
            "cancelled":   subject,
            "persisted_to": "pending_subjects"
        }))
    }

    /// Build a synthetic "workflow definition" carrying the merged
    /// `_lexiconLibrary` so the core `lookup_term` / `search_terms`
    /// helpers (which expect a workflow-definition shape) can be
    /// reused without duplication.
    fn lexicon_merged_definition(&self) -> Value {
        let base = self
            .lexicon_base
            .as_object()
            .cloned()
            .unwrap_or_default();
        let overlay_clone = {
            let overlay = self
                .lexicon_overlay
                .read()
                .expect("lexicon overlay lock poisoned");
            overlay.clone()
        };
        let mut merged = base;
        for (k, v) in overlay_clone {
            merged.insert(k, v);
        }
        json!({ "_lexiconLibrary": merged })
    }
}

// ── §32 dispatch helpers ──────────────────────────────────────────────────────

/// Parse a cross-primitive subject namespace per §32.
///
/// `"lexicon:churn"` → `(Some("lexicon"), "churn")`
/// `"swe_agent"` → `(None, "swe_agent")`
pub(crate) fn parse_subject_namespace(s: &str) -> (Option<&str>, &str) {
    match s.split_once(':') {
        Some((ns, term)) => (Some(ns), term),
        None => (None, s),
    }
}

/// Structured AMBIGUOUS_INTENT response body for `flowgate.query` dispatch.
/// Per SPEC §32, this is a 4xx-class structured response — NOT an MCP
/// protocol error — so HATEOAS links remain machine-parseable by clients.
fn ambiguous_intent_query() -> Value {
    json!({
        "error": {
            "code": "AMBIGUOUS_INTENT",
            "message": "flowgate.query args do not match a known dispatch shape",
            "hint": "see §32 dispatch table: home (no args), search (query), describe (subject), get (workflowId), explain (workflowId+transition), describe-in-workflow (subject+workflowId)"
        },
        "links": [
            { "rel": "home",   "method": "flowgate.query", "args": {} },
            { "rel": "search", "method": "flowgate.query", "args": { "query": "" } }
        ]
    })
}

/// Structured `RUN_ID_ALREADY_RUNNING` response body for `flowgate.command`
/// start. Per SPEC §32, this is a 4xx-class structured response — NOT an MCP
/// protocol error — so HATEOAS links remain machine-parseable by clients.
///
/// The `get` link points directly to the existing workflow instance so the
/// caller can resume or introspect without a second lookup.
pub(crate) fn run_id_already_running(run_id: &str, existing_workflow_id: &str) -> Value {
    json!({
        "error": {
            "code": "RUN_ID_ALREADY_RUNNING",
            "message": format!("An instance already exists with run_id '{run_id}'."),
            "hint": "Each run_id is single-use. Fetch the existing instance with the linked get, or retry with a fresh run_id."
        },
        "links": [
            {
                "rel": "get",
                "method": "flowgate.query",
                "args": { "workflowId": existing_workflow_id }
            }
        ]
    })
}

/// SPEC §30.10.5 — structured SUBJECT_NEEDS_DEFINITION interaction response.
///
/// Returned when `WorkflowRuntime::start` detects a `PENDING_DEFINITION`
/// placeholder in the workflow's `_lexiconLibrary`. The workflow instance is
/// NOT created. The original tool-call args are echoed back verbatim as
/// `queued_command.args` so the resolver can retry unchanged after defining the
/// subject.
///
/// Three HATEOAS links guide resolution:
///
/// - `link_as_alias`  — link the unknown subject as a synonym for an existing term.
/// - `define_new`     — add a new first-class lexicon entry.
/// - `cancel`         — abandon the original command.
///
/// The `candidates` array is intentionally empty here. Task 3.4 (Levenshtein
/// ranking) and Task 3.9 (semantic ranking) will populate it.
pub(crate) fn subject_needs_definition(
    unknown_subject: &str,
    bounded_context: Option<&str>,
    workflow_id_context: &str,
    queued_args: &Value,
) -> Value {
    let lexicon_subject = format!("lexicon:{unknown_subject}");
    json!({
        "interaction": {
            "kind": "SUBJECT_NEEDS_DEFINITION",
            "unknown_subject": unknown_subject,
            "context": {
                "encountered_in": workflow_id_context,
                "bounded_context": bounded_context
            },
            "candidates": []
        },
        "queued_command": {
            "method": "flowgate.command",
            "args": queued_args
        },
        "links": [
            {
                "rel": "link_as_alias",
                "method": "flowgate.command",
                "args": {
                    "subject": lexicon_subject,
                    "definition": { "aliases_add": [unknown_subject] }
                },
                "hint": "Use this if the unknown subject is a synonym for an existing term."
            },
            {
                "rel": "define_new",
                "method": "flowgate.command",
                "args": {
                    "subject": lexicon_subject,
                    "definition": {
                        "definition_short": "<fill in>",
                        "boundedContext": bounded_context
                    }
                },
                "hint": "Use this if the unknown subject is a genuinely new concept."
            },
            {
                "rel": "cancel",
                "method": "flowgate.command",
                "args": {
                    "intent": "cancel_pending_subject",
                    "unknown_subject": unknown_subject
                },
                "hint": "Abandon the original command — the subject was a mistake."
            }
        ]
    })
}

/// Structured AMBIGUOUS_INTENT response body for `flowgate.command` dispatch.
/// Per SPEC §32, this is a 4xx-class structured response — NOT an MCP
/// protocol error — so HATEOAS links remain machine-parseable by clients.
fn ambiguous_intent_command() -> Value {
    json!({
        "error": {
            "code": "AMBIGUOUS_INTENT",
            "message": "flowgate.command args do not match a known dispatch shape",
            "hint": "see §32 dispatch table: start (definitionId only), submit (workflowId+expectedVersion+transition), define (subject namespaced + definition)"
        },
        "links": [
            { "rel": "start_example",  "method": "flowgate.command", "args": { "definitionId": "<your-workflow>" } },
            { "rel": "submit_example", "method": "flowgate.command", "args": { "workflowId": "<id>", "expectedVersion": 0, "transition": "<name>" } },
            { "rel": "define_example", "method": "flowgate.command", "args": { "subject": "lexicon:<term>", "definition": { "definition_short": "..." } } }
        ]
    })
}
