//! A `workflow` executor that starts a sub-workflow and waits for it to
//! complete. Two invocation shapes:
//!
//! **Legacy (input-only):**
//!
//! ```yaml
//! executor:
//!   kind: workflow
//!   definitionId: with_artifact_lock
//!   input:
//!     artifact: "$.context.artifact_name"
//!     owner: "$.workflow.input.user"
//!   timeoutMs: 60000
//! ```
//!
//! In this shape the sub-workflow inherits the full host context as its
//! return value (back-compat for pre-v0.6 callers).
//!
//! **Capability (use: block, SPEC §6):**
//!
//! ```yaml
//! executor:
//!   kind: workflow
//!   definitionId: cap.plan.vet
//!   use:
//!     inputs:
//!       plan: "$.context.draft_plan"
//!     outputs:
//!       "$.context.vet_verdict": verdict
//!       "$.context.vet_findings": findings
//! ```
//!
//! In this shape the capability runs in a fresh blackboard populated from
//! `use.inputs`; on completion ONLY the outputs declared in `use.outputs`
//! propagate back to the host. Each projected value is validated against
//! the capability's `snippet.outputs` schema (embedded as `_snippetOutputs`
//! at config-resolve time). A validation failure aborts the transition
//! with `ExecutorError::SchemaViolation` and emits a
//! `cap.output.schema_violation` audit event — no partial outputs reach
//! the host blackboard (the cap-scoping firewall).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use mcp_flowgate_core::audit::{AuditEvent, AuditSink};
use mcp_flowgate_core::error::ExecutorError;
use mcp_flowgate_core::model::{
    ExecuteRequest, ExecuteResult, GetWorkflow, Principal, StartWorkflow,
};
use mcp_flowgate_core::ports::Executor;
use mcp_flowgate_core::runtime::WorkflowRuntime;
use mcp_flowgate_core::use_binding::{
    project_use_outputs, resolve_use_inputs, validate_outputs_against_snippet,
};

/// Maximum nesting depth for sub-workflows to prevent infinite recursion.
#[allow(dead_code)]
const MAX_WORKFLOW_DEPTH: u32 = 10;

pub struct WorkflowExecutor {
    runtime: WorkflowRuntime,
    audit: Arc<dyn AuditSink>,
}

impl WorkflowExecutor {
    pub fn new(runtime: WorkflowRuntime, audit: Arc<dyn AuditSink>) -> Self {
        Self { runtime, audit }
    }
}

