use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type WorkflowId = String;
pub type WorkflowDefinitionId = String;
pub type StateName = String;
pub type TransitionName = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowInstance {
    pub id: WorkflowId,
    pub definition_id: WorkflowDefinitionId,
    pub definition_version: String,
    /// The resolved workflow definition snapshot this instance was started
    /// with (SPEC §8.2 / §8.3). Captured once at `workflow.start` from the
    /// `DefinitionStore` and persisted with the instance. Every in-flight
    /// operation (`get`, `submit`, deterministic chaining, timeout) resolves
    /// the definition from *this* field — never from the live config — so
    /// editing or hot-reloading config never disturbs a running instance.
    pub definition: Value,
    pub state: StateName,
    pub version: u64,
    pub input: Value,
    pub context: Value,
    /// When this workflow instance was created. Used by lazy timeout
    /// checks: if the next `submit` or `get` happens after
    /// `definition.timeoutMs` elapsed, the instance auto-transitions to
    /// `definition.onTimeout.target`. Defaults to `Utc::now()` for
    /// instances loaded from older stores that didn't persist this field.
    #[serde(default = "Utc::now")]
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct Principal {
    pub subject: String,
    pub roles: Vec<String>,
    pub permissions: Vec<String>,
}

impl Principal {
    pub fn anonymous() -> Self {
        Self {
            subject: "anonymous".to_string(),
            roles: Vec::new(),
            permissions: Vec::new(),
        }
    }

    /// Role marker convention used by the runtime to recognise a human
    /// principal. Embedders that wire identity per request (see
    /// `docs/EMBEDDING.md`) tag human callers with this role; agent-driven
    /// invocations leave it absent. `actor: "human"` transitions reject
    /// submissions from principals without this role.
    pub const HUMAN_ROLE: &'static str = "human";

    pub fn is_human(&self) -> bool {
        self.roles.iter().any(|r| r == Self::HUMAN_ROLE)
    }
}

#[derive(Debug, Clone)]
pub struct StartWorkflow {
    pub definition_id: WorkflowDefinitionId,
    pub input: Value,
    pub principal: Principal,
}

#[derive(Debug, Clone)]
pub struct GetWorkflow {
    pub workflow_id: WorkflowId,
    pub principal: Principal,
}

#[derive(Debug, Clone)]
pub struct SubmitTransition {
    pub workflow_id: WorkflowId,
    pub expected_version: u64,
    pub transition: TransitionName,
    pub arguments: Value,
    pub principal: Principal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub kind: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExecuteRequest {
    pub workflow: WorkflowInstance,
    pub transition: Option<String>,
    pub arguments: Value,
    pub executor_config: Value,
    /// Idempotency key for this execute call. Computed once per
    /// `execute_with_reliability` invocation, identical across retries and
    /// across primary + fallback candidates so downstream services can
    /// dedupe. None when the executor config didn't request one.
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ExecuteResult {
    pub output: Value,
    pub evidence: Vec<Evidence>,
}
