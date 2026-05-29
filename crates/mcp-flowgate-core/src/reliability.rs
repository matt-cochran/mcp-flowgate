use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::time::timeout;

use crate::audit::{AuditEvent, AuditSink};
use crate::error::{ErrorClass, ExecutorError};
use crate::model::{ExecuteRequest, ExecuteResult, WorkflowInstance};
use crate::ports::ExecutorRegistry;

/// Parsed reliability policy. The wire format is the JSON object on a
/// transition or action; `from_value` is forgiving — missing fields fall back
/// to library defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReliabilityPolicy {
    #[serde(rename = "timeoutMs", default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<FallbackPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    #[serde(rename = "maxAttempts", default = "one")]
    pub max_attempts: u32,
    #[serde(default)]
    pub backoff: Backoff,
    #[serde(rename = "initialDelayMs", default)]
    pub initial_delay_ms: u64,
    #[serde(
        rename = "maxDelayMs",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub max_delay_ms: Option<u64>,
    #[serde(rename = "retryOn", default = "default_retry_on")]
    pub retry_on: Vec<String>,
}

fn one() -> u32 {
    1
}
fn default_retry_on() -> Vec<String> {
    vec![
        "timeout".into(),
        "transient_error".into(),
        "rate_limited".into(),
    ]
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 1,
            backoff: Backoff::None,
            initial_delay_ms: 0,
            max_delay_ms: None,
            retry_on: default_retry_on(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Backoff {
    #[default]
    None,
    Fixed,
    Exponential,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FallbackPolicy {
    #[serde(default = "first_success")]
    pub strategy: String,
    pub executors: Vec<Value>,
}

fn first_success() -> String {
    "first_success".to_string()
}

impl ReliabilityPolicy {
    pub fn from_value(value: Option<&Value>) -> Self {
        match value {
            Some(v) => serde_json::from_value(v.clone()).unwrap_or_default(),
            None => Self::default(),
        }
    }

    pub fn retry(&self) -> RetryPolicy {
        self.retry.clone().unwrap_or_default()
    }

    pub fn timeout(&self) -> Option<Duration> {
        self.timeout_ms.map(Duration::from_millis)
    }
}

/// Run an executor under the given reliability policy, emitting audit events
/// for each attempt. Tries the primary executor first, then any fallback
/// executors in declaration order.
#[allow(clippy::too_many_arguments)]
pub async fn execute_with_reliability(
    executors: &dyn ExecutorRegistry,
    audit: &Arc<dyn AuditSink>,
    instance: &WorkflowInstance,
    transition: Option<&str>,
    arguments: &Value,
    primary: Value,
    policy: &ReliabilityPolicy,
    correlation_id: &str,
) -> Result<ExecuteResult, ExecutorError> {
    let idempotency_key = compute_idempotency_key(&primary, instance, transition, correlation_id);
    let mut candidates: Vec<Value> = vec![primary];
    if let Some(fb) = &policy.fallback {
        candidates.extend(fb.executors.clone());
    }

    let retry = policy.retry();
    let mut last: Option<ExecutorError> = None;

    for (candidate_idx, exec_cfg) in candidates.into_iter().enumerate() {
        let kind = exec_cfg
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        if candidate_idx > 0 {
            let _ = audit
                .record(
                    AuditEvent::new("fallback.selected")
                        .with_workflow(&instance.id)
                        .with_correlation(correlation_id)
                        .with_payload(json!({
                            "transition": transition,
                            "candidate": candidate_idx,
                            "kind": kind,
                            "previousError": last.as_ref().map(|e| e.to_string()),
                        })),
                )
                .await;
        }

        let executor = match executors.get(&kind) {
            Some(e) => e,
            None => {
                last = Some(ExecutorError::Permanent(format!(
                    "executor kind '{kind}' is not registered"
                )));
                continue;
            }
        };

        for attempt in 1..=retry.max_attempts.max(1) {
            let _ = audit
                .record(
                    AuditEvent::new("executor.started")
                        .with_workflow(&instance.id)
                        .with_correlation(correlation_id)
                        .with_payload(json!({
                            "transition": transition,
                            "candidate": candidate_idx,
                            "attempt": attempt,
                            "kind": kind,
                            "idempotencyKey": idempotency_key,
                        })),
                )
                .await;

            let request = ExecuteRequest {
                workflow: instance.clone(),
                transition: transition.map(str::to_string),
                arguments: arguments.clone(),
                executor_config: exec_cfg.clone(),
                idempotency_key: idempotency_key.clone(),
                // SPEC §24 — thread the parent correlation_id through so
                // fan-out executors (kind: parallel) can emit per-branch
                // audit events that link back to the parent transition.
                correlation_id: Some(correlation_id.to_string()),
            };

            let result = match policy.timeout() {
                Some(t) => match timeout(t, executor.execute(request)).await {
                    Ok(r) => r,
                    Err(_) => Err(ExecutorError::Timeout(t.as_millis() as u64)),
                },
                None => executor.execute(request).await,
            };

            match result {
                Ok(ok) => {
                    let _ = audit
                        .record(
                            AuditEvent::new("executor.succeeded")
                                .with_workflow(&instance.id)
                                .with_correlation(correlation_id)
                                .with_payload(json!({
                                    "transition": transition,
                                    "candidate": candidate_idx,
                                    "attempt": attempt,
                                    "kind": kind,
                                })),
                        )
                        .await;
                    return Ok(ok);
                }
                Err(err) => {
                    let class = err.class();
                    let token = class.token().to_string();
                    let message = err.to_string();

                    if attempt < retry.max_attempts && retryable(&retry, class) {
                        let _ = audit
                            .record(
                                AuditEvent::new("executor.retrying")
                                    .with_workflow(&instance.id)
                                    .with_correlation(correlation_id)
                                    .with_payload(json!({
                                        "transition": transition,
                                        "candidate": candidate_idx,
                                        "attempt": attempt,
                                        "kind": kind,
                                        "errorClass": token,
                                        "error": message,
                                    })),
                            )
                            .await;
                        let delay = backoff_delay(&retry, attempt);
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                        last = Some(err);
                        continue;
                    }

                    let _ = audit
                        .record(
                            AuditEvent::new("executor.failed")
                                .with_workflow(&instance.id)
                                .with_correlation(correlation_id)
                                .with_payload(json!({
                                    "transition": transition,
                                    "candidate": candidate_idx,
                                    "attempt": attempt,
                                    "kind": kind,
                                    "errorClass": token,
                                    "error": message,
                                })),
                        )
                        .await;
                    last = Some(err);
                    break;
                }
            }
        }
    }

    Err(last.unwrap_or_else(|| ExecutorError::Permanent("no executor candidates".into())))
}

/// Compute an idempotency key for this execute call from the executor's
/// `idempotencyKey` field:
///
/// - `idempotencyKey: true` — auto-key, `<workflowId>.<transition>.<correlationId>`
/// - `idempotencyKey: "<template>"` — substitute `{workflowId}`,
///   `{transition}`, `{correlationId}` tokens.
/// - missing / `false` — no key.
///
/// The key is shared across retries and across fallback executors so a
/// downstream service that dedupes on the key sees the same identifier
/// for the whole "this submit" call.
fn compute_idempotency_key(
    primary_executor: &Value,
    instance: &WorkflowInstance,
    transition: Option<&str>,
    correlation_id: &str,
) -> Option<String> {
    let spec = primary_executor.get("idempotencyKey")?;
    let workflow_id = &instance.id;
    let transition = transition.unwrap_or("on_enter");

    if let Some(true) = spec.as_bool() {
        return Some(format!("{workflow_id}.{transition}.{correlation_id}"));
    }
    if let Some(template) = spec.as_str() {
        let key = template
            .replace("{workflowId}", workflow_id)
            .replace("{transition}", transition)
            .replace("{correlationId}", correlation_id);
        return Some(key);
    }
    None
}

fn retryable(retry: &RetryPolicy, class: ErrorClass) -> bool {
    let token = class.token();
    retry.retry_on.iter().any(|c| c == token)
}

fn backoff_delay(retry: &RetryPolicy, attempt: u32) -> Duration {
    let base = retry.initial_delay_ms;
    let raw_ms = match retry.backoff {
        Backoff::None => 0,
        Backoff::Fixed => base,
        Backoff::Exponential => base.saturating_mul(1u64 << attempt.saturating_sub(1).min(20)),
    };
    let capped = match retry.max_delay_ms {
        Some(max) => raw_ms.min(max),
        None => raw_ms,
    };
    Duration::from_millis(capped)
}
