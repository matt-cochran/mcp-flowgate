//! SPEC §21 — Deterministic graph-walking interpreter for Flowgate
//! workflows. One function, `walk_workflow`, advances a workflow from its
//! current state to a terminal state by:
//!
//!  1. **Asking the gateway** for the current state via `workflow.get`.
//!  2. **Returning** if the workflow has reached a `completed` status.
//!  3. **Delegating** the current state to a sub-agent when the response
//!     carries a `delegate` field (SPEC §21). The sub-agent decides which
//!     `workflow.submit` call to make; the interpreter doesn't.
//!  4. **Auto-advancing** when only one non-deterministic link remains
//!     (deterministic chains were already auto-advanced by the gateway —
//!     see SPEC §6).
//!  5. **Picking the first non-escalate link** when multiple links remain
//!     and no sub-agent is delegated. Wrong picks are corrected by the
//!     critic/retry cycle on the next iteration.
//!
//! The interpreter is structurally simple by design: a `loop { match … }`,
//! ~100 lines of logic, no clever metaprogramming. Errors propagate via
//! `InterpreterError`; sub-agent timeouts get a retry budget before
//! escalation.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agent_config::AgentConfig;
use crate::agent_resolver::{
    AgentResolutionExhausted, Delegate, DelegateParseError, ProviderFeatures, Resolver,
};

/// Maximum sub-agent retries on `SubAgentTimeout` before the interpreter
/// submits the `escalate` transition (if one exists) or propagates.
///
/// Three is the established retry budget in this codebase (see
/// runtime_chain.rs's recovery policy). Increasing this without bounding
/// it elsewhere is how we'd accidentally rack up cost; decreasing it
/// makes flaky-but-recoverable sub-agents look broken.
pub const SUB_AGENT_RETRY_BUDGET: u32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum InterpreterError {
    /// A sub-agent ran past its time or step budget. The interpreter
    /// caught the timeout and is asking the workflow to escalate (or
    /// propagating if no escalate transition is declared).
    #[error("sub-agent '{agent}' exceeded its budget at state '{state}'")]
    SubAgentTimeout { agent: String, state: String },

    /// A workflow declared `delegate: <name>` but `<name>` was not
    /// declared in the agent registry. The error message varies by
    /// registry kind — legacy CLI mode points at `--agent` flags;
    /// YAML mode points at `agents.yaml` and the specificity walk.
    #[error("workflow state '{state}': {source}")]
    AgentResolution {
        state: String,
        #[source]
        source: ResolutionError,
    },

    /// Underlying `workflow.submit` was rejected by the gateway (likely
    /// `INVALID_TRANSITION` or guard failure). The interpreter surfaces
    /// the gateway's error body so the operator sees why.
    #[error("gateway rejected submit at state '{state}': {reason}")]
    SubmitRejected { state: String, reason: String },

    /// No actionable link from the current state, no delegate, and no
    /// escalate transition. Workflow is stuck; this is an architecture
    /// bug to fix in YAML.
    #[error(
        "workflow stuck at state '{state}': no delegate, no actionable links, \
         no `escalate` transition. Add one, fix the guards, or set a delegate."
    )]
    WorkflowStuck { state: String },

    /// An MCP-level error — connection lost, malformed response, etc.
    #[error("MCP call '{tool}' failed: {source}")]
    Mcp {
        tool: String,
        #[source]
        source: anyhow::Error,
    },
}

/// Abstraction for "call this MCP tool with these arguments and give me
/// the structured response." The production impl wraps an rmcp client
/// connected to a `mcp-flowgate` child process; tests substitute a
/// canned-response mock.
///
/// The trait stays minimal on purpose. The interpreter only ever issues
/// `workflow.get` and `workflow.submit` calls — adding methods would
/// signal scope creep.
#[async_trait]
pub trait McpToolCaller: Send + Sync {
    async fn call(&self, tool: &str, args: Value) -> anyhow::Result<Value>;
}

