//! Sub-agent spawner. Wraps `aether_cli::headless::run_headless` to run
//! an isolated model session whose only tools are the seven Flowgate MCP
//! tools. The session inherits the workflow's `guidance` (goal +
//! instructions) and the blackboard as the user prompt the sub-agent
//! acts on.
//!
//! Lifecycle (per SPEC §21):
//!
//! 1. Build the sub-agent prompt from `response.guidance` +
//!    `response.context` (already done by the interpreter; passed in as
//!    `system_prompt`).
//! 2. Warn (don't block) if context exceeds `max_blackboard_bytes`.
//! 3. Spawn an Aether headless session with `provider:model` + Flowgate
//!    MCP config (Flowgate is the sole MCP server the sub-agent can see).
//! 4. Wait for the session to either issue `workflow.submit` (advancing
//!    the workflow) or hit the operator-configured timeout.
//! 5. Return `Ok(())` on natural completion (the LLM emitted
//!    `AgentMessage::Done`). The interpreter then re-fetches `workflow.get`
//!    and compares `version` to confirm the sub-agent actually advanced
//!    the workflow.
//!
//! The Aether `run_headless` API is run-to-completion: it doesn't expose
//! a per-tool-call hook. That's fine — the interpreter detects whether
//! the sub-agent actually advanced the workflow by re-fetching
//! `workflow.get` after each spawn and comparing `version`. A spawn that
//! returns `Ok` but didn't advance is treated as a soft timeout (the
//! retry path covers it).
//!
//! ## v1 limitations
//!
//! - **Step limits are not enforced.** `aether_cli::headless::run_headless`
//!   has no built-in step counter; the LLM runs until it emits
//!   `AgentMessage::Done` OR the wall-clock timeout fires. The
//!   `max_sub_agent_steps` field on `TuiConfig` is currently surfaced
//!   only as a logged hint; enforcement would require intercepting
//!   `ToolCall` events (deferred to v2).
//! - **Sub-agent stdout goes to the parent process's stdout.** Aether's
//!   `CliOutputFormat::Text` streams the agent's reasoning text. For
//!   parallel sub-agent fan-out (future), output multiplexing will need
//!   work — for now, sub-agents run sequentially (one delegate state at
//!   a time) so stdout interleaving is not an issue.

use std::time::Duration;

use aether_cli::headless::{run_headless, CliOutputFormat, HeadlessArgs};
use aether_cli::mcp_config_args::McpConfigArgs;
use async_trait::async_trait;
use tokio::time::timeout;

use crate::agent_resolver::ProviderFeatures;
use crate::flowgate_mcp;
use crate::interpreter::{InterpreterError, ResolvedAgent, SubAgentSpawner};
use crate::tui_config::TuiConfig;

/// Production sub-agent spawner. Holds a reference to the TUI config so
/// each spawn applies the operator's timeout / blackboard caps.
pub struct AetherSubAgentSpawner {
    pub config: TuiConfig,
}

impl AetherSubAgentSpawner {
    pub fn new(config: TuiConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl SubAgentSpawner for AetherSubAgentSpawner {
    async fn spawn_and_wait(
        &self,
        agent: &ResolvedAgent,
        system_prompt: &str,
        workflow_response: &serde_json::Value,
    ) -> Result<(), InterpreterError> {
        // Pre-spawn: warn on oversized blackboard. Don't block; the
        // timeout catches genuine overload while small overshoot is
        // tolerable.
        let context_size = workflow_response
            .get("context")
            .map(|c| c.to_string().len())
            .unwrap_or(0);
        if context_size > self.config.max_blackboard_bytes {
            tracing::warn!(
                agent = %agent.label,
                provider = %agent.provider,
                model = %agent.model,
                context_size,
                threshold = self.config.max_blackboard_bytes,
                "sub-agent context exceeds blackboard-size threshold; consider \
                 scoping the upstream output mapping to drop fields the \
                 downstream agent doesn't need"
            );
        }

        // FMECA T3 + PR1 scope note: per-provider feature toggles are
        // parsed and stored on `ResolvedAgent.features` (load-time
        // validation via `#[serde(deny_unknown_fields)]`). Runtime
        // translation to aether's per-provider extras is deferred —
        // logging here is the operator-visible signal that the toggle
        // was recognized but not applied.
        if !matches!(agent.features, ProviderFeatures::None) {
            tracing::warn!(
                agent = %agent.label,
                provider = %agent.provider,
                model = %agent.model,
                features = ?agent.features,
                "agents.yaml feature toggles parsed but not yet applied at spawn time \
                 (deferred to v0.3.1 — see /guides/agent-config.mdx#features)"
            );
        }

        let workflow_state = workflow_response
            .pointer("/workflow/state")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?")
            .to_string();

        tracing::info!(
            agent = %agent.label,
            provider = %agent.provider,
            model = %agent.model,
            state = %workflow_state,
            max_seconds = self.config.max_sub_agent_seconds,
            max_steps = self.config.max_sub_agent_steps,
            "spawning sub-agent (max_steps is currently advisory only — \
             aether headless has no built-in step counter; the timeout is \
             the enforced cap)"
        );

        // Build the headless args. The interpreter-built system_prompt
        // becomes the USER PROMPT (the sub-agent's "go do this thing"
        // directive); we don't set Aether's system_prompt override — the
        // agent's own settings.json system prompt + the user prompt
        // together drive the session.
        let mut mcp_config = McpConfigArgs::default();
        flowgate_mcp::set_as_sole_mcp(&mut mcp_config).map_err(|e| {
            InterpreterError::Mcp {
                tool: "aether/sub_agent/mcp_wiring".into(),
                source: e,
            }
        })?;

        let args = HeadlessArgs {
            prompt: vec![system_prompt.to_string()],
            agent: None,
            // Aether canonical model string: "provider:model".
            model: Some(format!("{}:{}", agent.provider, agent.model)),
            cwd: std::path::PathBuf::from("."),
            settings_source: Default::default(),
            provider_connection: Default::default(),
            mcp_config,
            system_prompt: None,
            output: CliOutputFormat::Text,
            verbose: false,
            events: vec![],
        };

        let total_timeout = Duration::from_secs(self.config.max_sub_agent_seconds);
        match timeout(total_timeout, run_headless(args)).await {
            Ok(Ok(_exit_code)) => {
                // Natural completion — LLM emitted AgentMessage::Done.
                // The interpreter checks workflow.version post-return; if
                // version didn't advance, it treats the spawn as a soft
                // timeout and retries.
                tracing::info!(
                    agent = %agent.label,
                    state = %workflow_state,
                    "sub-agent completed naturally"
                );
                Ok(())
            }
            Ok(Err(e)) => {
                // Aether's CliError. Pass through as an Mcp-style error
                // since the interpreter has no SubAgent-specific variant
                // and this is operationally close — the sub-agent's
                // tool-call surface IS MCP through to Flowgate.
                tracing::warn!(
                    agent = %agent.label,
                    state = %workflow_state,
                    error = %e,
                    "sub-agent failed inside aether headless"
                );
                Err(InterpreterError::Mcp {
                    tool: format!("aether/sub_agent/{}", agent.label),
                    source: anyhow::anyhow!("{e}"),
                })
            }
            Err(_elapsed) => {
                tracing::warn!(
                    agent = %agent.label,
                    state = %workflow_state,
                    timeout_seconds = self.config.max_sub_agent_seconds,
                    "sub-agent exceeded timeout"
                );
                Err(InterpreterError::SubAgentTimeout {
                    agent: agent.label.clone(),
                    state: workflow_state,
                })
            }
        }
    }
}
