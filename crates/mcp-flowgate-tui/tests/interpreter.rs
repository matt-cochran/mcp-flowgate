//! SPEC §21 — FMECA-style atomic assertions for the deterministic
//! interpreter (`walk_workflow`). Uses a `ScriptedMcpCaller` and
//! `ScriptedSpawner` so tests run without spawning real
//! `mcp-flowgate` or real LLM processes.
//!
//! One behavior per test. The interpreter has small surface but high
//! consequence (it decides whether to escalate, retry, or auto-advance)
//! so each branch gets a dedicated assertion.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use mcp_flowgate_tui::interpreter::{
    walk_workflow, InterpreterError, McpToolCaller, SubAgentSpawner, SUB_AGENT_RETRY_BUDGET,
};
use serde_json::{json, Value};

// ── test doubles ───────────────────────────────────────────────────────────

/// Scripted MCP backend. Each `expect` call queues a (tool, response)
/// pair; calls are matched in order. Mismatch is a hard failure.
struct ScriptedMcpCaller {
    queue: Mutex<Vec<(String, Value)>>,
    /// Track every call for assertions.
    calls: Mutex<Vec<(String, Value)>>,
}

impl ScriptedMcpCaller {
    fn new() -> Self {
        Self {
            queue: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn expect(&self, tool: &str, response: Value) {
        self.queue
            .lock()
            .unwrap()
            .push((tool.to_string(), response));
    }

    fn call_count(&self, tool: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == tool)
            .count()
    }

    fn calls_to(&self, tool: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == tool)
            .map(|(_, args)| args.clone())
            .collect()
    }
}

#[async_trait]
impl McpToolCaller for ScriptedMcpCaller {
    async fn call(&self, tool: &str, args: Value) -> anyhow::Result<Value> {
        self.calls.lock().unwrap().push((tool.to_string(), args));
        let mut queue = self.queue.lock().unwrap();
        if queue.is_empty() {
            anyhow::bail!("ScriptedMcpCaller: unexpected call to '{tool}' (queue empty)");
        }
        let (expected_tool, response) = queue.remove(0);
        assert_eq!(
            expected_tool, tool,
            "ScriptedMcpCaller: queued call was for '{expected_tool}' but got '{tool}'"
        );
        Ok(response)
    }
}

/// Scripted sub-agent spawner. Each `expect_spawn` call queues one
/// outcome (Ok or Err); spawns consume the queue in order.
struct ScriptedSpawner {
    outcomes: Mutex<Vec<Result<(), InterpreterError>>>,
    spawns: Mutex<u32>,
}

impl ScriptedSpawner {
    fn new() -> Self {
        Self {
            outcomes: Mutex::new(Vec::new()),
            spawns: Mutex::new(0),
        }
    }

    fn expect_spawn(&self, outcome: Result<(), InterpreterError>) {
        self.outcomes.lock().unwrap().push(outcome);
    }

    fn spawn_count(&self) -> u32 {
        *self.spawns.lock().unwrap()
    }
}

#[async_trait]
impl SubAgentSpawner for ScriptedSpawner {
    async fn spawn_and_wait(
        &self,
        agent: &mcp_flowgate_tui::agent_config::AgentConfig,
        _system_prompt: &str,
        _workflow_response: &Value,
    ) -> Result<(), InterpreterError> {
        let _ = agent;
        *self.spawns.lock().unwrap() += 1;
        let mut outcomes = self.outcomes.lock().unwrap();
        if outcomes.is_empty() {
            // Test rigging error: a spawn was made without a queued outcome.
            // Surface a distinctive error so the failing test names itself.
            return Err(InterpreterError::SubAgentTimeout {
                agent: "SCRIPT_OUTCOME_QUEUE_EXHAUSTED".into(),
                state: "SCRIPT".into(),
            });
        }
        outcomes.remove(0)
    }
}

// ── fixtures ───────────────────────────────────────────────────────────────

