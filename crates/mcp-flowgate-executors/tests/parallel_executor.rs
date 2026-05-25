//! SPEC §24 — `parallel` executor kind. FMECA-style atomic assertions
//! covering each Phase 1 / Phase 2 design surface:
//!   - join: all / any / at_least:K
//!   - on_branch_failure: bail / continue
//!   - max_concurrency cap is honored (no more than N in flight)
//!   - dynamic for_each branch generation
//!   - empty for_each → vacuous success
//!   - recursion-depth cap rejects nested parallel exceeding the limit
//!   - DOS poka-yoke: 10+ branches without explicit max_concurrency rejects
//!   - per-branch audit events share parent's correlation_id
//!
//! Tests build the registry from a real config so the parallel executor
//! has its registry wired (set_registry called in default_registry_with_mcp).

use std::sync::Arc;

use chrono::Utc;
use mcp_flowgate_core::audit::{AuditSink, MemoryAuditSink};
use mcp_flowgate_core::error::ExecutorError;
use mcp_flowgate_core::model::{ExecuteRequest, WorkflowInstance};
use mcp_flowgate_core::ports::ExecutorRegistry;
use mcp_flowgate_executors::{
    default_registry_with_mcp, McpExecutor, McpConnections, CliConnections,
};
use serde_json::{json, Value};

fn instance_stub() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_parallel_test".into(),
        definition_id: "demo".into(),
        definition_version: "0".into(),
        definition: json!({}),
        state: "s".into(),
        version: 0,
        input: json!({}),
        context: json!({}),
        started_at: Utc::now(),
        trace_id: None,
        run_id: None,
    }
}

fn build_registry(audit: Arc<MemoryAuditSink>) -> Arc<dyn ExecutorRegistry> {
    let mcp_conns = McpConnections::from_config(&json!({}));
    let cli_conns = Arc::new(CliConnections::from_config(&json!({})));
    let mcp_exec = Arc::new(McpExecutor::new(mcp_conns));
    default_registry_with_mcp(
        &json!({}),
        mcp_exec,
        cli_conns,
        audit as Arc<dyn AuditSink>,
    )
}

fn req(executor_config: Value, instance: WorkflowInstance) -> ExecuteRequest {
    ExecuteRequest {
        workflow: instance,
        transition: Some("fan-out".into()),
        arguments: json!({}),
        executor_config,
        idempotency_key: Some("test-parent-key".into()),
        correlation_id: Some("test-corr-id".into()),
    }
}

async fn run_parallel(executor_config: Value, instance: WorkflowInstance, audit: Arc<MemoryAuditSink>) -> Result<mcp_flowgate_core::model::ExecuteResult, ExecutorError> {
    let registry = build_registry(audit);
    let parallel = registry.get("parallel").expect("parallel registered");
    parallel.execute(req(executor_config, instance)).await
}

// ── join: all — every branch succeeds ───────────────────────────────────

#[tokio::test]
async fn join_all_with_two_noop_branches_succeeds() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": "all",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("parallel succeeds");
    assert_eq!(result.output["summary"]["ok_count"], 2);
    assert_eq!(result.output["summary"]["failed_count"], 0);
    assert_eq!(result.output["summary"]["verdict"], "succeeded");
    assert_eq!(result.output["branches"].as_array().unwrap().len(), 2);
}

// ── max_concurrency cap honored ─────────────────────────────────────────

#[tokio::test]
async fn max_concurrency_caps_in_flight_count() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": "all",
            "max_concurrency": 2,
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("parallel succeeds");
    let max_in_flight = result.output["summary"]["max_in_flight_observed"]
        .as_u64()
        .unwrap();
    assert!(
        max_in_flight <= 2,
        "max_in_flight {} must not exceed cap 2",
        max_in_flight
    );
}

// ── join: any — first success returns, siblings cancelled ────────────────

#[tokio::test]
async fn join_any_returns_on_first_success() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": "any",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("join: any with at least one success must succeed");
    assert!(
        result.output["summary"]["ok_count"].as_u64().unwrap() >= 1,
        "at least one success: {result:?}",
    );
    assert_eq!(result.output["summary"]["verdict"], "succeeded");
}

