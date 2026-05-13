use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
    pub correlation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub event_type: String,
    pub payload: Value,
}

impl AuditEvent {
    pub fn new(event_type: impl Into<String>) -> Self {
        Self {
            id: format!("evt_{}", Uuid::new_v4().simple()),
            timestamp: Utc::now(),
            workflow_id: None,
            correlation_id: format!("cor_{}", Uuid::new_v4().simple()),
            actor: None,
            event_type: event_type.into(),
            payload: json!({}),
        }
    }

    pub fn with_workflow(mut self, workflow_id: impl Into<String>) -> Self {
        self.workflow_id = Some(workflow_id.into());
        self
    }

    pub fn with_correlation(mut self, correlation_id: impl Into<String>) -> Self {
        self.correlation_id = correlation_id.into();
        self
    }

    pub fn with_actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = Some(actor.into());
        self
    }

    pub fn with_payload(mut self, payload: Value) -> Self {
        self.payload = payload;
        self
    }
}

#[async_trait]
pub trait AuditSink: Send + Sync {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()>;

    /// Return all recorded events. Returns `None` if the sink doesn't
    /// support retrieval (stdout, null).
    async fn list_events(&self) -> Option<Vec<AuditEvent>> {
        None
    }
}

/// Drops every event. Useful as a default when audit isn't configured.
pub struct NullAuditSink;

#[async_trait]
impl AuditSink for NullAuditSink {
    async fn record(&self, _event: AuditEvent) -> anyhow::Result<()> {
        Ok(())
    }

    async fn list_events(&self) -> Option<Vec<AuditEvent>> {
        None
    }
}

/// Writes one JSON line per event to stdout.
pub struct StdoutAuditSink;

#[async_trait]
impl AuditSink for StdoutAuditSink {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()> {
        let line = serde_json::to_string(&event)?;
        println!("{line}");
        Ok(())
    }

    async fn list_events(&self) -> Option<Vec<AuditEvent>> {
        None
    }
}

/// Stores events in memory. Cheap, useful for tests and short-lived processes.
#[derive(Default, Clone)]
pub struct MemoryAuditSink {
    events: Arc<Mutex<Vec<AuditEvent>>>,
}

impl MemoryAuditSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<AuditEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn event_types(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.event_type.clone())
            .collect()
    }

    pub fn clear(&self) {
        self.events.lock().unwrap().clear();
    }
}

#[async_trait]
impl AuditSink for MemoryAuditSink {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }

    async fn list_events(&self) -> Option<Vec<AuditEvent>> {
        Some(self.snapshot())
    }
}

/// Appends one JSON line per event to a file. Opens for each write to keep the
/// implementation tiny — fine for low-throughput audit streams.
pub struct FileAuditSink {
    path: std::path::PathBuf,
    lock: tokio::sync::Mutex<()>,
}

impl FileAuditSink {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: tokio::sync::Mutex::new(()),
        }
    }
}

#[async_trait]
impl AuditSink for FileAuditSink {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt;
        let _guard = self.lock.lock().await;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        file.write_all(&line).await?;
        Ok(())
    }

    async fn list_events(&self) -> Option<Vec<AuditEvent>> {
        let content = tokio::fs::read_to_string(&self.path).await.ok()?;
        let events: Vec<AuditEvent> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        Some(events)
    }
}
