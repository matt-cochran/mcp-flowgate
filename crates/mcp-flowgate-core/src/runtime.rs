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

    /// T24 — cancel a running workflow. Sets `cancelled_at` +
    /// `cancelled_reason` on the instance (without changing `state`,
    /// so the operator can later recover by reading the original
    /// position). Subsequent `submit` calls return `WORKFLOW_CANCELLED`;
    /// `get` surfaces `result.status: "cancelled"`. Emits a
    /// `workflow.cancelled` audit event.
    ///
    /// Idempotent: re-cancelling an already-cancelled workflow refreshes
    /// the reason but does not double-emit the audit event (the second
    /// call returns Ok without writing).
    pub async fn cancel(&self, workflow_id: &str, reason: &str) -> anyhow::Result<()> {
        let instance = self.store.load(workflow_id).await?;
        if instance.cancelled_at.is_some() {
            // Already cancelled — idempotent no-op. Re-cancelling
            // shouldn't surprise callers (e.g. a retry loop). The
            // reason from the first cancel wins.
            return Ok(());
        }
        let expected_version = instance.version;
        let mut updated = instance.clone();
        updated.cancelled_at = Some(Utc::now());
        updated.cancelled_reason = Some(reason.to_string());
        // bump version so concurrent submits using stale `expected_version`
        // hit the version-conflict path rather than racing past cancel.
        updated.version = updated.version.saturating_add(1);
        let saved = self.store.save_if_version(updated, expected_version).await?;

        let event = saved
            .audit_event("workflow.cancelled")
            .with_payload(serde_json::json!({
                "reason": reason,
                "state_at_cancel": saved.state,
                "version_at_cancel": saved.version,
            }));
        self.record_or_self_event(event).await;
        Ok(())
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

    /// T25 — spawn a tokio watchdog that fires after the workflow's
    /// `timeoutMs` elapses and triggers the lazy-timeout path (which
    /// transitions to `onTimeout.target`, emits `workflow.timed_out`,
    /// and runs the existing deterministic-chain expansion). The
    /// watchdog is best-effort: if the workflow completes naturally
    /// before the timeout, the watchdog's `get()` call is cheap and
    /// observes the terminal state without re-firing. Lost watchdogs
    /// across process restarts are recovered on next get/submit via
    /// the existing lazy check — this active watchdog only matters
    /// for workflows that complete (or stall) without any caller
    /// touching them after `start`.
    ///
    /// Returns the spawned `JoinHandle` so callers can keep / abort
    /// it; the runtime itself doesn't track these (no Drop hook
    /// needed). For most callers — the gateway's MCP server, tests —
    /// the handle is dropped on the floor and the task self-cleans
    /// when it finishes.
    fn spawn_timeout_watchdog(
        &self,
        workflow_id: &str,
        timeout_ms: u64,
    ) -> tokio::task::JoinHandle<()> {
        let rt = self.clone();
        let wid = workflow_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)).await;
            // Triggering get() runs the existing lazy timeout check.
            // We swallow the result + any error — the watchdog is
            // observational, not assertive.
            let _ = rt
                .get(GetWorkflow {
                    workflow_id: wid,
                    principal: Principal::anonymous(),
                    trace_id: None,
                    run_id: None,
                })
                .await;
        })
    }

    pub async fn start(&self, request: StartWorkflow) -> anyhow::Result<Value> {
        if self.is_draining() {
            bail!("gateway is shutting down; please retry shortly");
        }

        // SPEC §32 — run_id uniqueness assertion. If the store indexes
        // run_id, reject duplicates with a structured error. Stores that
        // return Ok(None) by trait default opt out of the check; their
        // runtime sees no constraint (best-effort safety net).
        if let Some(run_id) = &request.run_id {
            if let Some(existing_workflow_id) =
                self.store.find_by_run_id(run_id).await?
            {
                return Err(RuntimeError::RunIdAlreadyRunning {
                    run_id: run_id.clone(),
                    existing_workflow_id,
                }
                .into());
            }
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
            cancelled_at: None,
            cancelled_reason: None,
        };
        let correlation_id = format!("cor_{}", Uuid::new_v4().simple());

        let instance = self.store.create(instance).await?;

        // T25 — spawn the timeout watchdog as soon as the instance
        // exists in the store. Definitions without `timeoutMs` (the
        // common case) skip this; the lazy check still covers any
        // workflow that does get touched after a notional deadline.
        if let Some(timeout_ms) = definition.get("timeoutMs").and_then(Value::as_u64) {
            // Fire-and-forget: the watchdog self-cleans when its
            // sleep + get returns. The JoinHandle is detached
            // intentionally — no Drop hook on the runtime needs to
            // abort it, and the workflow itself doesn't outlive the
            // process.
            drop(self.spawn_timeout_watchdog(&instance.id, timeout_ms));
        }

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
                                "method": "flowgate.command",
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
        // T24 — cancellation takes precedence over timeout. The
        // original state is preserved on the instance; the response's
        // `result.status` carries the cancelled signal so callers
        // (interpreter, LLM resume) see the workflow is terminal even
        // though its `state` field still names the recoverable position.
        if instance.cancelled_at.is_some() {
            let cancelled_payload = serde_json::json!({
                "cancelled_at":  instance.cancelled_at,
                "cancelled_reason": instance.cancelled_reason,
            });
            return Ok(self
                .response(
                    &definition,
                    &instance,
                    "cancelled",
                    Some(cancelled_payload),
                    &request.principal,
                )
                .await);
        }
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

    /// SPEC §22 — mirror of [`describe_guidance_for_workflow`] but reads
    /// from the instance's `_scriptsLibrary` snapshot. Returns `None` when
    /// the subject isn't in the snapshot (caller can then fall back to
    /// the live discovery index, but typically a script subject either
    /// belongs to a workflow's library or isn't visible to that workflow).
    pub async fn describe_script_for_workflow(
        &self,
        workflow_id: &str,
        subject: &str,
    ) -> anyhow::Result<Option<Value>> {
        let instance = self.store.load(workflow_id).await?;
        let Some(entry) = instance
            .definition
            .pointer("/_scriptsLibrary")
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
            "kind":      "script",
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
        //
        // SPEC §22.6 (v0.3) — for `kind: script` executors, the descriptor
        // additionally carries `subject` (the curated script subject) and
        // `hash` (the body hash from the workflow's pinned _scriptsLibrary
        // snapshot). Together they let audit replay pull the exact bytes
        // that ran by content-identity. Fields are additive + optional;
        // non-script executors get the legacy 3-field shape unchanged.
        let executor_cfg = params.transition_def.get("executor");
        let executor = executor_cfg
            .and_then(|e| e.get("kind").and_then(Value::as_str).map(|k| (k, e)))
            .map(|(kind, exec_cfg)| {
                let mut desc = json!({ "kind": kind });
                if let Some((ok, duration_ms)) = params.executor_outcome {
                    desc["ok"] = Value::Bool(ok);
                    desc["durationMs"] = json!(duration_ms);
                }
                if kind == "script" {
                    if let Some(subject) = exec_cfg.get("subject").and_then(Value::as_str) {
                        desc["subject"] = Value::String(subject.to_string());
                        // Snapshot lookup — JSON-pointer escape for `~` / `/`
                        // per RFC 6901. Subjects use `.` so escapes don't
                        // normally trigger; do it correctly anyway.
                        let escaped = subject.replace('~', "~0").replace('/', "~1");
                        if let Some(hash) = params
                            .instance
                            .definition
                            .pointer(&format!("/_scriptsLibrary/{escaped}/hash"))
                            .and_then(Value::as_str)
                        {
                            desc["hash"] = Value::String(hash.to_string());
                        }
                    }
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

        // SPEC §29 — lightweight transitions emit a different event
        // type so consumers can separate state-change records from
        // interaction-style self-loops (e.g. `ask_human`). The
        // `purpose:` field (when declared) propagates into the audit
        // payload for downstream filtering.
        let lightweight = params
            .transition_def
            .get("lightweight")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(purpose) = params.transition_def.get("purpose").and_then(Value::as_str) {
            record["purpose"] = Value::String(purpose.to_string());
        }
        let event_type = if lightweight {
            "workflow.interaction"
        } else {
            "workflow.transition"
        };

        let mut event = params
            .instance
            .audit_event(event_type)
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