fn agent_registry() -> HashMap<String, mcp_flowgate_tui::agent_config::AgentConfig> {
    let mut m = HashMap::new();
    m.insert(
        "planner".to_string(),
        mcp_flowgate_tui::agent_config::AgentConfig {
            name: "planner".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-4".into(),
        },
    );
    m
}

fn resp_completed() -> Value {
    json!({
        "workflow": { "id": "wf_x", "definitionId": "demo", "state": "done", "version": 5 },
        "result":   { "status": "completed" },
        "context":  { "summary": "all good" },
        "links":    []
    })
}

fn resp_at_state(state: &str, version: u64, links: Vec<Value>, delegate: Option<&str>) -> Value {
    let mut body = json!({
        "workflow": { "id": "wf_x", "definitionId": "demo", "state": state, "version": version },
        "result":   { "status": "waiting_for_action" },
        "context":  {},
        "links":    links,
    });
    if let Some(d) = delegate {
        body["delegate"] = Value::String(d.to_string());
    }
    body
}

fn link(rel: &str, args: Value) -> Value {
    json!({ "rel": rel, "method": "workflow.submit", "args": args, "actor": "agent" })
}

fn link_deterministic(rel: &str, args: Value) -> Value {
    json!({ "rel": rel, "method": "workflow.submit", "args": args, "actor": "deterministic" })
}

// ── 1. Terminal state — returns context ───────────────────────────────────

#[tokio::test]
async fn terminal_state_returns_context_immediately() {
    let mcp = ScriptedMcpCaller::new();
    mcp.expect("workflow.get", resp_completed());
    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();

    let ctx = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect("walk completes");
    assert_eq!(ctx, json!({ "summary": "all good" }));
    assert_eq!(mcp.call_count("workflow.get"), 1);
    assert_eq!(mcp.call_count("workflow.submit"), 0);
    assert_eq!(spawner.spawn_count(), 0);
}

// ── 2. Single non-deterministic link — auto-submit ────────────────────────

#[tokio::test]
async fn single_actionable_link_auto_submits() {
    let mcp = ScriptedMcpCaller::new();
    mcp.expect(
        "workflow.get",
        resp_at_state("ready", 1, vec![link("go", json!({ "x": 1 }))], None),
    );
    mcp.expect("workflow.submit", json!({})); // accepted
    mcp.expect("workflow.get", resp_completed());

    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();
    let _ = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect("walk completes");
    let submit_calls = mcp.calls_to("workflow.submit");
    assert_eq!(submit_calls.len(), 1);
    assert_eq!(submit_calls[0], json!({ "x": 1 }));
}

// ── 3. Deterministic-actor links are filtered out ─────────────────────────

#[tokio::test]
async fn deterministic_links_are_ignored_by_interpreter() {
    // The gateway auto-chains deterministic transitions itself (SPEC §6),
    // so the interpreter MUST skip them. Here we provide one
    // deterministic link + one agent link; the interpreter should pick
    // the agent link.
    let mcp = ScriptedMcpCaller::new();
    mcp.expect(
        "workflow.get",
        resp_at_state(
            "branch",
            1,
            vec![
                link_deterministic("auto", json!({ "auto": true })),
                link("go", json!({ "manual": true })),
            ],
            None,
        ),
    );
    mcp.expect("workflow.submit", json!({}));
    mcp.expect("workflow.get", resp_completed());

    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();
    let _ = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect("walk completes");
    let submit_calls = mcp.calls_to("workflow.submit");
    assert_eq!(submit_calls.len(), 1);
    assert_eq!(
        submit_calls[0],
        json!({ "manual": true }),
        "interpreter must skip deterministic link and pick the agent link"
    );
}

// ── 4. Multi-link + escalate present — picks non-escalate ─────────────────

