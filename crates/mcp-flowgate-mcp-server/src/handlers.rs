//! Per-tool handler bodies. Methods live in a sibling `impl FlowgateServer`
//! block (same crate, same type — see `lib.rs` for the struct definition
//! and `ServerHandler` trait impl).

use mcp_flowgate_core::audit::AuditEvent;
use mcp_flowgate_core::discovery::{DiscoveryKind, SearchRequest};
use mcp_flowgate_core::model::{GetWorkflow, Principal, StartWorkflow, SubmitTransition};
use serde_json::{json, Value};

use crate::args::{
    DescribeArgs, ExplainArgs, GetArgs, SearchArgs, StartArgs, SubmitArgs,
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
                { "rel": "home", "method": "gateway.home", "args": {} }
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
                            { "rel": "home", "method": "gateway.home", "args": {} },
                            {
                                "rel": "get",
                                "method": "workflow.get",
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
                            { "rel": "home", "method": "gateway.home", "args": {} },
                            {
                                "rel": "get",
                                "method": "workflow.get",
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
                        { "rel": "home", "method": "gateway.home", "args": {} },
                        { "rel": "search", "method": "gateway.search", "args": { "query": "" } }
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
                        { "rel": "home", "method": "gateway.home", "args": {} },
                        { "rel": "search", "method": "gateway.search", "args": { "query": "" } }
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
                { "rel": "home", "method": "gateway.home", "args": {} },
                { "rel": "search", "method": "gateway.search", "args": { "query": "" } }
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
            .get("definition")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("lexicon.define requires `definition`"))?;
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
        let merged = self.lexicon_merged_definition();
        if let Err(msg) = mcp_flowgate_core::lexicon::define_allowed(
            &merged,
            &term,
            principal.is_human(),
        ) {
            return Err(anyhow::anyhow!("{msg}"));
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
