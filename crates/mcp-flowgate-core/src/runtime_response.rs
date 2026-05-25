//! Response-builder and guard-evaluation helpers for [`WorkflowRuntime`].
//! Methods stay on the same `impl WorkflowRuntime` block split across
//! sibling files — see `runtime.rs` for the type definition and lifecycle
//! entry points (`start`, `submit`, `get`).

use serde_json::{json, Value};

use crate::error::ExecutorError;
use crate::model::{Principal, WorkflowInstance};
use crate::runtime::WorkflowRuntime;
use crate::runtime_links::{
    collect_guidance_refs, is_terminal, link_filter_byguards, links, pointer_escape,
    transition_definition,
};
use crate::templating::render_template;

/// SPEC §9 + §20.4 — outcome of evaluating a transition's full `guards:`
/// list. Carries pass/fail plus per-guard `{kind, result}` records (for
/// transition-record `guards` field) plus an optional §20.4 diagnostic
/// code when the rejection has a filter-attributable cause.
pub(crate) struct GuardsOutcome {
    pub pass: bool,
    pub evaluated: Vec<Value>,
    pub diagnostic: Option<String>,
}

impl WorkflowRuntime {
    pub(crate) async fn guards_pass(
        &self,
        transition: &Value,
        instance: &WorkflowInstance,
        arguments: &Value,
        principal: &Principal,
        correlation_id: &str,
    ) -> anyhow::Result<GuardsOutcome> {
        let guards = transition
            .get("guards")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut evaluated = Vec::with_capacity(guards.len());
        for (idx, guard) in guards.iter().enumerate() {
            // SPEC §20.1 — `evaluate_with_diagnostic` returns a
            // §20.4 error code when the guard rejected for a specific
            // named reason (e.g. EVIDENCE_DIGEST_REQUIRED). Default-impl
            // returns None for everything else; preserved-behavior for
            // existing guard kinds.
            let (pass, diagnostic) = self
                .guards
                .evaluate_with_diagnostic(guard, instance, arguments, principal)
                .await?;
            let kind = guard
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            evaluated.push(json!({ "kind": kind, "result": pass }));
            self.audit
                .record(
                    instance
                        .audit_event("guard.evaluated")
                        .with_correlation(correlation_id)
                        .with_payload(json!({
                            "guardIndex": idx,
                            "guard": guard,
                            "passed": pass,
                        })),
                )
                .await?;
            if !pass {
                return Ok(GuardsOutcome {
                    pass: false,
                    evaluated,
                    diagnostic,
                });
            }
        }

        Ok(GuardsOutcome {
            pass: true,
            evaluated,
            diagnostic: None,
        })
    }

    /// Build the response body, including link filtering when the workflow
    /// or state declares `linkFilter: byGuards`. Always evaluated against
    /// the provided principal so "what could THIS caller do next" is what
    /// surfaces.
    pub(crate) async fn response(
        &self,
        definition: &Value,
        instance: &WorkflowInstance,
        status: &str,
        error: Option<Value>,
        principal: &Principal,
    ) -> Value {
        let final_status = if is_terminal(definition, &instance.state) {
            "completed"
        } else {
            status
        };

        let mut all_links = links(definition, instance);
        if link_filter_byguards(definition, &instance.state) {
            all_links = self
                .filter_links_by_guards(all_links, definition, instance, principal)
                .await;
        }

        let mut body = json!({
            "workflow": {
                "id": instance.id,
                "definitionId": instance.definition_id,
                "definitionVersion": instance.definition_version,
                "state": instance.state,
                "version": instance.version,
            },
            "result": {
                "status": final_status,
            },
            "context": instance.context,
            "links": all_links,
            "evidence": [],
        });

        // SPEC §6.3 — surface the reserved `summary` slot at top level so an
        // LLM resuming a workflow cold sees the last human-readable summary
        // without having to dig through context. Absent when never set.
        if let Some(summary) = instance
            .context
            .get("summary")
            .and_then(Value::as_str)
        {
            body["summary"] = Value::String(summary.to_string());
        }

        if let Some(err) = error {
            body["error"] = err;
        }

        // Phase guidance: attach goal/instructions from the current state.
        // `{{ }}` placeholders are interpolated at render time against the
        // live instance; stored strings are never mutated (SPEC v2 §5.2).
        let state_path = format!("/states/{}", pointer_escape(&instance.state));
        let state_def_opt = definition.pointer(&state_path);
        let mut guidance = serde_json::Map::new();
        if let Some(state_def) = state_def_opt {
            if let Some(g) = state_def.get("goal").and_then(Value::as_str) {
                guidance.insert("goal".into(), json!(render_template(g, instance)));
            }
            if let Some(g) = state_def.get("guidance").and_then(Value::as_str) {
                guidance.insert(
                    "instructions".into(),
                    json!(render_template(g, instance)),
                );
            }
            // SPEC §21 — `delegate` is a pass-through pointer to an agent
            // config name. The gateway never branches on it; the TUI
            // interpreter consumes it to spawn an isolated sub-agent.
            // Empty/non-string entries are rejected at config load by
            // `INVALID_DELEGATE`, so any value reaching this code is a
            // non-empty string.
            if let Some(d) = state_def.get("delegate").and_then(Value::as_str) {
                if !d.is_empty() {
                    body["delegate"] = Value::String(d.to_string());
                }
            }
        }

        // Skills refs: surface workflow-scope + active-state-scope refs
        // (SPEC v2 §5.5). Each ref pairs `subject` (the gateway.describe
        // lookup) with `verb` (the mode). Verbs are resolved from the
        // `_skillsLibrary` stamped onto the snapshot at config-resolve.
        let refs = collect_guidance_refs(definition, state_def_opt);
        if !refs.is_empty() {
            guidance.insert("refs".into(), Value::Array(refs));
        }

        if !guidance.is_empty() {
            body["guidance"] = Value::Object(guidance);
        }

        body
    }

