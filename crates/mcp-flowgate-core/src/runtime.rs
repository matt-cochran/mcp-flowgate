use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::bail;
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use serde::Serialize;

use crate::audit::AuditSink;
use crate::error::RuntimeError;
use crate::model::*;
use crate::ports::*;
pub(crate) use crate::runtime_schema::{apply_schema_defaults, required_str, validate_schema};
pub use crate::runtime_links::is_terminal;
pub(crate) use crate::runtime_links::{empty_object_schema, pointer_escape, transition_definition};

// ---------------------------------------------------------------------------
// Deterministic chaining types
// ---------------------------------------------------------------------------

/// One step in a deterministic chain, recording the state traversal.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainStep {
    pub from_state: String,
    pub transition: String,
    pub to_state: String,
    pub version: u64,
}

/// Outcome of a deterministic chain run.
pub enum ChainOutcome {
    /// Chain completed normally: reached a decision point (non-deterministic
    /// state), terminal state, or depth limit.
    Completed(ChainResult),
    /// Chain stopped because an executor failed or no viable deterministic
    /// transition could be selected.
    Failed {
        partial: ChainResult,
        error: String,
        error_class: String,
        failed_transition: String,
    },
}

/// Accumulated state from a deterministic chain.
pub struct ChainResult {
    pub instance: WorkflowInstance,
    pub steps: Vec<ChainStep>,
    pub evidence: Vec<Evidence>,
}

/// SPEC §7.2 — parameter bundle for `emit_transition_record`. Collected into
/// a struct so the caller doesn't shuffle ~12 positional arguments. Borrows
/// the live instance + correlation id so the helper is a pure projection
/// over the commit context.
pub(crate) struct TransitionRecordParams<'a> {
    pub(crate) instance: &'a WorkflowInstance,
    pub(crate) from_state: &'a str,
    pub(crate) transition_name: &'a str,
    pub(crate) transition_def: &'a Value,
    pub(crate) actor: &'a str,
    pub(crate) principal: Option<&'a str>,
    pub(crate) arguments: &'a Value,
    pub(crate) blackboard_delta: Value,
    pub(crate) guard_results: Vec<Value>,
    pub(crate) child_workflow_id: Option<String>,
    /// `Some((ok, durationMs))` only when the executor actually ran on this
    /// transition. `None` for transitions without an `executor:` and for
    /// `onTimeout` records.
    pub(crate) executor_outcome: Option<(bool, u64)>,
    pub(crate) correlation_id: &'a str,
}

/// The workflow runtime. Holds Arcs of all ports so it can be cloned cheaply
/// and embedded in tool handlers.
#[derive(Clone)]
pub struct WorkflowRuntime {
    pub(crate) definitions: Arc<dyn DefinitionStore>,
    pub(crate) store: Arc<dyn WorkflowStore>,
    pub(crate) executors: Arc<dyn ExecutorRegistry>,
    pub(crate) guards: Arc<dyn GuardEvaluator>,
    pub(crate) audit: Arc<dyn AuditSink>,
    pub(crate) evidence: Option<Arc<dyn EvidenceStore>>,
    /// Set by the supervisor to refuse new `workflow.start` calls during a
    /// graceful drain. Existing `submit`/`get` keep working so in-flight work
    /// finishes cleanly. See `docs/CONFIG.md` "Zero-downtime config changes".
    pub(crate) draining: Arc<AtomicBool>,
}