#[async_trait]
impl Executor for WorkflowExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let definition_id = request
            .executor_config
            .get("definitionId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ExecutorError::Permanent("workflow executor requires 'definitionId'".to_string())
            })?
            .to_string();

        let timeout_ms = request
            .executor_config
            .get("timeoutMs")
            .and_then(Value::as_u64);

        let parent_corr = request
            .correlation_id
            .clone()
            .unwrap_or_else(|| "unset-corr".to_string());

        // Branch on whether this is a capability invocation (`use:` block)
        // or the legacy input-only shape. Capability invocations get the
        // scoping firewall + snippet-output validation; legacy callers
        // keep their pre-v0.6 behavior unchanged.
        let use_block = request.executor_config.get("use").cloned();
        let snippet_outputs = request
            .executor_config
            .get("_snippetOutputs")
            .cloned()
            .unwrap_or(Value::Null);

        let sub_input = match &use_block {
            Some(use_val) => {
                let use_inputs = use_val.get("inputs").cloned().unwrap_or(json!({}));
                Value::Object(resolve_use_inputs(
                    &use_inputs,
                    &request.arguments,
                    &request.workflow.context,
                    &request.workflow.input,
                ))
            }
            None => {
                let input = request
                    .executor_config
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                resolve_input(&input, &request.workflow.context, &request.arguments)
            }
        };

        // Emit cap.invoked for capability calls so audit reconstruction can
        // link parent ↔ child via parent_correlation_id (SPEC §6.3).
        if use_block.is_some() {
            self.audit
                .record(
                    AuditEvent::new("cap.invoked")
                        .with_workflow(request.workflow.id.clone())
                        .with_correlation(parent_corr.clone())
                        .with_payload(json!({
                            "definitionId": definition_id,
                            "parent_correlation_id": parent_corr,
                        })),
                )
                .await
                .ok();
        }

        let start_resp = self
            .runtime
            .start(StartWorkflow {
                definition_id: definition_id.clone(),
                input: sub_input,
                principal: Principal::anonymous(),
                trace_id: None,
                run_id: None,
            })
            .await
            .map_err(|e| {
                if use_block.is_some() {
                    let kind = "cap_start_failed";
                    let audit = self.audit.clone();
                    let wf_id = request.workflow.id.clone();
                    let def_id = definition_id.clone();
                    let corr = parent_corr.clone();
                    let err_msg = e.to_string();
                    tokio::spawn(async move {
                        audit
                            .record(
                                AuditEvent::new("cap.terminated")
                                    .with_workflow(wf_id)
                                    .with_correlation(corr.clone())
                                    .with_payload(json!({
                                        "definitionId":          def_id,
                                        "parent_correlation_id": corr,
                                        "error_kind":            kind,
                                        "error":                 err_msg,
                                    })),
                            )
                            .await
                            .ok();
                    });
                }
                ExecutorError::Permanent(format!("failed to start sub-workflow: {e}"))
            })?;

        let sub_workflow_id = start_resp
            .pointer("/workflow/id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ExecutorError::Permanent("sub-workflow response missing workflow.id".to_string())
            })?
            .to_string();

        // Legacy audit event (kept for back-compat with existing consumers).
        self.audit
            .record(
                AuditEvent::new("sub_workflow.started")
                    .with_workflow(request.workflow.id.clone())
                    .with_correlation(parent_corr.clone())
                    .with_payload(json!({
                        "sub_workflow_id": sub_workflow_id,
                        "definition_id":   definition_id,
                    })),
            )
            .await
            .ok();

        // Poll until terminal or timeout.
        let start_time = std::time::Instant::now();
        loop {
            if let Some(timeout) = timeout_ms {
                if start_time.elapsed().as_millis() as u64 > timeout {
                    self.audit
                        .record(
                            AuditEvent::new("sub_workflow.timed_out")
                                .with_workflow(request.workflow.id.clone())
                                .with_correlation(parent_corr.clone())
                                .with_payload(json!({
                                    "sub_workflow_id": sub_workflow_id,
                                    "timeout_ms":      timeout,
                                })),
                        )
                        .await
                        .ok();
                    if use_block.is_some() {
                        emit_cap_terminated(
                            &self.audit,
                            &request,
                            &definition_id,
                            &parent_corr,
                            "cap_timeout",
                            Some(json!({ "timeout_ms": timeout })),
                        )
                        .await;
                    }
                    return Err(ExecutorError::Timeout(timeout));
                }
            }

            let get_resp = self
                .runtime
                .get(GetWorkflow {
                    workflow_id: sub_workflow_id.clone(),
                    principal: Principal::anonymous(),
                    trace_id: None,
                    run_id: None,
                })
                .await
                .map_err(|e| {
                    ExecutorError::Permanent(format!("failed to get sub-workflow: {e}"))
                })?;

            let status = get_resp
                .pointer("/result/status")
                .and_then(Value::as_str)
                .unwrap_or("running");

            match status {
                "completed" => {
                    let child_context = get_resp
                        .pointer("/context")
                        .cloned()
                        .unwrap_or_else(|| json!({}));

                    self.audit
                        .record(
                            AuditEvent::new("sub_workflow.completed")
                                .with_workflow(request.workflow.id.clone())
                                .with_correlation(parent_corr.clone())
                                .with_payload(json!({
                                    "sub_workflow_id": sub_workflow_id,
                                })),
                        )
                        .await
                        .ok();

                    // Capability shape: project ONLY declared outputs;
                    // validate; return projected map keyed by cap output
                    // name so the synthesized transition `output:` mapping
                    // (built at config-resolve time) plucks via
                    // `$.output.<cap_output_name>`. Anything else in the
                    // child context dies with the capability instance.
                    if let Some(use_val) = use_block.as_ref() {
                        let use_outputs = use_val.get("outputs").cloned().unwrap_or(json!({}));
                        let projected_by_host =
                            project_use_outputs(&use_outputs, &child_context);
                        if let Err(violations) = validate_outputs_against_snippet(
                            &snippet_outputs,
                            &use_outputs,
                            &projected_by_host,
                        ) {
                            let diff: Vec<Value> = violations
                                .iter()
                                .map(|v| {
                                    json!({
                                        "slot":   v.slot,
                                        "reason": v.reason,
                                    })
                                })
                                .collect();
                            self.audit
                                .record(
                                    AuditEvent::new("cap.output.schema_violation")
                                        .with_workflow(request.workflow.id.clone())
                                        .with_correlation(parent_corr.clone())
                                        .with_payload(json!({
                                            "definitionId":          definition_id,
                                            "parent_correlation_id": parent_corr,
                                            "violations":            diff,
                                        })),
                                )
                                .await
                                .ok();
                            emit_cap_terminated(
                                &self.audit,
                                &request,
                                &definition_id,
                                &parent_corr,
                                "schema_violation",
                                Some(json!({ "violations": violations.len() })),
                            )
                            .await;
                            return Err(ExecutorError::SchemaViolation(format!(
                                "capability '{definition_id}' produced outputs failing snippet \
                                 contract: {}",
                                violations
                                    .iter()
                                    .map(|v| format!("{}: {}", v.slot, v.reason))
                                    .collect::<Vec<_>>()
                                    .join("; ")
                            )));
                        }
                        // Rekey by cap output name so the synthesized
                        // transition output's `$.output.<cap_output_name>`
                        // pointers resolve.
                        let by_cap_name =
                            rekey_by_cap_output_name(&use_outputs, &projected_by_host);
                        return Ok(ExecuteResult {
                            output: Value::Object(by_cap_name),
                            evidence: vec![],
                            child_workflow_id: Some(sub_workflow_id),
                        });
                    }

                    // Legacy shape — full child context returned, as today.
                    return Ok(ExecuteResult {
                        output: child_context,
                        evidence: vec![],
                        child_workflow_id: Some(sub_workflow_id),
                    });
                }
                "failed" | "timed_out" => {
                    self.audit
                        .record(
                            AuditEvent::new("sub_workflow.failed")
                                .with_workflow(request.workflow.id.clone())
                                .with_correlation(parent_corr.clone())
                                .with_payload(json!({
                                    "sub_workflow_id": sub_workflow_id,
                                    "status":          status,
                                })),
                        )
                        .await
                        .ok();
                    if use_block.is_some() {
                        emit_cap_terminated(
                            &self.audit,
                            &request,
                            &definition_id,
                            &parent_corr,
                            if status == "timed_out" {
                                "cap_timeout"
                            } else {
                                "cap_failed"
                            },
                            Some(json!({ "terminal_status": status })),
                        )
                        .await;
                    }

                    return Err(ExecutorError::Permanent(format!(
                        "sub-workflow reached terminal state '{status}'"
                    )));
                }
                _ => {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    }
}

/// Rebuild the projected map keyed by capability output name. The runtime's
/// merge_output projection plucks via `$.output.<cap_output_name>`, so we
/// must hand it that shape — not the host-path-keyed map that
/// `project_use_outputs` produces (that one is keyed by host path because
/// other callers/tests use it directly).
fn rekey_by_cap_output_name(
    use_outputs: &Value,
    projected_by_host_path: &Map<String, Value>,
) -> Map<String, Value> {
    let mut out = Map::new();
    let Some(bindings) = use_outputs.as_object() else {
        return out;
    };
    for (host_path, cap_name_value) in bindings {
        let Some(cap_name) = cap_name_value.as_str() else {
            continue;
        };
        if let Some(v) = projected_by_host_path.get(host_path) {
            out.insert(cap_name.to_string(), v.clone());
        }
    }
    out
}

/// Fire-and-forget audit emission for `cap.terminated`. Used by every
/// abnormal-termination path in the capability branch (cap_start_failed,
/// cap_timeout, cap_failed, schema_violation). The audit emission itself
/// never blocks the executor's error return.
async fn emit_cap_terminated(
    audit: &Arc<dyn AuditSink>,
    request: &ExecuteRequest,
    definition_id: &str,
    parent_corr: &str,
    error_kind: &str,
    extra_payload: Option<Value>,
) {
    let mut payload = json!({
        "definitionId":          definition_id,
        "parent_correlation_id": parent_corr,
        "error_kind":            error_kind,
    });
    if let (Some(extra), Some(obj)) = (extra_payload, payload.as_object_mut()) {
        if let Some(extra_obj) = extra.as_object() {
            for (k, v) in extra_obj {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    audit
        .record(
            AuditEvent::new("cap.terminated")
                .with_workflow(request.workflow.id.clone())
                .with_correlation(parent_corr.to_string())
                .with_payload(payload),
        )
        .await
        .ok();
}

fn resolve_input(input: &Value, context: &Value, arguments: &Value) -> Value {
    match input {
        Value::String(s) if s.starts_with("$.") => {
            mcp_flowgate_core::mapping::read_in_scopes(s, arguments, context, &json!({}), None)
                .unwrap_or(Value::Null)
        }
        Value::Object(map) => {
            let mut resolved = serde_json::Map::new();
            for (k, v) in map {
                resolved.insert(k.clone(), resolve_input(v, context, arguments));
            }
            Value::Object(resolved)
        }
        other => other.clone(),
    }
}