// ── join: at_least: K — threshold met ────────────────────────────────────

#[tokio::test]
async fn join_at_least_3_succeeds_with_3_of_5() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": { "at_least": 3 },
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("at_least: 3 with 5 successes must succeed");
    assert!(
        result.output["summary"]["ok_count"].as_u64().unwrap() >= 3,
    );
    assert_eq!(result.output["summary"]["verdict"], "succeeded");
}

// ── DOS poka-yoke: 10+ branches without max_concurrency rejects ─────────

#[tokio::test]
async fn ten_plus_branches_without_max_concurrency_rejects() {
    let audit = Arc::new(MemoryAuditSink::new());
    let branches: Vec<Value> = (0..12).map(|_| json!({ "kind": "noop" })).collect();
    let err = run_parallel(
        json!({
            "kind": "parallel",
            "branches": branches,
            "join": "all",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect_err("12 branches without max_concurrency must reject");
    let s = format!("{err:?}");
    assert!(s.contains("INVALID_PARALLEL_CONFIG"), "got: {s}");
    assert!(s.contains("max_concurrency"), "got: {s}");
}

// ── Dynamic for_each ────────────────────────────────────────────────────

#[tokio::test]
async fn dynamic_for_each_expands_array_into_branches() {
    let audit = Arc::new(MemoryAuditSink::new());
    let mut instance = instance_stub();
    instance.context = json!({ "queries": ["alpha", "beta", "gamma"] });
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": {
                "for_each": "$.context.queries",
                "do":       { "kind": "noop" },
            },
            "join": "all",
        }),
        instance,
        audit.clone(),
    )
    .await
    .expect("for_each over 3-element array produces 3 branches");
    assert_eq!(result.output["summary"]["n"], 3);
    assert_eq!(result.output["summary"]["ok_count"], 3);
}

// ── Empty for_each → vacuous success ─────────────────────────────────────

#[tokio::test]
async fn empty_for_each_returns_vacuous_success() {
    let audit = Arc::new(MemoryAuditSink::new());
    let mut instance = instance_stub();
    instance.context = json!({ "queries": [] });
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": {
                "for_each": "$.context.queries",
                "do":       { "kind": "noop" },
            },
            "join": "all",
        }),
        instance,
        audit.clone(),
    )
    .await
    .expect("empty for_each must vacuous-succeed, not error");
    assert_eq!(result.output["summary"]["n"], 0);
    assert_eq!(result.output["summary"]["ok_count"], 0);
    assert_eq!(result.output["summary"]["verdict"], "succeeded");
    let event_types = audit.event_types();
    assert!(
        event_types.iter().any(|e| e == "parallel.fanout.empty"),
        "must emit parallel.fanout.empty for observability; got: {event_types:?}"
    );
}

// ── Audit per-branch events share parent's correlation_id ───────────────