#[tokio::test]
async fn multi_link_with_escalate_picks_non_escalate() {
    let mcp = ScriptedMcpCaller::new();
    mcp.expect(
        "workflow.get",
        resp_at_state(
            "branch",
            1,
            vec![
                link("escalate", json!({ "escalated": true })),
                link("retry", json!({ "retry": true })),
                link("continue", json!({ "continued": true })),
            ],
            None,
        ),
    );
    mcp.expect("workflow.submit", json!({}));
    mcp.expect("workflow.get", resp_completed());

    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();
    let _ = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect("walk completes");
    let submit_calls = mcp.calls_to("workflow.submit");
    assert_eq!(submit_calls.len(), 1);
    assert_eq!(
        submit_calls[0],
        json!({ "retry": true }),
        "must pick first non-escalate link, not the first link"
    );
}

// ── 5. Multi-link, no escalate — picks first link (deterministic fallback)─

#[tokio::test]
async fn multi_link_no_escalate_picks_first_link() {
    let mcp = ScriptedMcpCaller::new();
    mcp.expect(
        "workflow.get",
        resp_at_state(
            "branch",
            1,
            vec![
                link("path_a", json!({ "path": "a" })),
                link("path_b", json!({ "path": "b" })),
            ],
            None,
        ),
    );
    mcp.expect("workflow.submit", json!({}));
    mcp.expect("workflow.get", resp_completed());

    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();
    let _ = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect("walk completes");
    let submit_calls = mcp.calls_to("workflow.submit");
    assert_eq!(submit_calls[0], json!({ "path": "a" }));
}

// ── 6. Delegate state without registered agent → UnknownAgent ─────────────

#[tokio::test]
async fn unknown_delegate_agent_surfaces_actionable_error() {
    let mcp = ScriptedMcpCaller::new();
    mcp.expect(
        "workflow.get",
        resp_at_state("planning", 1, vec![], Some("ghost-agent")),
    );
    let spawner = ScriptedSpawner::new();
    let agents = agent_registry(); // contains "planner", NOT "ghost-agent"

    let err = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect_err("unknown agent must error");
    match err {
        InterpreterError::UnknownAgent { state, agent } => {
            assert_eq!(state, "planning");
            assert_eq!(agent, "ghost-agent");
        }
        other => panic!("expected UnknownAgent, got: {other:?}"),
    }
}

// ── 7. Sub-agent advances workflow → walk continues ───────────────────────

#[tokio::test]
async fn sub_agent_success_advances_workflow_and_continues() {
    let mcp = ScriptedMcpCaller::new();
    // Initial get → delegate state (version 1).
    mcp.expect(
        "workflow.get",
        resp_at_state("planning", 1, vec![], Some("planner")),
    );
    // After sub-agent returns Ok: interpreter re-fetches to confirm
    // the workflow advanced (version 2 means it did).
    mcp.expect(
        "workflow.get",
        resp_at_state("editing", 2, vec![link("done", json!({}))], None),
    );
    // Loop back to top: interpreter calls workflow.get AGAIN before
    // deciding what to do at the new state.
    mcp.expect(
        "workflow.get",
        resp_at_state("editing", 2, vec![link("done", json!({}))], None),
    );
    // Single-link auto-advance.
    mcp.expect("workflow.submit", json!({}));
    mcp.expect("workflow.get", resp_completed());

    let spawner = ScriptedSpawner::new();
    spawner.expect_spawn(Ok(())); // sub-agent claims success
    let agents = agent_registry();
    let _ = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect("walk completes");
    assert_eq!(spawner.spawn_count(), 1);
}

// ── 8. Sub-agent timeout, budget exhausts, no escalate → propagates ───────

