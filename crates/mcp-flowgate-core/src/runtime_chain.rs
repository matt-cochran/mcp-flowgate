//! Chain, timeout, and on-enter helpers for [`WorkflowRuntime`].
//! Methods stay on the same `impl WorkflowRuntime` block split across
//! sibling files — see `runtime.rs` for the type definition and lifecycle
//! entry points (`start`, `submit`, `get`).

use anyhow::{anyhow, bail};
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::mapping::merge_output;
use crate::model::{Evidence, Principal, WorkflowInstance};
use crate::reliability::{execute_with_reliability, ReliabilityPolicy};
use crate::runtime::{
    ChainOutcome, ChainResult, ChainStep, TransitionRecordParams, WorkflowRuntime,
};
use crate::runtime_links::{is_terminal, pointer_escape};
use crate::runtime_records::{blackboard_delta, validate_blackboard_writes};
use crate::runtime_schema::required_str;

impl WorkflowRuntime {
    /// Resolve the next state for a transition. The transition's declared
    /// `target` is the default; if `branches: [{when, target}]` is present,
    /// the first branch whose `when` guard passes wins. Falls back to the
    /// declared `target` when no branch matches.
    ///
    /// Emits a `transition.branched` audit event when a branch fires so
    /// it's clear in logs which branch the runtime took.
    pub(crate) async fn resolve_target(
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
                self.record_or_self_event(
                        instance.audit_event("transition.branched")
                            .with_correlation(correlation_id)
                            .with_actor(&principal.subject)
                            .with_payload(json!({
                                "branchIndex": idx,
                                "fromState": instance.state,
                                "toState": branch_target,
                            })),
                ).await;
                return Ok(branch_target.to_string());
            }
        }
        Ok(default_target)
    }

    /// SPEC §26 — apply a `while: <guard>` loop on the FROM state.
    ///
    /// Called by `submit` after the executor's output has been merged
    /// into `next.context` and the next-state `target` has been
    /// resolved. If the FROM state declares `while:`, this:
    /// 1. Evaluates the while-guard against the post-output context.
    /// 2. If truthy, increments the iteration counter in synthetic
    ///    context slot `_while_iter.<state>` and returns the
    ///    REROUTED target (= from_state) so the workflow re-enters.
    /// 3. If iteration > `max_iterations`, fails fast with
    ///    `WHILE_ITERATION_CAP_EXCEEDED`.
    /// 4. If the guard is falsy and we're actually leaving the state,
    ///    clears the synthetic iteration counter.
    ///
    /// `max_iterations` is REQUIRED when `while:` is declared (config
    /// validation should enforce this; this runtime check is the
    /// defense-in-depth backstop).
    ///
    /// Returns `Ok(Some(rerouted_target))` when re-entry fires;
    /// `Ok(None)` when the workflow proceeds normally.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn apply_while_loop(
        &self,
        definition: &Value,
        from_state: &str,
        declared_target: &str,
        next: &mut WorkflowInstance,
        arguments: &Value,
        principal: &Principal,
        correlation_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let Some(state_def) = definition
            .pointer(&format!("/states/{}", pointer_escape(from_state)))
        else {
            return Ok(None);
        };
        let Some(while_guard) = state_def.get("while") else {
            // Nothing to do — no while-guard on this state.
            // But if we're leaving from a state that previously had a
            // while-iter counter, scrub it. Cheap: just remove the slot
            // if present.
            clear_while_iter(next, from_state);
            return Ok(None);
        };
        let max_iter = state_def
            .get("max_iterations")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                anyhow!(
                    "INVALID_STATE_CONFIG: state '{from_state}' declares `while:` \
                     but no `max_iterations:` cap. The cap is REQUIRED to prevent \
                     runaway loops; declare an explicit ceiling (SPEC §26)."
                )
            })? as u32;

        let truthy = self
            .guards
            .evaluate(while_guard, next, arguments, principal)
            .await?;

        if !truthy {
            // Guard went falsy — we're actually leaving. Clean up.
            clear_while_iter(next, from_state);
            return Ok(None);
        }

        // Guard is truthy: re-enter. Bump iteration counter.
        let current = read_while_iter(next, from_state);
        let next_iter = current.saturating_add(1);
        if next_iter > max_iter {
            bail!(
                "WHILE_ITERATION_CAP_EXCEEDED: state '{from_state}' has `while:` \
                 guard that remained truthy after {max_iter} iterations. Either \
                 the guard's exit condition is unreachable, or `max_iterations:` \
                 needs to be increased after operator review (SPEC §26)."
            );
        }
        write_while_iter(next, from_state, next_iter);

        let _ = declared_target; // we deliberately ignore the declared target on re-enter.
        self.record_or_self_event(
            next.audit_event("workflow.state.iteration")
                .with_correlation(correlation_id)
                .with_actor(&principal.subject)
                .with_payload(json!({
                    "state":         from_state,
                    "iteration":     next_iter,
                    "max_iterations": max_iter,
                })),
        )
        .await;

        Ok(Some(from_state.to_string()))
    }

    /// Lazy workflow-level timeout check. If `definition.timeoutMs` is
    /// declared and the wall-clock interval since `instance.started_at`
    /// exceeds it, advance the workflow to `definition.onTimeout.target`
    /// and emit a `workflow.timed_out` audit event. Returns `Some(updated)`
    /// when a timeout fired (caller should respond from that snapshot),
    /// `None` otherwise.
    pub(crate) async fn check_and_apply_timeout(
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
                self.record_or_self_event(
                        instance.audit_event("workflow.timed_out")
                            .with_actor(&principal.subject)
                            .with_payload(json!({
                                "elapsedMs": elapsed,
                                "timeoutMs": timeout_ms,
                                "fromState": instance.state,
                                "applied": false,
                            })),
                ).await;
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

        self.record_or_self_event(
                saved.audit_event("workflow.timed_out")
                    .with_correlation(&correlation_id)
                    .with_actor(&principal.subject)
                    .with_payload(json!({
                        "elapsedMs": elapsed,
                        "timeoutMs": timeout_ms,
                        "fromState": from_state,
                        "toState": target,
                        "applied": true,
                    })),
        ).await;
        Ok(Some(saved))
    }

    pub(crate) async fn run_on_enter(
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
    pub(crate) async fn run_deterministic_chain(
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

            // Stop: if any STATE-CHANGING non-deterministic transition is
            // present, this is a decision point for the LLM/human.
            //
            // SPEC §29 lightweight transitions (e.g. auto-injected
            // `ask_human` self-loops from `enable_human_ask`) are
            // interactions, NOT state changes — they don't advance the
            // workflow and shouldn't bail the chain. Filtering them out
            // of the decision-point check preserves deterministic chaining
            // through states that opt into HITL ask.
            let non_det_state_changers = transitions
                .iter()
                .filter(|(_, t)| t.get("actor").and_then(Value::as_str) != Some("deterministic"))
                .filter(|(_, t)| {
                    !t.get("lightweight")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
                .count();
            if non_det_state_changers > 0 {
                break;
            }

            // Stop: no deterministic transitions to fire
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
                    self.record_or_self_event(
                            instance.audit_event("chain.failed")
                                .with_correlation(correlation_id)
                                .with_payload(json!({
                                    "fromState": instance.state,
                                    "chainDepth": steps.len(),
                                    "errorClass": "selection_error",
                                    "message": e.to_string(),
                                })),
                    ).await;
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
            self.record_or_self_event(
                    instance.audit_event("chain.step")
                        .with_correlation(correlation_id)
                        .with_payload(json!({
                            "transition": transition_name,
                            "fromState": from_state,
                            "chainDepth": steps.len(),
                        })),
            ).await;

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
                            self.record_or_self_event(
                                    instance.audit_event("chain.failed")
                                        .with_correlation(correlation_id)
                                        .with_payload(json!({
                                            "transition": transition_name,
                                            "fromState": from_state,
                                            "chainDepth": steps.len(),
                                            "code": "BLACKBOARD_TYPE_ERROR",
                                            "message": message,
                                        })),
                            ).await;
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
                        self.record_or_self_event(
                                instance.audit_event("chain.failed")
                                    .with_correlation(correlation_id)
                                    .with_payload(json!({
                                        "transition": transition_name,
                                        "fromState": from_state,
                                        "chainDepth": steps.len(),
                                        "errorClass": err.class().token(),
                                        "message": err.to_string(),
                                    })),
                        ).await;
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
            self.record_or_self_event(
                    instance.audit_event("workflow.transitioned")
                        .with_correlation(correlation_id)
                        .with_actor(&principal.subject)
                        .with_payload(json!({
                            "transition": transition_name,
                            "state": instance.state,
                            "version": instance.version,
                            "deterministic": true,
                            "chainDepth": steps.len(),
                        })),
            ).await;

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
            self.record_or_self_event(
                    instance.audit_event("chain.completed")
                        .with_correlation(correlation_id)
                        .with_payload(json!({
                            "steps": steps.len(),
                            "finalState": instance.state,
                        })),
            ).await;
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
                self.record_or_self_event(
                        instance.audit_event("guard.evaluated")
                            .with_correlation(correlation_id)
                            .with_payload(json!({
                                "guard": guard,
                                "passed": pass,
                                "context": "deterministic_selection",
                            })),
                ).await;
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
}

// ── SPEC §27 helpers — state-local blackboard slot lifecycle ──────────────

impl WorkflowRuntime {
    /// SPEC §27 — when a transition leaves a state, clear every slot
    /// declared `scope: state` on that state. State-local slots are
    /// initialized on enter, persist across `while:`-re-entry of the
    /// same state, and are cleared on exit (including chain-hop exits).
    ///
    /// Called by `submit` AFTER the final target is determined and
    /// AFTER `apply_while_loop` has had a chance to re-route back to
    /// the same state. When `from_state == target`, this is a no-op
    /// (re-entry preserves state-local values).
    ///
    /// Emits a `workflow.slot.cleared` audit event with the list of
    /// cleared slot names so operators can correlate state exits
    /// with their cumulative blackboard footprint.
    pub(crate) async fn clear_state_local_slots_on_exit(
        &self,
        definition: &Value,
        from_state: &str,
        target: &str,
        next: &mut WorkflowInstance,
        correlation_id: &str,
        principal: &Principal,
    ) {
        if from_state == target {
            // While-re-entry or self-loop — keep state-local slots and
            // keep per-state fire counters (the counter's whole purpose
            // is to bound self-loops).
            return;
        }
        // SPEC §29 — scrub synthetic per-transition fire counters for
        // this state. They're keyed `_fire_count.<state>.<transition>`
        // and only mean anything inside one state-entry; clear when we
        // leave. Generic — applies to any transition that declared
        // `max_fires_per_visit`, not just HITL.
        let fire_prefix = format!("_fire_count.{from_state}.");
        if let Some(ctx) = next.context.as_object_mut() {
            ctx.retain(|k, _| !k.starts_with(&fire_prefix));
        }
        let Some(state_def) = definition
            .pointer(&format!("/states/{}", pointer_escape(from_state)))
        else {
            return;
        };
        let Some(slots) = state_def.get("slots").and_then(Value::as_object) else {
            return;
        };
        let mut cleared: Vec<String> = Vec::new();
        if let Some(ctx) = next.context.as_object_mut() {
            for (name, decl) in slots {
                let scope = decl
                    .get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or("workflow");
                if scope == "state" && ctx.remove(name).is_some() {
                    cleared.push(name.clone());
                }
            }
        }
        if cleared.is_empty() {
            return;
        }
        self.record_or_self_event(
            next.audit_event("workflow.slot.cleared")
                .with_correlation(correlation_id)
                .with_actor(&principal.subject)
                .with_payload(json!({
                    "state": from_state,
                    "slots": cleared,
                })),
        )
        .await;
    }
}

// ── SPEC §26 helpers — while-loop iteration counter ────────────────────────

fn while_iter_key(state: &str) -> String {
    format!("_while_iter.{state}")
}

fn read_while_iter(instance: &WorkflowInstance, state: &str) -> u32 {
    let key = while_iter_key(state);
    instance
        .context
        .get(&key)
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32
}

fn write_while_iter(instance: &mut WorkflowInstance, state: &str, value: u32) {
    let key = while_iter_key(state);
    if let Some(ctx) = instance.context.as_object_mut() {
        ctx.insert(key, json!(value));
    }
}

fn clear_while_iter(instance: &mut WorkflowInstance, state: &str) {
    let key = while_iter_key(state);
    if let Some(ctx) = instance.context.as_object_mut() {
        ctx.remove(&key);
    }
}
