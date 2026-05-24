use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use serde::Serialize;

use crate::audit::{AuditEvent, AuditSink};
use crate::error::{ExecutorError, RuntimeError};
use crate::mapping::merge_output;
use crate::model::*;
use crate::ports::*;
use crate::reliability::{execute_with_reliability, ReliabilityPolicy};
pub(crate) use crate::runtime_schema::{apply_schema_defaults, required_str, validate_schema};
pub(crate) use crate::runtime_records::{blackboard_delta, validate_blackboard_writes};
pub use crate::runtime_links::is_terminal;
pub(crate) use crate::runtime_links::{
    collect_guidance_refs, empty_object_schema, link_filter_byguards, links, pointer_escape,
    transition_definition,
};
pub(crate) use crate::templating::render_template;

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
struct TransitionRecordParams<'a> {
    instance: &'a WorkflowInstance,
    from_state: &'a str,
    transition_name: &'a str,
    transition_def: &'a Value,
    actor: &'a str,
    principal: Option<&'a str>,
    arguments: &'a Value,
    blackboard_delta: Value,
    guard_results: Vec<Value>,
    child_workflow_id: Option<String>,
    /// `Some((ok, durationMs))` only when the executor actually ran on this
    /// transition. `None` for transitions without an `executor:` and for
    /// `onTimeout` records.
    executor_outcome: Option<(bool, u64)>,
    correlation_id: &'a str,
}