#[tokio::test]
async fn sub_agent_timeout_exhausting_budget_without_escalate_propagates() {
    let mcp = ScriptedMcpCaller::new();
    // The interpreter will retry the sub-agent SUB_AGENT_RETRY_BUDGET times.
    // Each iteration: get → spawn (fails) → repeat. After budget exhausts
    // it re-fetches once more and tries to find an escalate link.
    for _ in 0..SUB_AGENT_RETRY_BUDGET {
        mcp.expect(
            "workflow.get",
            resp_at_state("planning", 1, vec![], Some("planner")),
        );
    }
    // After budget exhaust, the interpreter re-fetches before trying
    // escalate.
    mcp.expect(
        "workflow.get",
        resp_at_state("planning", 1, vec![], Some("planner")),
    );

    let spawner = ScriptedSpawner::new();
    for _ in 0..SUB_AGENT_RETRY_BUDGET {
        spawner.expect_spawn(Err(InterpreterError::SubAgentTimeout {
            agent: "planner".into(),
            state: "planning".into(),
        }));
    }
    let agents = agent_registry();
    let err = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect_err("budget exhaust with no escalate must propagate");
    assert!(
        matches!(err, InterpreterError::SubAgentTimeout { .. }),
        "expected SubAgentTimeout, got: {err:?}"
    );
}

// ── 9. Sub-agent timeout, budget exhausts, escalate link present → submits ─

#[tokio::test]
async fn sub_agent_timeout_exhausting_budget_with_escalate_submits_escalate() {
    let mcp = ScriptedMcpCaller::new();
    for _ in 0..SUB_AGENT_RETRY_BUDGET {
        mcp.expect(
            "workflow.get",
            resp_at_state(
                "planning",
                1,
                vec![link("escalate", json!({ "esc": true }))],
                Some("planner"),
            ),
        );
    }
    // Re-fetch before escalate.
    mcp.expect(
        "workflow.get",
        resp_at_state(
            "planning",
            1,
            vec![link("escalate", json!({ "esc": true }))],
            Some("planner"),
        ),
    );
    // Escalate submit accepted.
    mcp.expect("workflow.submit", json!({}));
    // Next loop iteration sees completed (post-escalate workflow done).
    mcp.expect("workflow.get", resp_completed());

    let spawner = ScriptedSpawner::new();
    for _ in 0..SUB_AGENT_RETRY_BUDGET {
        spawner.expect_spawn(Err(InterpreterError::SubAgentTimeout {
            agent: "planner".into(),
            state: "planning".into(),
        }));
    }
    let agents = agent_registry();
    let _ = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect("escalate path completes walk");
    let submits = mcp.calls_to("workflow.submit");
    assert_eq!(submits.len(), 1);
    assert_eq!(submits[0], json!({ "esc": true }));
}

// ── 10. No delegate, no actionable links → WorkflowStuck ──────────────────

#[tokio::test]
async fn no_delegate_no_links_returns_workflow_stuck() {
    let mcp = ScriptedMcpCaller::new();
    mcp.expect("workflow.get", resp_at_state("stuck", 1, vec![], None));
    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();
    let err = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect_err("no links + no delegate must error");
    match err {
        InterpreterError::WorkflowStuck { state } => assert_eq!(state, "stuck"),
        other => panic!("expected WorkflowStuck, got: {other:?}"),
    }
}

// ── 11. Gateway submit rejection surfaces as SubmitRejected ───────────────

#[tokio::test]
async fn gateway_submit_rejection_surfaces_as_submit_rejected() {
    let mcp = ScriptedMcpCaller::new();
    mcp.expect(
        "workflow.get",
        resp_at_state("ready", 1, vec![link("go", json!({}))], None),
    );
    // Gateway returns body-level error (INVALID_TRANSITION-style).
    mcp.expect(
        "workflow.submit",
        json!({
            "error": { "code": "INVALID_TRANSITION", "message": "no such txn" }
        }),
    );

    let spawner = ScriptedSpawner::new();
    let agents = agent_registry();
    let err = walk_workflow(&mcp, &spawner, "wf_x", &agents)
        .await
        .expect_err("body-level error must surface as SubmitRejected");
    assert!(
        matches!(err, InterpreterError::SubmitRejected { .. }),
        "expected SubmitRejected, got: {err:?}"
    );
}