impl WorkflowRuntime {
    pub fn new(
        definitions: Arc<dyn DefinitionStore>,
        store: Arc<dyn WorkflowStore>,
        executors: Arc<dyn ExecutorRegistry>,
        guards: Arc<dyn GuardEvaluator>,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            definitions,
            store,
            executors,
            guards,
            audit,
            evidence: None,
            draining: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Mark this runtime as draining. Subsequent `start` calls fail with a
    /// clean error; `submit`/`get` continue to work so in-flight workflows
    /// can complete.
    pub fn begin_drain(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    /// True once `begin_drain` has been called.
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    /// Attach an evidence store. Without one, `evidence` guards always pass
    /// (placeholder behavior). With one, accumulated evidence from each
    /// successful transition is persisted and queried by guards on later
    /// transitions.
    pub fn with_evidence(mut self, evidence: Arc<dyn EvidenceStore>) -> Self {
        self.evidence = Some(evidence);
        self
    }

    pub fn audit(&self) -> &Arc<dyn AuditSink> {
        &self.audit
    }

    /// SPEC §5.8 non-critical-path audit pattern (FMECA FM-8 mitigation,
    /// audit-resolution plan C.1). Records `event` to the audit sink; on
    /// sink failure, emits an `audit.write_failed` self-event so the loss
    /// is observable. If the self-event ALSO fails, falls back to a
    /// tracing::warn — last-resort but never silent.
    ///
    /// Use this from non-critical paths where the workflow operation must
    /// continue regardless of audit outcome (e.g. `chain.step`,
    /// `chain.completed`, post-outcome notifications). The §7.3
    /// audit-before-commit pattern (e.g. transition records, definition
    /// publishes) must propagate errors via `?` instead.
    pub(crate) async fn record_or_self_event(&self, event: crate::audit::AuditEvent) {
        let event_type = event.event_type.clone();
        if let Err(primary_err) = self.audit.record(event).await {
            let self_event = crate::audit::AuditEvent::new("audit.write_failed")
                .with_payload(serde_json::json!({
                    "originalEvent": event_type,
                    "error":         primary_err.to_string(),
                }));
            if let Err(inner) = self.audit.record(self_event).await {
                tracing::warn!(
                    original = %event_type,
                    primary_err = %primary_err,
                    selfevt_err = %inner,
                    "non-critical audit write failed and self-event also failed"
                );
            }
        }
    }

    pub async fn start(&self, request: StartWorkflow) -> anyhow::Result<Value> {
        if self.is_draining() {
            bail!("gateway is shutting down; please retry shortly");
        }
        let definition = self.definitions.load(&request.definition_id).await?;
        let mut input = request.input;
        apply_schema_defaults(definition.pointer("/inputSchema"), &mut input);
        validate_schema(definition.pointer("/inputSchema"), &input, "workflow input")?;
        let request = StartWorkflow { input, ..request };

        let initial_state = required_str(&definition, "/initialState")?.to_owned();
        // The instance carries its own resolved definition snapshot. Source
        // `definition_version` from that snapshot's `version` (Task 4.1
        // guarantees every workflow definition has one, defaulting to "0").
        let definition_version = definition
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("0")
            .to_owned();

        let initial_context = definition
            .get("initialContext")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let instance = WorkflowInstance {
            id: format!("wf_{}", Uuid::new_v4().simple()),
            definition_id: request.definition_id.clone(),
            definition_version,
            definition: definition.clone(),
            state: initial_state,
            version: 0,
            input: request.input,
            context: initial_context,
            started_at: Utc::now(),
            // SPEC §20.2 — persist trace/run on the instance so every
            // downstream audit event for this workflow inherits them.
            trace_id: request.trace_id,
            run_id: request.run_id,
        };
        let correlation_id = format!("cor_{}", Uuid::new_v4().simple());

        let instance = self.store.create(instance).await?;

        self.audit
            .record(
                instance
                    .audit_event("workflow.started")
                    .with_correlation(&correlation_id)
                    .with_actor(&request.principal.subject)
                    .with_payload(json!({
                        "definitionId": instance.definition_id,
                        "state": instance.state,
                        "version": instance.version,
                    })),
            )
            .await?;

        let instance = self
            .run_on_enter(definition.clone(), instance, &correlation_id)
            .await?;

        // Run deterministic chain from the initial state
        let max_depth = definition
            .get("maxChainDepth")
            .and_then(Value::as_u64)
            .unwrap_or(50);
        let chain_outcome = self
            .run_deterministic_chain(
                &definition,
                instance,
                &request.principal,
                &correlation_id,
                max_depth,
            )
            .await?;

        match chain_outcome {
            ChainOutcome::Completed(result) => {
                if is_terminal(&definition, &result.instance.state) {
                    self.audit
                        .record(
                            result
                                .instance
                                .audit_event("workflow.completed")
                                .with_correlation(&correlation_id)
                                .with_payload(json!({ "state": result.instance.state })),
                        )
                        .await?;
                }

                let mut response = self
                    .response(
                        &definition,
                        &result.instance,
                        "started",
                        None,
                        &request.principal,
                    )
                    .await;
                if !result.steps.is_empty() {
                    response["chain"] = serde_json::to_value(&result.steps)?;
                }
                if !result.evidence.is_empty() {
                    response["evidence"] = serde_json::to_value(&result.evidence)?;
                }
                Ok(response)
            }
            ChainOutcome::Failed {
                partial,
                error,
                error_class,
                failed_transition,
            } => {
                let mut response = self
                    .response(
                        &definition,
                        &partial.instance,
                        "failed",
                        Some(json!({
                            "code": "CHAIN_FAILED",
                            "message": error,
                            "errorClass": error_class,
                            "attemptedTransition": failed_transition,
                        })),
                        &request.principal,
                    )
                    .await;
                if !partial.steps.is_empty() {
                    response["chain"] = serde_json::to_value(&partial.steps)?;
                }
                if !partial.evidence.is_empty() {
                    response["evidence"] = serde_json::to_value(&partial.evidence)?;
                }
                // Include the failed deterministic transition in links for recovery
                if !failed_transition.is_empty() {
                    if let Some(links) = response.get_mut("links").and_then(Value::as_array_mut) {
                        if let Some(t_def) = transition_definition(
                            &definition,
                            &partial.instance.state,
                            &failed_transition,
                        ) {
                            links.push(json!({
                                "rel": failed_transition,
                                "title": t_def.get("title").and_then(Value::as_str)
                                    .unwrap_or(&failed_transition),
                                "description": t_def.get("description"),
                                "method": "workflow.submit",
                                "actor": "deterministic",
                                "args": {
                                    "workflowId": partial.instance.id,
                                    "expectedVersion": partial.instance.version,
                                    "transition": failed_transition,
                                },
                                "inputSchema": empty_object_schema(),
                            }));
                        }
                    }
                }
                Ok(response)
            }
        }
    }

    pub async fn get(&self, request: GetWorkflow) -> anyhow::Result<Value> {
        let instance = self.store.load(&request.workflow_id).await?;
        // In-flight: resolve the definition from the instance's carried
        // snapshot, never from the live `DefinitionStore`. A config edit or
        // hot reload must not disturb a running instance (SPEC §8.3).
        let definition = instance.definition.clone();
        if let Some(timed_out) = self
            .check_and_apply_timeout(&definition, instance.clone(), &request.principal)
            .await?
        {
            return Ok(self
                .response(
                    &definition,
                    &timed_out,
                    "timed_out",
                    None,
                    &request.principal,
                )
                .await);
        }
        Ok(self
            .response(
                &definition,
                &instance,
                "waiting_for_action",
                None,
                &request.principal,
            )
            .await)
    }

    /// SPEC §8.2 + §12 — resolve a guidance fragment's `{verb, body}` from the
    /// snapshot pinned to a specific workflow instance. Returns `None` if
    /// either the workflow id or the subject is unknown to the snapshot.
    /// Used by `gateway.describe { id, workflowId }` so an in-flight LLM
    /// receives the body that existed when the workflow was started — not
    /// whatever the operator has since edited the live config to say.
    pub async fn describe_guidance_for_workflow(
        &self,
        workflow_id: &str,
        subject: &str,
    ) -> anyhow::Result<Option<Value>> {
        let instance = self.store.load(workflow_id).await?;
        let Some(entry) = instance
            .definition
            .pointer("/_skillsLibrary")
            .and_then(Value::as_object)
            .and_then(|lib| lib.get(subject))
        else {
            return Ok(None);
        };
        let verb = entry.get("verb").and_then(Value::as_str).unwrap_or_default();
        let body = entry.get("body").and_then(Value::as_str).unwrap_or_default();
        let lifecycle = entry.get("lifecycle").and_then(Value::as_str).unwrap_or_default();
        let hash = entry.get("hash").and_then(Value::as_str).unwrap_or_default();
        Ok(Some(json!({
            "kind":      "guidance",
            "subject":   subject,
            "verb":      verb,
            "lifecycle": lifecycle,
            "hash":      hash,
            "body":      body,
        })))
    }

    pub async fn explain(&self, workflow_id: &str, transition: &str) -> anyhow::Result<Value> {
        let instance = self.store.load(workflow_id).await?;
        // In-flight: resolve the definition from the instance's carried
        // snapshot, never from the live `DefinitionStore` (SPEC §8.3).
        let definition = instance.definition.clone();

        let transition_def = transition_definition(&definition, &instance.state, transition);
        let allowed = transition_def.is_some();
        let actor = transition_def
            .and_then(|t| t.get("actor"))
            .and_then(Value::as_str)
            .unwrap_or("agent");
        let is_deterministic = actor == "deterministic";

        let legal_now: Vec<String> = definition
            .pointer(&format!(
                "/states/{}/transitions",
                pointer_escape(&instance.state)
            ))
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();

        Ok(json!({
            "workflowId": instance.id,
            "currentState": instance.state,
            "transition": transition,
            "allowedFromCurrentState": allowed,
            "actor": actor,
            "deterministic": is_deterministic,
            "legalTransitionsNow": legal_now,
        }))
    }

    /// Emit the transition record for one applied transition, **record-first**:
    /// this writes the `workflow.transition` audit event and MUST be called
    /// *before* `save_if_version` commits the resulting snapshot.
    ///
    /// `seq` is the resulting `WorkflowInstance.version` (post-increment). The
    /// caller passes the to-be-committed instance so every required field can be
    /// sourced exactly.
    ///
    /// On `Err`, the caller MUST abort the transition and NOT commit the
    /// snapshot — propagating the [`RuntimeError::RecordWriteFailed`] is the
    /// whole point of the record-first ordering. The `Result` must never be
    /// swallowed.
    pub(crate) async fn emit_transition_record(
        &self,
        params: TransitionRecordParams<'_>,
    ) -> Result<(), RuntimeError> {
        let seq = params.instance.version;

        // SPEC §7.2 — executor descriptor: `{ kind, ok, durationMs }` when
        // the transition's executor actually ran. `kind` comes from the
        // declared executor on the transition; `ok` + `durationMs` come
        // from the caller's wall-clock around `execute_with_reliability`.
        // For transitions without an executor (or paths like onTimeout)
        // the descriptor is omitted entirely.
        let executor = params
            .transition_def
            .get("executor")
            .and_then(|e| e.get("kind"))
            .and_then(Value::as_str)
            .map(|kind| {
                let mut desc = json!({ "kind": kind });
                if let Some((ok, duration_ms)) = params.executor_outcome {
                    desc["ok"] = Value::Bool(ok);
                    desc["durationMs"] = json!(duration_ms);
                }
                desc
            });

        // SPEC §7.2 — `blackboardDelta` carries the per-transition diff of
        // `context` so cumulative replay (§7.5) can reconstruct the blackboard
        // at any past `seq`. Computed by the call site against pre/post-merge
        // contexts.
        //
        // SPEC §7.2 — `guards` carries each guard that was actually evaluated
        // on this transition, in declaration order, as `{kind, result}` pairs.
        // For deterministic chain hops and onTimeout (where guards aren't
        // evaluated), this is an empty vec. `childWorkflowId` is set when
        // the transition's executor was `kind: workflow` and reported the
        // sub-workflow id it spawned; null otherwise.
        let child = match params.child_workflow_id {
            Some(id) => Value::String(id),
            None => Value::Null,
        };
        let mut record = json!({
            "workflowId": params.instance.id,
            "definitionId": params.instance.definition_id,
            "definitionVersion": params.instance.definition_version,
            "seq": seq,
            "timestamp": Utc::now().to_rfc3339(),
            "fromState": params.from_state,
            "toState": params.instance.state,
            "transition": params.transition_name,
            "actor": params.actor,
            "principal": params.principal,
            "guards": params.guard_results,
            "arguments": params.arguments,
            "blackboardDelta": params.blackboard_delta,
            "childWorkflowId": child,
            "correlationId": params.correlation_id,
        });
        if let Some(executor) = executor {
            record["executor"] = executor;
        }

        let mut event = params
            .instance
            .audit_event("workflow.transition")
            .with_correlation(params.correlation_id)
            .with_payload(record);
        if let Some(principal) = params.principal {
            event = event.with_actor(principal);
        }

        self.audit
            .record(event)
            .await
            .map_err(|source| RuntimeError::RecordWriteFailed {
                workflow_id: params.instance.id.clone(),
                seq,
                source,
            })
    }

}

// ---------------------------------------------------------------------------
// Guidance refs (SPEC v2 §5.5)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Guidance string templating (SPEC v2 §5.2)
// ---------------------------------------------------------------------------