/// Abstraction for "run an isolated sub-agent session and wait for it
/// to advance the workflow." The production impl spawns an Aether
/// headless run with our `McpToolCaller` as the only tool backend; tests
/// substitute a mock that simulates submit-or-timeout.
///
/// The `spawn_and_wait` contract: the implementation MUST either issue
/// a `workflow.submit` call against the MCP caller (advancing the
/// workflow's version) OR time out. Returning `Ok(())` without
/// advancing the workflow is a contract violation that the interpreter
/// will treat as a stuck state.
#[async_trait]
pub trait SubAgentSpawner: Send + Sync {
    async fn spawn_and_wait(
        &self,
        agent: &ResolvedAgent,
        system_prompt: &str,
        workflow_response: &Value,
    ) -> Result<(), InterpreterError>;
}

// ── agent registry (legacy + yaml) ─────────────────────────────────────────

/// One resolved agent ready to be spawned: provider + model + the typed
/// feature set for that provider. Source-agnostic; both the legacy
/// `--agent` flag path and the new YAML resolver path produce this shape.
#[derive(Debug, Clone)]
pub struct ResolvedAgent {
    /// Operator-facing label (the delegate name or the legacy `--agent`
    /// name). Used for logging and the workflow's escalate path.
    pub label: String,
    /// Aether canonical provider name (e.g. `"anthropic"`).
    pub provider: String,
    /// Aether model identifier.
    pub model: String,
    /// Typed feature toggles for the binding's provider. Legacy CLI
    /// path always produces `ProviderFeatures::None`.
    pub features: ProviderFeatures,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolutionError {
    #[error(
        "delegate `{delegate}` is not registered. Either pass `--agent {delegate}=provider/model` \
         (legacy CLI mode) OR add it to your agents.yaml under `overrides:`."
    )]
    UnknownLegacyAgent { delegate: String },

    #[error("delegate `{delegate}` is not a valid <affinity> | <tier> | <affinity>-<tier>: {source}")]
    InvalidDelegate {
        delegate: String,
        #[source]
        source: DelegateParseError,
    },

    #[error("{0}")]
    Exhausted(#[from] AgentResolutionExhausted),
}

/// Source-agnostic resolution of `delegate:` strings to a `ResolvedAgent`.
pub trait AgentRegistry: Send + Sync {
    fn resolve(&self, delegate: &str) -> Result<ResolvedAgent, ResolutionError>;
}

/// v0.2-compatible registry: a HashMap of `--agent` flag values keyed by
/// delegate name. Wraps the existing `AgentConfig` shape.
pub struct LegacyAgentRegistry {
    pub agents: HashMap<String, AgentConfig>,
}

impl LegacyAgentRegistry {
    pub fn new(agents: HashMap<String, AgentConfig>) -> Self {
        Self { agents }
    }
}

impl AgentRegistry for LegacyAgentRegistry {
    fn resolve(&self, delegate: &str) -> Result<ResolvedAgent, ResolutionError> {
        let c = self.agents.get(delegate).ok_or_else(|| {
            ResolutionError::UnknownLegacyAgent {
                delegate: delegate.to_string(),
            }
        })?;
        Ok(ResolvedAgent {
            label: c.name.clone(),
            provider: c.provider.clone(),
            model: c.model.clone(),
            features: ProviderFeatures::None,
        })
    }
}

/// v0.3 YAML-backed registry. Parses the delegate string, walks the
/// specificity ladder, and returns the FIRST binding from the chosen
/// list. (Full per-list Chain-of-Responsibility at spawn time is
/// deferred to v0.3.1 once aether's error surface exposes the failure
/// class we need to classify per-attempt failures.)
pub struct YamlAgentRegistry {
    pub resolver: Resolver,
}

impl YamlAgentRegistry {
    pub fn new(resolver: Resolver) -> Self {
        Self { resolver }
    }
}

