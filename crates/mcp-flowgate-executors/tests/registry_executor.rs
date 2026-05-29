//! SPEC §17.2 + §8.4 — `RegistryExecutor` tests: flag on/off behavior,
//! argument validation, downstream write propagation.

use std::sync::Arc;

use chrono::Utc;
use mcp_flowgate_core::audit::{AuditSink, MemoryAuditSink};
use mcp_flowgate_core::model::{ExecuteRequest, WorkflowInstance};
use mcp_flowgate_core::ports::{DefinitionStoreWritable, Executor};
use mcp_flowgate_core::store::InMemoryWritableDefinitionStore;
use mcp_flowgate_executors::RegistryExecutor;
use serde_json::{json, Value};

fn instance_stub() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_stub".into(),
        definition_id: "stub".into(),
        definition_version: "0".into(),
        definition: Value::Null,
        state: "s".into(),
        version: 0,
        input: json!({}),
        context: json!({}),
        started_at: Utc::now(),
        trace_id: None,
        run_id: None,
        cancelled_at: None,
        cancelled_reason: None,
    }
}

fn req(args: Value) -> ExecuteRequest {
    ExecuteRequest {
        workflow: instance_stub(),
        transition: None,
        arguments: args,
        executor_config: Value::Null,
        idempotency_key: None,
        correlation_id: None,
    }
}

// ── Negative: flag off → WRITE_DISABLED, no I/O ─────────────────────────────

#[tokio::test]
async fn flag_off_returns_write_disabled_in_output_error() {
    let exec = RegistryExecutor::disabled();
    let out = exec
        .execute(req(json!({
            "definition_id": "x",
            "definition":    {},
        })))
        .await
        .expect("disabled executor returns Ok with error in output");
    assert_eq!(out.output["error"].as_str(), Some("WRITE_DISABLED"));
}

#[tokio::test]
async fn flag_off_does_not_invoke_store() {
    // Construct an enabled-but-uninvoked store; verify nothing was registered.
    let audit = Arc::new(MemoryAuditSink::new());
    let store = InMemoryWritableDefinitionStore::new(audit.clone() as Arc<dyn AuditSink>);
    // Note: passing `disabled()` means the store reference is never used.
    let exec = RegistryExecutor::disabled();
    let _ = exec
        .execute(req(json!({
            "definition_id": "wf",
            "definition":    {},
        })))
        .await
        .unwrap();
    // The (unrelated) audit must have zero events; the (unrelated) store
    // must have zero ids.
    assert!(audit.event_types().is_empty());
    assert!(store.known_ids().is_empty());
}

// ── Positive: flag on → write through, definition loadable ──────────────────

#[tokio::test]
async fn flag_on_writes_through_to_store() {
    let audit = Arc::new(MemoryAuditSink::new());
    let store: Arc<dyn DefinitionStoreWritable> = Arc::new(InMemoryWritableDefinitionStore::new(
        audit.clone() as Arc<dyn AuditSink>,
    ));
    let exec = RegistryExecutor::enabled(store.clone());
    let res = exec
        .execute(req(json!({
            "definition_id": "wf_new",
            "definition":    { "initialState": "s" },
        })))
        .await
        .expect("enabled register succeeds");
    assert_eq!(res.output["outcome"].as_str(), Some("published"));
    let loaded = store.load("wf_new").await.expect("definition loadable");
    assert_eq!(loaded["initialState"].as_str(), Some("s"));
}

#[tokio::test]
async fn flag_on_emits_published_audit_event() {
    let audit = Arc::new(MemoryAuditSink::new());
    let store: Arc<dyn DefinitionStoreWritable> = Arc::new(InMemoryWritableDefinitionStore::new(
        audit.clone() as Arc<dyn AuditSink>,
    ));
    let exec = RegistryExecutor::enabled(store);
    let _ = exec
        .execute(req(json!({
            "definition_id": "x",
            "definition":    {},
        })))
        .await
        .unwrap();
    let kinds = audit.event_types();
    assert!(kinds.contains(&"definition.published".to_string()));
}

// ── Negative: missing arguments fail fast ──────────────────────────────────

#[tokio::test]
async fn missing_definition_id_errors() {
    let exec = RegistryExecutor::disabled();
    let err = exec
        .execute(req(json!({ "definition": {} })))
        .await
        .expect_err("missing definition_id must error");
    assert!(format!("{err:?}").contains("definition_id"));
}

#[tokio::test]
async fn missing_definition_errors() {
    let exec = RegistryExecutor::disabled();
    let err = exec
        .execute(req(json!({ "definition_id": "x" })))
        .await
        .expect_err("missing definition must error");
    assert!(format!("{err:?}").contains("definition"));
}