/// The workflow runtime. Holds Arcs of all ports so it can be cloned cheaply
/// and embedded in tool handlers.
#[derive(Clone)]
pub struct WorkflowRuntime {
    definitions: Arc<dyn DefinitionStore>,
    store: Arc<dyn WorkflowStore>,
    executors: Arc<dyn ExecutorRegistry>,
    guards: Arc<dyn GuardEvaluator>,
    audit: Arc<dyn AuditSink>,
    evidence: Option<Arc<dyn EvidenceStore>>,
    /// Set by the supervisor to refuse new `workflow.start` calls during a
    /// graceful drain. Existing `submit`/`get` keep working so in-flight work
    /// finishes cleanly. See `docs/CONFIG.md` "Zero-downtime config changes".
    draining: Arc<AtomicBool>,
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
        };
        let correlation_id = format!("cor_{}", Uuid::new_v4().simple());

        let instance = self.store.create(instance).await?;

        self.audit
            .record(
                AuditEvent::new("workflow.started")
                    .with_workflow(&instance.id)
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
                            AuditEvent::new("workflow.completed")
                                .with_workflow(&result.instance.id)
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

    pub async fn submit(&self, request: SubmitTransition) -> anyhow::Result<Value> {
        let instance = self.store.load(&request.workflow_id).await?;
        // In-flight: resolve the definition from the instance's carried
        // snapshot, never from the live `DefinitionStore` (SPEC §8.3).
        let definition = instance.definition.clone();

        let correlation_id = format!("cor_{}", Uuid::new_v4().simple());

        // Lazy timeout check: if more than `definition.timeoutMs` has elapsed
        // since start, fire onTimeout and short-circuit before the submit
        // gets validated / executed.
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

        self.audit
            .record(
                AuditEvent::new("transition.requested")
                    .with_workflow(&instance.id)
                    .with_correlation(&correlation_id)
                    .with_actor(&request.principal.subject)
                    .with_payload(json!({
                        "transition": request.transition,
                        "expectedVersion": request.expected_version,
                        "fromState": instance.state,
                    })),
            )
            .await?;

        if instance.version != request.expected_version {
            return Ok(self
                .record_rejected(
                    &definition,
                    &instance,
                    "STALE_WORKFLOW_VERSION",
                    format!(
                        "Expected workflow version {}, but current version is {}.",
                        request.expected_version, instance.version
                    ),
                    &request.transition,
                    &correlation_id,
                    &request.principal,
                )
                .await);
        }

        let transition =
            match transition_definition(&definition, &instance.state, &request.transition) {
                Some(value) => value.clone(),
                None => {
                    return Ok(self
                        .record_rejected(
                            &definition,
                            &instance,
                            "INVALID_TRANSITION",
                            format!(
                                "Transition '{}' is not valid from state '{}'.",
                                request.transition, instance.state
                            ),
                            &request.transition,
                            &correlation_id,
                            &request.principal,
                        )
                        .await);
                }
            };

        // Actor gate. A transition tagged `actor: "human"` requires the
        // submitter to be a human principal (see `Principal::is_human`).
        // Closes the loophole where an agent could call a human-only
        // transition directly even though no agent-actor link was ever
        // offered. Other actor values (`agent`, missing, custom) impose
        // no submit-time check — humans can drive agent transitions, and
        // executor-layer behaviour (e.g. the `human` executor stopping
        // state advancement) remains the second line of defence.
        if transition.get("actor").and_then(Value::as_str) == Some("human")
            && !request.principal.is_human()
        {
            return Ok(self
                .record_rejected(
                    &definition,
                    &instance,
                    "ACTOR_MISMATCH",
                    format!(
                        "Transition '{}' requires a human principal; \
                         submitter '{}' has no '{}' role.",
                        request.transition,
                        request.principal.subject,
                        Principal::HUMAN_ROLE
                    ),
                    &request.transition,
                    &correlation_id,
                    &request.principal,
                )
                .await);
        }

        let mut arguments = request.arguments;
        apply_schema_defaults(transition.pointer("/inputSchema"), &mut arguments);
        if let Err(err) = validate_schema(
            transition.pointer("/inputSchema"),
            &arguments,
            "transition input",
        ) {
            return Ok(self
                .record_rejected(
                    &definition,
                    &instance,
                    "INPUT_SCHEMA_VIOLATION",
                    err.to_string(),
                    &request.transition,
                    &correlation_id,
                    &request.principal,
                )
                .await);
        }

        let (guards_ok, guard_results) = match self
            .guards_pass(
                &transition,
                &instance,
                &arguments,
                &request.principal,
                &correlation_id,
            )
            .await
        {
            Ok(pair) => pair,
            Err(err) => {
                // SPEC §9: a guard hitting an unset slot must fail fast with
                // rich context, not a silent `false`. The runtime is the
                // backstop here even when static `check` would have caught
                // it. Other guard evaluator failures still propagate as
                // anyhow errors (executor/audit/etc. — not a SPEC-classified
                // rejection).
                if let Some(unset) = err.downcast_ref::<crate::guards::UnsetSlotError>() {
                    return Ok(self
                        .record_rejected(
                            &definition,
                            &instance,
                            "GUARD_UNSET_SLOT",
                            unset.to_string(),
                            &request.transition,
                            &correlation_id,
                            &request.principal,
                        )
                        .await);
                }
                return Err(err);
            }
        };
        if !guards_ok {
            return Ok(self
                .record_rejected(
                    &definition,
                    &instance,
                    "GUARD_REJECTED",
                    "One or more guards rejected the transition.".to_string(),
                    &request.transition,
                    &correlation_id,
                    &request.principal,
                )
                .await);
        }

        let mut next = instance.clone();
        let mut accumulated_evidence: Vec<Evidence> = Vec::new();
        let mut child_workflow_id: Option<String> = None;
        let mut executor_outcome: Option<(bool, u64)> = None;

        if let Some(executor_config) = transition.get("executor") {
            let policy = ReliabilityPolicy::from_value(transition.get("reliability"));
            let exec_started = std::time::Instant::now();
            match execute_with_reliability(
                self.executors.as_ref(),
                &self.audit,
                &next,
                Some(&request.transition),
                &arguments,
                executor_config.clone(),
                &policy,
                &correlation_id,
            )
            .await
            {
                Ok(result) => {
                    executor_outcome =
                        Some((true, exec_started.elapsed().as_millis() as u64));
                    merge_output(
                        &mut next.context,
                        transition.get("output"),
                        &arguments,
                        &next.input,
                        &result.output,
                    )?;
                    // SPEC §6.2: typed blackboard slots are validated *before*
                    // the transition advances. A mismatch aborts here so the
                    // caller sees BLACKBOARD_TYPE_ERROR and the snapshot stays
                    // at the pre-transition version.
                    if let Err((slot, reason)) = validate_blackboard_writes(
                        &definition,
                        transition.get("output"),
                        &next.context,
                    ) {
                        return Ok(self
                            .record_rejected(
                                &definition,
                                &instance,
                                "BLACKBOARD_TYPE_ERROR",
                                format!("output write to typed slot '{slot}': {reason}"),
                                &request.transition,
                                &correlation_id,
                                &request.principal,
                            )
                            .await);
                    }
                    child_workflow_id = result.child_workflow_id.clone();
                    accumulated_evidence.extend(result.evidence);
                }
                Err(err) => {
                    self.audit
                        .record(
                            AuditEvent::new("transition.rejected")
                                .with_workflow(&instance.id)
                                .with_correlation(&correlation_id)
                                .with_actor(&request.principal.subject)
                                .with_payload(json!({
                                    "transition": request.transition,
                                    "code": "EXECUTOR_FAILED",
                                    "errorClass": err.class().token(),
                                    "message": err.to_string(),
                                })),
                        )
                        .await?;
                    return Ok(self
                        .failed_response(
                            &definition,
                            &instance,
                            &err,
                            &request.transition,
                            &request.principal,
                        )
                        .await);
                }
            }
        }

        // SPEC §6.3 — write the optional model-authored summary to
        // `context.summary`. Reserved slot; never a guard input (`check`
        // errors on guards reading it); surfaced in every response.
        if let Some(summary) = &request.summary {
            if let Some(ctx) = next.context.as_object_mut() {
                ctx.insert("summary".into(), Value::String(summary.clone()));
            }
        }

        // Pick the destination state. By default it's the transition's
        // `target`, but `branches: [{ when, target }]` can override based on
        // the executor's result and the post-output context. First branch
        // whose `when` guard passes wins; otherwise the declared target.
        let from_state = next.state.clone();
        let target = self
            .resolve_target(
                &transition,
                &next,
                &arguments,
                &request.principal,
                &correlation_id,
            )
            .await?;
        next.state = target;
        next.version += 1;

        // Record-first: emit the transition record BEFORE committing the
        // snapshot. The transition's declared `actor` (default `agent`) is the
        // record's actor; `deterministic`/`system` actors carry a null
        // principal, others carry the submitter's subject. If the record write
        // fails we abort here and never call `save_if_version`, so the
        // instance version stays unchanged.
        let actor = transition
            .get("actor")
            .and_then(Value::as_str)
            .unwrap_or("agent");
        let principal = if actor == "deterministic" || actor == "system" {
            None
        } else {
            Some(request.principal.subject.as_str())
        };
        let delta = blackboard_delta(&instance.context, &next.context);
        self.emit_transition_record(TransitionRecordParams {
            instance: &next,
            from_state: &from_state,
            transition_name: &request.transition,
            transition_def: &transition,
            actor,
            principal,
            arguments: &arguments,
            blackboard_delta: delta,
            guard_results,
            child_workflow_id,
            executor_outcome,
            correlation_id: &correlation_id,
        })
        .await?;

        let next = self
            .store
            .save_if_version(next, request.expected_version)
            .await?;

        // Persist accumulated evidence so subsequent `evidence` guards can
        // see it. Failures are logged but don't fail the transition — audit
        // is the ground truth for what happened.
        if let Some(estore) = &self.evidence {
            for ev in &accumulated_evidence {
                if let Err(e) = estore.record(&next.id, ev.clone()).await {
                    tracing::warn!(workflow = %next.id, error = %e, "evidence record failed");
                }
            }
        }

        let next = self
            .run_on_enter(definition.clone(), next, &correlation_id)
            .await?;

        self.audit
            .record(
                AuditEvent::new("workflow.transitioned")
                    .with_workflow(&next.id)
                    .with_correlation(&correlation_id)
                    .with_actor(&request.principal.subject)
                    .with_payload(json!({
                        "transition": request.transition,
                        "state": next.state,
                        "version": next.version,
                    })),
            )
            .await?;

        // Run deterministic chain from the new state
        let max_depth = definition
            .get("maxChainDepth")
            .and_then(Value::as_u64)
            .unwrap_or(50);
        let chain_outcome = self
            .run_deterministic_chain(
                &definition,
                next,
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
                            AuditEvent::new("workflow.completed")
                                .with_workflow(&result.instance.id)
                                .with_correlation(&correlation_id)
                                .with_payload(json!({ "state": result.instance.state })),
                        )
                        .await?;
                }

                let mut response = self
                    .response(
                        &definition,
                        &result.instance,
                        "executed",
                        None,
                        &request.principal,
                    )
                    .await;
                // Merge evidence from submit + chain
                let mut all_evidence = accumulated_evidence;
                all_evidence.extend(result.evidence);
                if !all_evidence.is_empty() {
                    response["evidence"] = serde_json::to_value(&all_evidence)?;
                }
                if !result.steps.is_empty() {
                    response["chain"] = serde_json::to_value(&result.steps)?;
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
                let mut all_evidence = accumulated_evidence;
                all_evidence.extend(partial.evidence);
                if !all_evidence.is_empty() {
                    response["evidence"] = serde_json::to_value(&all_evidence)?;
                }
                if !partial.steps.is_empty() {
                    response["chain"] = serde_json::to_value(&partial.steps)?;
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
        Ok(Some(json!({
            "kind": "guidance",
            "subject": subject,
            "verb": verb,
            "body": body,
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
    async fn emit_transition_record(
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

        let mut event = AuditEvent::new("workflow.transition")
            .with_workflow(&params.instance.id)
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

    /// Resolve the destination state for a successful transition. If
    /// `branches: [{when, target}]` is declared, evaluate each `when`
    /// guard against the post-execute state and return the first match's
    /// target. Otherwise fall back to the transition's `target` field.
    /// Emits a `transition.branched` audit event when a branch fires so
    /// it's clear in logs which branch the runtime took.
    async fn resolve_target(
        &self,
        transition: &Value,
        instance: &WorkflowInstance,
        arguments: &Value,
        principal: &Principal,
        correlation_id: &str,
    ) -> anyhow::Result<String> {
        let default_target = required_str(transition, "/target")?.to_owned();
        let Some(branches) = transition.get("branches").and_then(Value::as_array) else {
            return Ok(default_target);
        };
        for (idx, branch) in branches.iter().enumerate() {
            let Some(when) = branch.get("when") else {
                continue;
            };
            let Some(branch_target) = branch.get("target").and_then(Value::as_str) else {
                continue;
            };
            let pass = self
                .guards
                .evaluate(when, instance, arguments, principal)
                .await?;
            if pass {
                let _ = self
                    .audit
                    .record(
                        AuditEvent::new("transition.branched")
                            .with_workflow(&instance.id)
                            .with_correlation(correlation_id)
                            .with_actor(&principal.subject)
                            .with_payload(json!({
                                "branchIndex": idx,
                                "fromState": instance.state,
                                "toState": branch_target,
                            })),
                    )
                    .await;
                return Ok(branch_target.to_string());
            }
        }
        Ok(default_target)
    }

    /// Lazy workflow-level timeout check. If `definition.timeoutMs` is
    /// declared and the wall-clock interval since `instance.started_at`
    /// exceeds it, advance the workflow to `definition.onTimeout.target`
    /// and emit a `workflow.timed_out` audit event. Returns `Some(updated)`
    /// when a timeout fired (caller should respond from that snapshot),
    /// `None` otherwise.
    async fn check_and_apply_timeout(
        &self,
        definition: &Value,
        mut instance: WorkflowInstance,
        principal: &Principal,
    ) -> anyhow::Result<Option<WorkflowInstance>> {
        let Some(timeout_ms) = definition.get("timeoutMs").and_then(Value::as_u64) else {
            return Ok(None);
        };
        // If the workflow already reached a terminal state, no timeout to apply.
        if is_terminal(definition, &instance.state) {
            return Ok(None);
        }
        let elapsed = Utc::now()
            .signed_duration_since(instance.started_at)
            .num_milliseconds();
        if elapsed < 0 || (elapsed as u64) < timeout_ms {
            return Ok(None);
        }

        let target = match definition
            .pointer("/onTimeout/target")
            .and_then(Value::as_str)
        {
            Some(t) => t.to_string(),
            // Without a declared onTimeout, the workflow can't recover
            // declaratively. Audit the timeout but leave the instance alone
            // so the caller still gets a meaningful `failed`-style response.
            None => {
                let _ = self
                    .audit
                    .record(
                        AuditEvent::new("workflow.timed_out")
                            .with_workflow(&instance.id)
                            .with_actor(&principal.subject)
                            .with_payload(json!({
                                "elapsedMs": elapsed,
                                "timeoutMs": timeout_ms,
                                "fromState": instance.state,
                                "applied": false,
                            })),
                    )
                    .await;
                return Ok(None);
            }
        };

        let from_state = instance.state.clone();
        let expected_version = instance.version;
        instance.state = target.clone();
        instance.version += 1;

        // Record-first: emit the `workflow.transition` record BEFORE committing
        // the timeout state change. If the record write fails, leave the workflow
        // unchanged so the next timeout check retries it.
        let correlation_id = format!("cor_{}", Uuid::new_v4().simple());
        let transition_name = definition
            .pointer("/onTimeout/transition")
            .and_then(Value::as_str)
            .unwrap_or("onTimeout");
        let on_timeout_def = definition
            .pointer("/onTimeout")
            .cloned()
            .unwrap_or(Value::Null);
        if let Err(e) = self
            .emit_transition_record(TransitionRecordParams {
                instance: &instance,
                from_state: &from_state,
                transition_name,
                transition_def: &on_timeout_def,
                actor: "system",
                principal: None,
                arguments: &json!({}),
                blackboard_delta: Value::Object(serde_json::Map::new()),
                guard_results: Vec::new(),
                child_workflow_id: None,
                executor_outcome: None,
                correlation_id: &correlation_id,
            })
            .await
        {
            tracing::warn!(
                workflow = %instance.id,
                error = %e,
                "timeout transition record failed — skipping state commit to allow retry"
            );
            return Ok(None);
        }

        let saved = self
            .store
            .save_if_version(instance, expected_version)
            .await?;

        let _ = self
            .audit
            .record(
                AuditEvent::new("workflow.timed_out")
                    .with_workflow(&saved.id)
                    .with_correlation(&correlation_id)
                    .with_actor(&principal.subject)
                    .with_payload(json!({
                        "elapsedMs": elapsed,
                        "timeoutMs": timeout_ms,
                        "fromState": from_state,
                        "toState": target,
                        "applied": true,
                    })),
            )
            .await;
        Ok(Some(saved))
    }

    async fn run_on_enter(
        &self,
        definition: Value,
        mut instance: WorkflowInstance,
        correlation_id: &str,
    ) -> anyhow::Result<WorkflowInstance> {
        let path = format!("/states/{}/onEnter", pointer_escape(&instance.state));
        let Some(on_enter) = definition.pointer(&path).cloned() else {
            return Ok(instance);
        };

        let Some(executor_config) = on_enter.get("executor") else {
            return Ok(instance);
        };

        let policy = ReliabilityPolicy::from_value(on_enter.get("reliability"));
        let result = execute_with_reliability(
            self.executors.as_ref(),
            &self.audit,
            &instance,
            None,
            &json!({}),
            executor_config.clone(),
            &policy,
            correlation_id,
        )
        .await
        .map_err(|e| anyhow!("onEnter executor failed: {e}"))?;

        let on_enter_input = instance.input.clone();
        merge_output(
            &mut instance.context,
            on_enter.get("output"),
            &json!({}),
            &on_enter_input,
            &result.output,
        )?;
        if let Err((slot, reason)) = validate_blackboard_writes(
            &definition,
            on_enter.get("output"),
            &instance.context,
        ) {
            bail!("BLACKBOARD_TYPE_ERROR: onEnter output write to typed slot '{slot}': {reason}");
        }

        if let Some(estore) = &self.evidence {
            for ev in &result.evidence {
                if let Err(e) = estore.record(&instance.id, ev.clone()).await {
                    tracing::warn!(workflow = %instance.id, error = %e, "evidence record failed");
                }
            }
        }

        let expected_version = instance.version;
        instance.version += 1;
        self.store.save_if_version(instance, expected_version).await
    }

    // -----------------------------------------------------------------------
    // Deterministic chaining
    // -----------------------------------------------------------------------

    /// Run a deterministic chain starting from the current state. Keeps
    /// executing `actor: "deterministic"` transitions automatically until
    /// a decision point (any non-deterministic transition), terminal state,
    /// depth limit, or failure is reached.
    ///
    /// Returns a `ChainOutcome` — either `Completed` (normal stop) or
    /// `Failed` (executor/guard error with partial progress).
    async fn run_deterministic_chain(
        &self,
        definition: &Value,
        mut instance: WorkflowInstance,
        principal: &Principal,
        correlation_id: &str,
        max_depth: u64,
    ) -> anyhow::Result<ChainOutcome> {
        let mut steps: Vec<ChainStep> = Vec::new();
        let mut accumulated_evidence: Vec<Evidence> = Vec::new();

        loop {
            // Stop: terminal state
            if is_terminal(definition, &instance.state) {
                break;
            }

            // Stop: depth limit
            if steps.len() as u64 >= max_depth {
                break;
            }

            // Gather transitions for current state
            let transitions_path =
                format!("/states/{}/transitions", pointer_escape(&instance.state));
            let Some(transitions) = definition
                .pointer(&transitions_path)
                .and_then(Value::as_object)
            else {
                break; // No transitions defined
            };

            // Collect deterministic transitions
            let deterministic: Vec<(&String, &Value)> = transitions
                .iter()
                .filter(|(_, t)| t.get("actor").and_then(Value::as_str) == Some("deterministic"))
                .collect();

            // Stop: if ANY transition is non-deterministic, this is a
            // decision point for the LLM/human.
            if deterministic.len() != transitions.len() {
                break;
            }

            // Stop: no transitions at all
            if deterministic.is_empty() {
                break;
            }

            // Select which deterministic transition to execute
            let (transition_name, transition_def) = match self
                .select_deterministic_transition(
                    &deterministic,
                    &instance,
                    principal,
                    correlation_id,
                )
                .await
            {
                Ok(selected) => selected,
                Err(e) => {
                    let _ = self
                        .audit
                        .record(
                            AuditEvent::new("chain.failed")
                                .with_workflow(&instance.id)
                                .with_correlation(correlation_id)
                                .with_payload(json!({
                                    "fromState": instance.state,
                                    "chainDepth": steps.len(),
                                    "errorClass": "selection_error",
                                    "message": e.to_string(),
                                })),
                        )
                        .await;
                    return Ok(ChainOutcome::Failed {
                        failed_transition: String::new(),
                        error: e.to_string(),
                        error_class: "selection_error".into(),
                        partial: ChainResult {
                            instance,
                            steps,
                            evidence: accumulated_evidence,
                        },
                    });
                }
            };

            let from_state = instance.state.clone();

            // Audit: chain step beginning
            let _ = self
                .audit
                .record(
                    AuditEvent::new("chain.step")
                        .with_workflow(&instance.id)
                        .with_correlation(correlation_id)
                        .with_payload(json!({
                            "transition": transition_name,
                            "fromState": from_state,
                            "chainDepth": steps.len(),
                        })),
                )
                .await;

            // Snapshot pre-merge context so the transition record can carry
            // an accurate blackboardDelta (SPEC §7.2). Cheap clone — context
            // is bounded.
            let pre_context = instance.context.clone();
            let mut chain_child_workflow_id: Option<String> = None;
            let mut chain_executor_outcome: Option<(bool, u64)> = None;

            // Execute the transition's executor (if present)
            if let Some(executor_config) = transition_def.get("executor") {
                let policy = ReliabilityPolicy::from_value(transition_def.get("reliability"));
                let exec_started = std::time::Instant::now();
                match execute_with_reliability(
                    self.executors.as_ref(),
                    &self.audit,
                    &instance,
                    Some(&transition_name),
                    &json!({}), // deterministic transitions take no LLM arguments
                    executor_config.clone(),
                    &policy,
                    correlation_id,
                )
                .await
                {
                    Ok(result) => {
                        chain_executor_outcome =
                            Some((true, exec_started.elapsed().as_millis() as u64));
                        merge_output(
                            &mut instance.context,
                            transition_def.get("output"),
                            &json!({}),
                            &instance.input,
                            &result.output,
                        )?;
                        chain_child_workflow_id = result.child_workflow_id.clone();
                        if let Err((slot, reason)) = validate_blackboard_writes(
                            definition,
                            transition_def.get("output"),
                            &instance.context,
                        ) {
                            let message = format!(
                                "BLACKBOARD_TYPE_ERROR: output write to typed slot '{slot}': {reason}"
                            );
                            let _ = self
                                .audit
                                .record(
                                    AuditEvent::new("chain.failed")
                                        .with_workflow(&instance.id)
                                        .with_correlation(correlation_id)
                                        .with_payload(json!({
                                            "transition": transition_name,
                                            "fromState": from_state,
                                            "chainDepth": steps.len(),
                                            "code": "BLACKBOARD_TYPE_ERROR",
                                            "message": message,
                                        })),
                                )
                                .await;
                            return Ok(ChainOutcome::Failed {
                                failed_transition: transition_name,
                                error: message,
                                error_class: "blackboard_type_error".to_string(),
                                partial: ChainResult {
                                    instance,
                                    steps,
                                    evidence: accumulated_evidence,
                                },
                            });
                        }
                        accumulated_evidence.extend(result.evidence);
                    }
                    Err(err) => {
                        let _ = self
                            .audit
                            .record(
                                AuditEvent::new("chain.failed")
                                    .with_workflow(&instance.id)
                                    .with_correlation(correlation_id)
                                    .with_payload(json!({
                                        "transition": transition_name,
                                        "fromState": from_state,
                                        "chainDepth": steps.len(),
                                        "errorClass": err.class().token(),
                                        "message": err.to_string(),
                                    })),
                            )
                            .await;
                        return Ok(ChainOutcome::Failed {
                            failed_transition: transition_name,
                            error: err.to_string(),
                            error_class: err.class().token().to_string(),
                            partial: ChainResult {
                                instance,
                                steps,
                                evidence: accumulated_evidence,
                            },
                        });
                    }
                }
            }

            // Resolve target state (auto-branching)
            let target = self
                .resolve_target(
                    &transition_def,
                    &instance,
                    &json!({}),
                    principal,
                    correlation_id,
                )
                .await?;

            let expected_version = instance.version;
            instance.state = target.clone();
            instance.version += 1;

            // Record-first: emit the transition record for this chain hop
            // BEFORE committing the snapshot. Deterministic transitions carry a
            // null principal. A record-write failure aborts the whole chain
            // before `save_if_version`, so the instance version stays unchanged.
            let delta = blackboard_delta(&pre_context, &instance.context);
            self.emit_transition_record(TransitionRecordParams {
                instance: &instance,
                from_state: &from_state,
                transition_name: &transition_name,
                transition_def: &transition_def,
                actor: "deterministic",
                principal: None,
                arguments: &json!({}),
                blackboard_delta: delta,
                guard_results: Vec::new(),
                child_workflow_id: chain_child_workflow_id,
                executor_outcome: chain_executor_outcome,
                correlation_id,
            })
            .await?;

            instance = self
                .store
                .save_if_version(instance, expected_version)
                .await?;

            // Persist evidence
            if let Some(estore) = &self.evidence {
                for ev in &accumulated_evidence {
                    if let Err(e) = estore.record(&instance.id, ev.clone()).await {
                        tracing::warn!(
                            workflow = %instance.id, error = %e,
                            "evidence record failed during chain"
                        );
                    }
                }
            }

            // Record the step
            steps.push(ChainStep {
                from_state: from_state.clone(),
                transition: transition_name.clone(),
                to_state: target.clone(),
                version: instance.version,
            });

            // Audit: transition completed
            let _ = self
                .audit
                .record(
                    AuditEvent::new("workflow.transitioned")
                        .with_workflow(&instance.id)
                        .with_correlation(correlation_id)
                        .with_actor(&principal.subject)
                        .with_payload(json!({
                            "transition": transition_name,
                            "state": instance.state,
                            "version": instance.version,
                            "deterministic": true,
                            "chainDepth": steps.len(),
                        })),
                )
                .await;

            // Run onEnter for the new state
            instance = self
                .run_on_enter(definition.clone(), instance, correlation_id)
                .await?;

            // Check lazy timeout
            if let Some(timeout_ms) = definition.get("timeoutMs").and_then(Value::as_u64) {
                let elapsed = Utc::now()
                    .signed_duration_since(instance.started_at)
                    .num_milliseconds();
                if elapsed >= 0 && (elapsed as u64) >= timeout_ms {
                    break;
                }
            }
        }

        // Emit chain.completed if any steps were taken
        if !steps.is_empty() {
            let _ = self
                .audit
                .record(
                    AuditEvent::new("chain.completed")
                        .with_workflow(&instance.id)
                        .with_correlation(correlation_id)
                        .with_payload(json!({
                            "steps": steps.len(),
                            "finalState": instance.state,
                        })),
                )
                .await;
        }

        Ok(ChainOutcome::Completed(ChainResult {
            instance,
            steps,
            evidence: accumulated_evidence,
        }))
    }

    /// Select which deterministic transition to execute when a state has
    /// one or more. With a single candidate, it's returned directly. With
    /// multiple, guards are evaluated and exactly one must pass.
    async fn select_deterministic_transition(
        &self,
        candidates: &[(&String, &Value)],
        instance: &WorkflowInstance,
        principal: &Principal,
        correlation_id: &str,
    ) -> anyhow::Result<(String, Value)> {
        if candidates.len() == 1 {
            let (name, def) = candidates[0];
            return Ok((name.clone(), (*def).clone()));
        }

        // Multiple candidates: evaluate guards to select
        let mut viable = Vec::new();
        for (name, def) in candidates {
            let guards = def
                .get("guards")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            let mut all_pass = true;
            for guard in &guards {
                let pass = self
                    .guards
                    .evaluate(guard, instance, &json!({}), principal)
                    .await?;
                let _ = self
                    .audit
                    .record(
                        AuditEvent::new("guard.evaluated")
                            .with_workflow(&instance.id)
                            .with_correlation(correlation_id)
                            .with_payload(json!({
                                "guard": guard,
                                "passed": pass,
                                "context": "deterministic_selection",
                            })),
                    )
                    .await;
                if !pass {
                    all_pass = false;
                    break;
                }
            }

            if all_pass {
                viable.push(((*name).clone(), (*def).clone()));
            }
        }

        match viable.len() {
            0 => bail!(
                "no viable deterministic transition in state '{}': \
                 all {} candidates had failing guards",
                instance.state,
                candidates.len()
            ),
            1 => Ok(viable.into_iter().next().unwrap()),
            n => bail!(
                "ambiguous deterministic transition in state '{}': \
                 {} of {} candidates had passing guards; \
                 exactly one must be viable",
                instance.state,
                n,
                candidates.len()
            ),
        }
    }

    /// Evaluate every guard on a transition in declaration order. Returns
    /// `(overall_pass, evaluated)` where `evaluated` is the SPEC §7.2 record
    /// shape `[{kind, result}, …]` covering every guard actually checked.
    /// Evaluation short-circuits on the first failure — `evaluated` includes
    /// that failing guard but not any after it.
    async fn guards_pass(
        &self,
        transition: &Value,
        instance: &WorkflowInstance,
        arguments: &Value,
        principal: &Principal,
        correlation_id: &str,
    ) -> anyhow::Result<(bool, Vec<Value>)> {
        let guards = transition
            .get("guards")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut evaluated = Vec::with_capacity(guards.len());
        for (idx, guard) in guards.iter().enumerate() {
            let pass = self
                .guards
                .evaluate(guard, instance, arguments, principal)
                .await?;
            let kind = guard
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            evaluated.push(json!({ "kind": kind, "result": pass }));
            self.audit
                .record(
                    AuditEvent::new("guard.evaluated")
                        .with_workflow(&instance.id)
                        .with_correlation(correlation_id)
                        .with_payload(json!({
                            "guardIndex": idx,
                            "guard": guard,
                            "passed": pass,
                        })),
                )
                .await?;
            if !pass {
                return Ok((false, evaluated));
            }
        }

        Ok((true, evaluated))
    }

    /// Build the response body, including link filtering when the workflow
    /// or state declares `linkFilter: byGuards`. Always evaluated against
    /// the provided principal so "what could THIS caller do next" is what
    /// surfaces.
    async fn response(
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

    async fn invalid_response(
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
    async fn record_rejected(
        &self,
        definition: &Value,
        instance: &WorkflowInstance,
        code: &str,
        message: String,
        attempted_transition: &str,
        correlation_id: &str,
        principal: &Principal,
    ) -> Value {
        let _ = self
            .audit
            .record(
                AuditEvent::new("transition.rejected")
                    .with_workflow(&instance.id)
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

    async fn failed_response(
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

// ---------------------------------------------------------------------------
// Guidance refs (SPEC v2 §5.5)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Guidance string templating (SPEC v2 §5.2)
// ---------------------------------------------------------------------------

