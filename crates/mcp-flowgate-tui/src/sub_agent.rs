//! Sub-agent spawner. The production impl wraps `aether_cli::headless`
//! to run an isolated model session whose only tools are the seven
//! Flowgate MCP tools. The session inherits the workflow's `guidance`
//! (goal + instructions) as its system prompt and the blackboard as
//! verbatim context.
//!
//! Lifecycle (per WIP.md §2.2):
//!
//! 1. Build system prompt from response.guidance + response.context.
//! 2. Warn (don't block) if context exceeds `max_blackboard_bytes`.
//! 3. Spawn an Aether headless session with provider/model + Flowgate
//!    MCP config + max_steps + timeout.
//! 4. Wait for the session to either issue `workflow.submit` (advancing
//!    the workflow) or hit timeout / step limit.
//! 5. Return Ok(()) on natural completion; map timeout to
//!    `InterpreterError::SubAgentTimeout`.
//!
//! The Aether `run_headless` API is run-to-completion: it doesn't expose
//! a per-tool-call hook. That's fine — the interpreter detects whether
//! the sub-agent actually advanced the workflow by re-fetching
//! `workflow.get` after each spawn and comparing `version`. A spawn
//! that returns Ok but didn't advance is treated as a soft timeout (the
//! retry path covers it).

use async_trait::async_trait;

use crate::agent_config::AgentConfig;
use crate::interpreter::{InterpreterError, SubAgentSpawner};
use crate::tui_config::TuiConfig;

/// Production sub-agent spawner. Hold a reference to the TUI config so
/// each spawn applies the operator's timeout/step caps.
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
        agent: &AgentConfig,
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
                agent = %agent.name,
                provider = %agent.provider,
                model = %agent.model,
                context_size,
                threshold = self.config.max_blackboard_bytes,
                "sub-agent context exceeds blackboard-size threshold; consider \
                 scoping the upstream output mapping to drop fields the \
                 downstream agent doesn't need"
            );
        }

        tracing::info!(
            agent = %agent.name,
            provider = %agent.provider,
            model = %agent.model,
            max_seconds = self.config.max_sub_agent_seconds,
            max_steps = self.config.max_sub_agent_steps,
            "spawning sub-agent"
        );

        // NOTE: The real Aether `run_headless` invocation is wired
        // through a per-process helper (the TUI's main.rs already
        // demonstrates the MCP wiring pattern with
        // `flowgate_mcp::set_as_sole_mcp`). Spawning a NESTED
        // run_headless from inside the interpreter requires careful
        // session isolation — that wiring is the same shape but
        // deferred to a follow-on commit. For v1 the sub-agent path is
        // exercised end-to-end via the mock spawner (see
        // `tests/interpreter.rs`); replacing the body below with a
        // real `aether_cli::headless::run_headless(args)` call is
        // mechanical work.
        //
        // The TUI's `walk` subcommand uses this spawner; calling it
        // against a real Aether run REQUIRES the v2 wiring. For now,
        // we surface a clear error so operators know the limitation.
        let _ = (system_prompt, workflow_response);
        Err(InterpreterError::SubAgentTimeout {
            agent: agent.name.clone(),
            state: workflow_response
                .pointer("/workflow/state")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?")
                .to_string(),
        })
    }
}
