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
}