    /// Evaluate each link's transition guards silently (no audit) and keep
    /// only those that would currently pass. Argument-dependent guards are
    /// evaluated against `{}` since arguments aren't known at link-gen
    /// time — those typically end up filtered out, which is the right
    /// answer for "show me what I could do *right now* without thinking."
    async fn filter_links_by_guards(
        &self,
        links: Vec<Value>,
        definition: &Value,
        instance: &WorkflowInstance,
        principal: &Principal,
    ) -> Vec<Value> {
        let empty_args = json!({});
        let mut out = Vec::with_capacity(links.len());
        for link in links {
            let rel = match link.get("rel").and_then(Value::as_str) {
                Some(r) => r,
                None => continue,
            };
            let transition = match transition_definition(definition, &instance.state, rel) {
                Some(t) => t,
                None => continue,
            };
            let guards = transition
                .get("guards")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut all_pass = true;
            for guard in guards {
                match self
                    .guards
                    .evaluate(&guard, instance, &empty_args, principal)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) | Err(_) => {
                        all_pass = false;
                        break;
                    }
                }
            }
            if all_pass {
                out.push(link);
            }
        }
        out
    }

    pub(crate) async fn invalid_response(
        &self,
        definition: &Value,
        instance: &WorkflowInstance,
        code: &str,
        message: String,
        attempted_transition: Option<&str>,
        principal: &Principal,
    ) -> Value {
        self.response(
            definition,
            instance,
            "rejected",
            Some(json!({
                "code": code,
                "message": message,
                "attemptedTransition": attempted_transition,
            })),
            principal,
        )
        .await
    }

    /// Audit-aware version of `invalid_response`. Records `transition.rejected`
    /// before building the response body. Errors recording the event are
    /// swallowed to ensure the caller still gets a useful response — the
    /// rejection itself is the primary signal.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_rejected(
        &self,
        definition: &Value,
        instance: &WorkflowInstance,
        code: &str,
        message: String,
        attempted_transition: &str,
        correlation_id: &str,
        principal: &Principal,
    ) -> Value {
        self.record_or_self_event(
            instance
                .audit_event("transition.rejected")
                .with_correlation(correlation_id)
                .with_actor(&principal.subject)
                .with_payload(json!({
                    "transition": attempted_transition,
                    "code": code,
                    "message": message,
                    "fromState": instance.state,
                })),
        )
        .await;
        self.invalid_response(
            definition,
            instance,
            code,
            message,
            Some(attempted_transition),
            principal,
        )
        .await
    }

    pub(crate) async fn failed_response(
        &self,
        definition: &Value,
        instance: &WorkflowInstance,
        err: &ExecutorError,
        attempted_transition: &str,
        principal: &Principal,
    ) -> Value {
        self.response(
            definition,
            instance,
            "failed",
            Some(json!({
                "code": "EXECUTOR_FAILED",
                "message": err.to_string(),
                "errorClass": err.class().token(),
                "attemptedTransition": attempted_transition,
            })),
            principal,
        )
        .await
    }
}