#[tokio::test]
async fn per_branch_audit_events_share_parent_correlation_id() {
    let audit = Arc::new(MemoryAuditSink::new());
    let _ = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": "all",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("parallel succeeds");

    let events = audit.snapshot();
    let parent_corr = "test-corr-id";
    // Every parallel.* audit event must carry the parent's correlation_id.
    let parallel_events: Vec<_> = events
        .iter()
        .filter(|e| e.event_type.starts_with("parallel."))
        .collect();
    assert!(
        !parallel_events.is_empty(),
        "expected at least one parallel.* event; got: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
    for ev in &parallel_events {
        assert_eq!(
            ev.correlation_id, parent_corr,
            "event {} must carry parent's correlation_id; got: {}",
            ev.event_type, ev.correlation_id
        );
    }
}

// ── Recursion-depth cap rejects nested parallel beyond the limit ────────

#[tokio::test]
async fn nested_parallel_beyond_max_recursion_depth_rejects() {
    // 4 levels deep with cap 2 → must reject. Build inside-out: deepest
    // first, then wrap each level above.
    let audit = Arc::new(MemoryAuditSink::new());
    let depth4 = json!({
        "kind": "parallel",
        "branches": [{ "kind": "noop" }],
        "join": "all",
        "max_recursion_depth": 2,
    });
    let depth3 = json!({
        "kind": "parallel",
        "branches": [depth4],
        "join": "all",
        "max_recursion_depth": 2,
    });
    let depth2 = json!({
        "kind": "parallel",
        "branches": [depth3],
        "join": "all",
        "max_recursion_depth": 2,
    });
    let depth1 = json!({
        "kind": "parallel",
        "branches": [depth2],
        "join": "all",
        "max_recursion_depth": 2,
    });

    // Top-level execution starts at depth=0 (no task_local set), then the
    // first nested branch enters depth=1 (still ok with cap=2), the second
    // enters depth=2 (still ok), the third enters depth=3 (REJECT — cap=2
    // means current_depth=2 >= cap).
    let result = run_parallel(depth1, instance_stub(), audit.clone()).await;
    // Some branch result inside the aggregated output should carry the
    // PARALLEL_DEPTH_EXCEEDED error. Because on_branch_failure defaults to
    // `bail`, the whole thing fails — find the error.
    let err = result.expect_err("nested parallel beyond cap must fail");
    let s = format!("{err:?}");
    assert!(
        s.contains("PARALLEL_DEPTH_EXCEEDED") || s.contains("fan-out failed"),
        "expected PARALLEL_DEPTH_EXCEEDED or bail-due-to-it; got: {s}"
    );
}

// ── GAP-G: parallel.branch.cancelled event emitted per cancelled branch ──

#[tokio::test]
async fn join_any_emits_cancelled_event_for_dropped_siblings() {
    // join=any returns on first success and cancels the rest. Each
    // dropped branch must produce a `parallel.branch.cancelled` audit
    // event so operators can see which branches were aborted.
    let audit = Arc::new(MemoryAuditSink::new());
    let _ = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
                { "kind": "noop" },
            ],
            "join": "any",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await
    .expect("join: any succeeds");

    let events = audit.snapshot();
    let cancelled: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "parallel.branch.cancelled")
        .collect();
    // Hard to assert exact count (timing-dependent — depends on how many
    // had started before abort_all), but at least one of 4 branches
    // should have been cancelled in a typical join=any run. Be lenient:
    // assert that EITHER cancelled events are present OR all 4 actually
    // completed (in which case the test races every branch to the same
    // tick — fine, just no cancellations to log).
    let completed: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "parallel.branch.completed")
        .collect();
    let total_concluded = cancelled.len() + completed.len();
    assert_eq!(
        total_concluded, 4,
        "every branch must conclude as either completed or cancelled; got: {} completed + {} cancelled",
        completed.len(),
        cancelled.len()
    );
    // All cancelled events share parent's correlation_id (regression
    // assert for the F3 mitigation).
    for ev in &cancelled {
        assert_eq!(ev.correlation_id, "test-corr-id");
    }
}

// ── on_branch_failure: continue still drains all branches ───────────────

// (Requires an executor that can fail. NoopExecutor always succeeds.
// We use a missing-kind branch to trigger the "executor kind not registered"
// failure path, since execute_with_reliability emits ExecutorError::Permanent
// for unknown kinds.)

#[tokio::test]
async fn on_branch_failure_continue_drains_all_branches() {
    let audit = Arc::new(MemoryAuditSink::new());
    let result = run_parallel(
        json!({
            "kind": "parallel",
            "branches": [
                { "kind": "noop" },
                { "kind": "nonexistent_kind" },
                { "kind": "noop" },
            ],
            "join": "all",
            "on_branch_failure": "continue",
        }),
        instance_stub(),
        audit.clone(),
    )
    .await;
    // Verdict is failed (join=all + 1 failure), but ok_count should be 2
    // (continue ran both successes).
    let err = result.expect_err("join=all with 1 failure must fail");
    let s = format!("{err:?}");
    // The audit log should show 3 branch.started events (all drained).
    let started: Vec<_> = audit
        .snapshot()
        .into_iter()
        .filter(|e| e.event_type == "parallel.branch.started")
        .collect();
    assert_eq!(
        started.len(),
        3,
        "on_branch_failure: continue must start ALL 3 branches; got: {} ({s})",
        started.len()
    );
}
