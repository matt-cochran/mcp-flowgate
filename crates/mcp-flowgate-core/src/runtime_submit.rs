//! `submit` entry point for [`WorkflowRuntime`]. The 455-LOC submit method
//! plus its lifecycle audit lives here; the type definition and other
//! entry points (`start`, `get`) remain in `runtime.rs`. All methods
//! share the same `impl WorkflowRuntime` block split across sibling files.

use anyhow::bail;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::mapping::merge_output;
use crate::model::{Evidence, Principal, SubmitTransition};
use crate::reliability::{execute_with_reliability, ReliabilityPolicy};
use crate::runtime::{ChainOutcome, TransitionRecordParams, WorkflowRuntime};
use crate::runtime_links::{empty_object_schema, is_terminal, transition_definition};
use crate::runtime_records::{blackboard_delta, validate_blackboard_writes};
use crate::runtime_schema::{apply_schema_defaults, validate_schema};

impl WorkflowRuntime {
    pub async fn submit(&self, request: SubmitTransition) -> anyhow::Result<Value> {
        let instance = self.store.load(&request.workflow_id).await?;
        // In-flight: resolve the definition from the instance's carried
        // snapshot, never from the live `DefinitionStore` (SPEC §8.3).
        let definition = instance.definition.clone();

        let correlation_id = format!("cor_{}", Uuid::new_v4().simple());

        // T24 — cancelled workflows refuse submit. The caller sees
        // WORKFLOW_CANCELLED with the original reason in the error
        // body so retry loops don't loop forever.
        if let Some(cancelled_at) = instance.cancelled_at {
            bail!(
                "WORKFLOW_CANCELLED: workflow {} was cancelled at {} (reason: {})",
                request.workflow_id,
                cancelled_at,
                instance.cancelled_reason.as_deref().unwrap_or("(none)"),
            );
        }

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
                instance
                    .audit_event("transition.requested")
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

        // SPEC §29 — generic per-state fire cap. A transition may declare
        // `max_fires_per_visit: N` to bound how many times it can fire
        // before the workflow advances to a different state. Counter
        // lives in synthetic context slot `_fire_count.<state>.<transition>`
        // and resets on state exit (handled in clear_state_local_slots_on_exit
        // — synthetic slots whose state matches the leaving state get
        // scrubbed). Useful for `ask_human` self-loops (prevent agent
        // spamming) but generic — applies to any transition.
        if let Some(max_fires) = transition
            .get("max_fires_per_visit")
            .and_then(Value::as_u64)
        {
            let key = format!(
                "_fire_count.{}.{}",
                instance.state, request.transition
            );
            let current = instance
                .context
                .get(&key)
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if current >= max_fires {
                return Ok(self
                    .record_rejected(
                        &definition,
                        &instance,
                        "TRANSITION_FIRE_CAP_EXCEEDED",
                        format!(
                            "Transition '{}' has fired {} times in state '{}' \
                             (max_fires_per_visit = {}). Cap is per-state-entry \
                             and resets when the workflow advances. Either raise \
                             the cap, or have the workflow advance to a different \
                             state before re-firing.",
                            request.transition,
                            current,
                            instance.state,
                            max_fires
                        ),
                        &request.transition,
                        &correlation_id,
                        &request.principal,
                    )
                    .await);
            }
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

        let outcome = match self
            .guards_pass(
                &transition,
                &instance,
                &arguments,
                &request.principal,
                &correlation_id,
            )
            .await
        {
            Ok(o) => o,
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
        let guard_results = outcome.evaluated;
        if !outcome.pass {
            // SPEC §20.4 — when a §20.1 filter (require_digest /
            // min_confidence) attributed the rejection, surface the
            // specific code so callers can distinguish it from generic
            // GUARD_REJECTED.
            let (code, msg) = match outcome.diagnostic.as_deref() {
                Some("EVIDENCE_DIGEST_REQUIRED") => (
                    "EVIDENCE_DIGEST_REQUIRED",
                    "Evidence guard quorum failed: a `require_digest: true` \
                     clause excluded records missing a content digest."
                        .to_string(),
                ),
                Some("EVIDENCE_CONFIDENCE_BELOW_THRESHOLD") => (
                    "EVIDENCE_CONFIDENCE_BELOW_THRESHOLD",
                    "Evidence guard quorum failed: a `min_confidence` clause \
                     excluded records whose confidence was below threshold \
                     (or missing entirely)."
                        .to_string(),
                ),
                _ => (
                    "GUARD_REJECTED",
                    "One or more guards rejected the transition.".to_string(),
                ),
            };
            return Ok(self
                .record_rejected(
                    &definition,
                    &instance,
                    code,
                    msg,
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

                    // SPEC §28: declarative slot constraints evaluated at
                    // write-time. Catches violations at the agent's edit
                    // site, not at downstream guard read. Compose with
                    // typed-schema validation above (which handles regex /
                    // min / max / length / enum); §28 adds the things
                    // JSON Schema can't express (path_allowlist,
                    // subset_of dynamic reference).
                    if let Err(v) = crate::slot_constraint::evaluate_constraints(
                        &definition,
                        &instance.state,
                        &next.context,
                    ) {
                        return Ok(self
                            .record_rejected(
                                &definition,
                                &instance,
                                "SLOT_CONSTRAINT_VIOLATED",
                                v.message,
                                &request.transition,
                                &correlation_id,
                                &request.principal,
                            )
                            .await);
                    }
                    child_workflow_id = result.child_workflow_id.clone();
                    // SPEC §20.1 — validate every evidence record's
                    // confidence range BEFORE accepting it into the
                    // workflow's accumulated evidence. Out-of-range
                    // values fail-fast with INVALID_CONFIDENCE rather
                    // than poisoning downstream guards.
                    for ev in &result.evidence {
                        if let Err(bad) = ev.validate_confidence() {
                            return Ok(self
                                .record_rejected(
                                    &definition,
                                    &instance,
                                    "INVALID_CONFIDENCE",
                                    format!(
                                        "Evidence record (kind='{}', id='{}') has \
                                         confidence={} outside the allowed range \
                                         0.0..=1.0 (SPEC §20.1).",
                                        ev.kind, ev.id, bad
                                    ),
                                    &request.transition,
                                    &correlation_id,
                                    &request.principal,
                                )
                                .await);
                        }
                    }
                    accumulated_evidence.extend(result.evidence);
                }
                Err(err) => {
                    self.audit
                        .record(
                            instance
                                .audit_event("transition.rejected")
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

        // SPEC §29 — increment per-state fire counter on successful fire.
        // The pre-check at the top of submit consults this counter to
        // enforce `max_fires_per_visit`. Stored in synthetic context
        // slot `_fire_count.<state>.<transition>`; scrubbed on state exit.
        if transition
            .get("max_fires_per_visit")
            .and_then(Value::as_u64)
            .is_some()
        {
            let key = format!(
                "_fire_count.{}.{}",
                instance.state, request.transition
            );
            if let Some(ctx) = next.context.as_object_mut() {
                let n = ctx
                    .get(&key)
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .saturating_add(1);
                ctx.insert(key, json!(n));
            }
        }

        // Pick the destination state. By default it's the transition's
        // `target`, but `branches: [{ when, target }]` can override based on
        // the executor's result and the post-output context. First branch
        // whose `when` guard passes wins; otherwise the declared target.
        let from_state = next.state.clone();
        let mut target = self
            .resolve_target(
                &transition,
                &next,
                &arguments,
                &request.principal,
                &correlation_id,
            )
            .await?;

        // SPEC §26 — `while: <guard>` loop. When the FROM state declares
        // a while-guard AND that guard evaluates truthy against the
        // post-transition context, re-route target to from_state so the
        // workflow re-enters the same state. Tracks iteration count in
        // the synthetic `_while_iter.<state>` context slot; resets when
        // we actually leave. `max_iterations` cap is REQUIRED on while:
        // and enforced here — exceeding it fails with WHILE_ITERATION_CAP_EXCEEDED.
        if let Some(rerouted) = self
            .apply_while_loop(
                &definition,
                &from_state,
                &target,
                &mut next,
                &arguments,
                &request.principal,
                &correlation_id,
            )
            .await?
        {
            target = rerouted;
        }

        // SPEC §27 — clear state-local slots when actually leaving the
        // state. No-op for self-loops / while: re-entry (target == from).
        self.clear_state_local_slots_on_exit(
            &definition,
            &from_state,
            &target,
            &mut next,
            &correlation_id,
            &request.principal,
        )
        .await;

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
                next.audit_event("workflow.transitioned")
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
}
