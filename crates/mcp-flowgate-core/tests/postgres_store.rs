//! Integration tests for PostgresWorkflowStore.
//!
//! These tests require a running Postgres instance. Set the
//! `POSTGRES_TEST_URL` environment variable to point to it:
//!
//! ```bash
//! export POSTGRES_TEST_URL="postgres://postgres:postgres@localhost:5432/flowgate_test"
//! cargo test --test postgres_store
//! ```
//!
//! If the env var is not set, tests are skipped.

use mcp_flowgate_core::model::WorkflowInstance;
use mcp_flowgate_core::ports::WorkflowStore;
use mcp_flowgate_core::store_postgres::PostgresWorkflowStore;
use serde_json::json;

fn get_test_url() -> Option<String> {
    std::env::var("POSTGRES_TEST_URL")
        .ok()
        .filter(|u| !u.is_empty())
}

async fn create_store() -> PostgresWorkflowStore {
    let url = get_test_url().expect("POSTGRES_TEST_URL must be set for this test");
    PostgresWorkflowStore::connect(&url).await.unwrap()
}

fn make_instance(id: &str, version: u64, state: &str) -> WorkflowInstance {
    WorkflowInstance {
        id: id.to_string(),
        definition_id: "test_def".to_string(),
        definition_version: "1.0.0".to_string(),
        definition: json!({
            "version": "1.0.0",
            "initialState": "running",
            "states": { "running": {}, "completed": { "terminal": true } }
        }),
        state: state.to_string(),
        version,
        input: json!({"key": "value"}),
        context: json!({"count": 0}),
        started_at: chrono::Utc::now(),
        trace_id: None,
        run_id: None,
    }
}

#[tokio::test]
async fn create_and_load_roundtrip() {
    if get_test_url().is_none() {
        eprintln!("skipping postgres test: POSTGRES_TEST_URL not set");
        return;
    }
    let store = create_store().await;
    let instance = make_instance("create_load_test", 1, "running");
    store.create(instance.clone()).await.unwrap();
    let loaded = store.load("create_load_test").await.unwrap();
    assert_eq!(loaded.id, instance.id);
    assert_eq!(loaded.state, "running");
    assert_eq!(loaded.version, 1);
}

#[tokio::test]
async fn save_if_version_succeeds_on_match() {
    if get_test_url().is_none() {
        eprintln!("skipping postgres test: POSTGRES_TEST_URL not set");
        return;
    }
    let store = create_store().await;
    let instance = make_instance("save_match_test", 1, "running");
    store.create(instance.clone()).await.unwrap();

    let mut updated = instance.clone();
    updated.state = "completed".to_string();
    updated.version = 2;
    let saved = store.save_if_version(updated.clone(), 1).await.unwrap();
    assert_eq!(saved.state, "completed");
    assert_eq!(saved.version, 2);
}

#[tokio::test]
async fn save_if_version_rejects_stale() {
    if get_test_url().is_none() {
        eprintln!("skipping postgres test: POSTGRES_TEST_URL not set");
        return;
    }
    let store = create_store().await;
    let instance = make_instance("save_stale_test", 1, "running");
    store.create(instance.clone()).await.unwrap();

    let mut updated = instance.clone();
    updated.state = "completed".to_string();
    updated.version = 2;
    let err = store
        .save_if_version(updated.clone(), 999)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("stale"),
        "expected stale version error, got: {err}"
    );
}

#[tokio::test]
async fn concurrent_create_detects_collision() {
    if get_test_url().is_none() {
        eprintln!("skipping postgres test: POSTGRES_TEST_URL not set");
        return;
    }
    let store = create_store().await;
    let instance = make_instance("collision_test", 1, "running");
    store.create(instance.clone()).await.unwrap();
    let err = store.create(instance.clone()).await.unwrap_err();
    assert!(
        err.to_string().contains("collision"),
        "expected collision error, got: {err}"
    );
}

#[tokio::test]
async fn two_process_optimistic_lock() {
    if get_test_url().is_none() {
        eprintln!("skipping postgres test: POSTGRES_TEST_URL not set");
        return;
    }
    let store = create_store().await;
    let instance = make_instance("two_process_test", 1, "running");
    store.create(instance.clone()).await.unwrap();

    // Process A loads (v=1)
    let loaded_a = store.load("two_process_test").await.unwrap();
    assert_eq!(loaded_a.version, 1);

    // Process B loads (v=1)
    let loaded_b = store.load("two_process_test").await.unwrap();
    assert_eq!(loaded_b.version, 1);

    // Process B saves (v=1 -> v=2)
    let mut updated_b = loaded_b.clone();
    updated_b.state = "reviewing".to_string();
    updated_b.version = 2;
    store.save_if_version(updated_b.clone(), 1).await.unwrap();

    // Process A tries to save (v=1 expected) -> rejected
    let mut updated_a = loaded_a.clone();
    updated_a.state = "approved".to_string();
    updated_a.version = 2;
    let err = store
        .save_if_version(updated_a.clone(), 1)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("stale"),
        "expected stale version error, got: {err}"
    );

    // Process A re-loads (v=2) and saves successfully
    let reloaded = store.load("two_process_test").await.unwrap();
    assert_eq!(reloaded.version, 2);
    let mut updated_a2 = reloaded.clone();
    updated_a2.state = "approved".to_string();
    updated_a2.version = 3;
    store.save_if_version(updated_a2.clone(), 2).await.unwrap();

    let final_check = store.load("two_process_test").await.unwrap();
    assert_eq!(final_check.state, "approved");
    assert_eq!(final_check.version, 3);
}
