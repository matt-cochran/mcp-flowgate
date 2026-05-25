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