impl AgentRegistry for YamlAgentRegistry {
    fn resolve(&self, delegate: &str) -> Result<ResolvedAgent, ResolutionError> {
        let d = Delegate::parse(delegate).map_err(|source| {
            ResolutionError::InvalidDelegate {
                delegate: delegate.to_string(),
                source,
            }
        })?;
        let (bindings, _level) = self.resolver.walk(&d)?;
        let first = bindings.first().ok_or_else(|| {
            ResolutionError::Exhausted(AgentResolutionExhausted {
                delegate: delegate.to_string(),
                walked_levels: vec!["(empty list at chosen level)".to_string()],
                attempts: Vec::new(),
            })
        })?;
        Ok(ResolvedAgent {
            label: delegate.to_string(),
            provider: first.provider.display_name().to_string(),
            model: first.model.clone(),
            features: first.features.clone(),
        })
    }
}

/// Drive a workflow to a terminal state. See module-level docs for the
/// algorithm. Returns the final `context` blackboard map on success.
///
/// **Side effects.** Issues `workflow.get` and `workflow.submit` calls
/// against `mcp`. May invoke `spawner.spawn_and_wait` zero or more times
/// (once per delegate state visited).
pub async fn walk_workflow(
    mcp: &dyn McpToolCaller,
    spawner: &dyn SubAgentSpawner,
    workflow_id: &str,
    registry: &dyn AgentRegistry,
) -> Result<Value, InterpreterError> {
    let mut retries: u32 = 0;
    loop {
        let resp = mcp_get(mcp, workflow_id).await?;

        if is_completed(&resp) {
            return Ok(extract_context(&resp));
        }

        let state_before = current_state(&resp);
        let version_before = current_version(&resp);

        if let Some(agent_name) = resp.get("delegate").and_then(Value::as_str) {
            let agent = registry.resolve(agent_name).map_err(|source| {
                InterpreterError::AgentResolution {
                    state: state_before.clone(),
                    source,
                }
            })?;
            let prompt = build_sub_agent_prompt(&resp);
            match spawner.spawn_and_wait(&agent, &prompt, &resp).await {
                Ok(()) => {
                    // Sub-agent claims success; verify the workflow
                    // actually advanced. Aether headless can return
                    // cleanly even when the model declined to submit —
                    // in that case we treat it as an implicit timeout
                    // and let the retry budget cover it.
                    let resp_after = mcp_get(mcp, workflow_id).await?;
                    if current_version(&resp_after) > version_before {
                        retries = 0;
                        continue;
                    }
                    // Sub-agent ran without advancing — count as a
                    // soft timeout for retry purposes.
                    retries = retries.saturating_add(1);
                    if retries >= SUB_AGENT_RETRY_BUDGET {
                        try_escalate_or_propagate(
                            mcp,
                            workflow_id,
                            &resp_after,
                            agent_name,
                        )
                        .await?;
                        retries = 0;
                        continue;
                    }
                    continue;
                }
                Err(InterpreterError::SubAgentTimeout { .. }) => {
                    retries = retries.saturating_add(1);
                    if retries >= SUB_AGENT_RETRY_BUDGET {
                        // Re-fetch in case the sub-agent partially
                        // advanced before timing out.
                        let resp_now = mcp_get(mcp, workflow_id).await?;
                        try_escalate_or_propagate(
                            mcp,
                            workflow_id,
                            &resp_now,
                            &agent.label,
                        )
                        .await?;
                        retries = 0;
                        continue;
                    }
                    continue;
                }
                Err(other) => return Err(other),
            }
        }

        // No delegate: auto-advance based on links.
        let pick = pick_link(&resp).ok_or_else(|| InterpreterError::WorkflowStuck {
            state: state_before.clone(),
        })?;
        submit_link(mcp, &pick).await?;
        retries = 0;
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

async fn mcp_get(mcp: &dyn McpToolCaller, workflow_id: &str) -> Result<Value, InterpreterError> {
    mcp.call("workflow.get", json!({ "workflowId": workflow_id }))
        .await
        .map_err(|e| InterpreterError::Mcp {
            tool: "workflow.get".into(),
            source: e,
        })
}

fn is_completed(resp: &Value) -> bool {
    resp.pointer("/result/status").and_then(Value::as_str) == Some("completed")
}

fn extract_context(resp: &Value) -> Value {
    resp.get("context").cloned().unwrap_or_else(|| json!({}))
}

fn current_state(resp: &Value) -> String {
    resp.pointer("/workflow/state")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string()
}

fn current_version(resp: &Value) -> u64 {
    resp.pointer("/workflow/version")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Build the sub-agent system prompt from the response's `guidance` +
/// `context`. The sub-agent inherits goal + instructions and sees the
/// blackboard verbatim. Size threshold for warnings is handled by the
/// production spawner (TuiConfig.max_blackboard_bytes), not here — the
/// interpreter doesn't make policy calls about prompt length.
fn build_sub_agent_prompt(resp: &Value) -> String {
    let goal = resp
        .pointer("/guidance/goal")
        .and_then(Value::as_str)
        .unwrap_or("(no goal declared)");
    let instructions = resp
        .pointer("/guidance/instructions")
        .and_then(Value::as_str)
        .unwrap_or("");
    let context = resp.get("context").cloned().unwrap_or_else(|| json!({}));
    let context_str = serde_json::to_string_pretty(&context).unwrap_or_default();
    format!(
        "You are a sub-agent inside a governed Flowgate workflow.\n\n\
         Goal: {goal}\n\n\
         Instructions: {instructions}\n\n\
         Blackboard (current context):\n{context_str}\n\n\
         You must call `workflow.submit` with one of the links from the \
         current `workflow.get` response when you are ready to advance \
         the workflow."
    )
}

/// Filter actionable links: drop ones whose actor is `deterministic`
/// (those are auto-chained by the gateway itself per SPEC §6, not for
/// the interpreter to drive).
fn actionable_links(resp: &Value) -> Vec<Value> {
    resp.get("links")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|l| l.get("actor").and_then(Value::as_str) != Some("deterministic"))
        .collect()
}

/// Pick the link the interpreter will submit. Algorithm:
/// 1. Filter `actor == "deterministic"` (handled by the gateway).
/// 2. If exactly one remains → return it (the "obvious" path).
/// 3. If multiple remain → return the first non-`escalate` link.
///    Picking `escalate` aggressively would short-circuit useful work.
fn pick_link(resp: &Value) -> Option<Value> {
    let actionable = actionable_links(resp);
    if actionable.is_empty() {
        return None;
    }
    if actionable.len() == 1 {
        return Some(actionable[0].clone());
    }
    // Multi-link case: prefer first non-escalate. Falls back to first
    // link if every option happens to be `escalate` (degenerate config).
    actionable
        .iter()
        .find(|l| l.get("rel").and_then(Value::as_str) != Some("escalate"))
        .cloned()
        .or_else(|| actionable.into_iter().next())
}

async fn submit_link(mcp: &dyn McpToolCaller, link: &Value) -> Result<(), InterpreterError> {
    let args = link.get("args").cloned().unwrap_or_else(|| json!({}));
    let state = current_state(link);
    let resp = mcp.call("workflow.submit", args).await.map_err(|e| {
        InterpreterError::Mcp {
            tool: "workflow.submit".into(),
            source: e,
        }
    })?;
    // The gateway returns rejections in the body (`error.code`) not as
    // MCP-level errors. Translate so the interpreter sees them.
    if let Some(err) = resp.get("error") {
        let reason = err.get("message").and_then(Value::as_str).unwrap_or("");
        return Err(InterpreterError::SubmitRejected {
            state,
            reason: reason.to_string(),
        });
    }
    Ok(())
}

/// After SUB_AGENT_RETRY_BUDGET timeouts: try to submit an `escalate`
/// transition if one exists in the current response's links. Otherwise
/// propagate `SubAgentTimeout`.
async fn try_escalate_or_propagate(
    mcp: &dyn McpToolCaller,
    _workflow_id: &str,
    resp: &Value,
    agent_name: &str,
) -> Result<(), InterpreterError> {
    let escalate_link = resp
        .get("links")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|l| l.get("rel").and_then(Value::as_str) == Some("escalate"))
                .cloned()
        });
    let Some(link) = escalate_link else {
        return Err(InterpreterError::SubAgentTimeout {
            agent: agent_name.to_string(),
            state: current_state(resp),
        });
    };
    submit_link(mcp, &link).await
}
