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
    /// SPEC §20.2 — caller-supplied trace id propagated to every audit
    /// event for this instance. Captured at `workflow.start` and persisted
    /// with the snapshot so it survives reload + drain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// SPEC §20.2 — caller-supplied run id, same lifecycle as `trace_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

impl WorkflowInstance {
    /// SPEC §20.2 — build an audit event pre-decorated with this
    /// instance's `workflow_id`, `trace_id`, and `run_id`. Use this at
    /// emission sites so the three identifiers stay in sync without
    /// boilerplate at every call site.
    pub fn audit_event(&self, event_type: impl Into<String>) -> crate::audit::AuditEvent {
        let mut e = crate::audit::AuditEvent::new(event_type).with_workflow(&self.id);
        if let Some(t) = &self.trace_id {
            e = e.with_trace_id(t.clone());
        }
        if let Some(r) = &self.run_id {
            e = e.with_run_id(r.clone());
        }
        e
    }
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

#[derive(Debug, Clone, Default)]
pub struct StartWorkflow {
    pub definition_id: WorkflowDefinitionId,
    pub input: Value,
    pub principal: Principal,
    /// SPEC §20.2 — optional trace id propagated to every audit event
    /// for the created instance. Persisted on the instance.
    pub trace_id: Option<String>,
    /// SPEC §20.2 — optional run id, same lifecycle as `trace_id`.
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GetWorkflow {
    pub workflow_id: WorkflowId,
    pub principal: Principal,
    /// SPEC §20.2 — optional trace id for any audit events this call
    /// emits (the existing instance's persisted trace_id is preserved
    /// and used unless this is explicitly set to override).
    pub trace_id: Option<String>,
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SubmitTransition {
    pub workflow_id: WorkflowId,
    pub expected_version: u64,
    pub transition: TransitionName,
    pub arguments: Value,
    pub principal: Principal,
    /// SPEC §6.3 — optional model-authored summary. When present, the runtime
    /// stores it to `context.summary` on commit. It is **never** a guard input
    /// (model-authored content is untrusted); `check` errors on any guard that
    /// reads `$.context.summary`.
    pub summary: Option<String>,
    /// SPEC §20.2 — optional per-submit trace id. The instance's
    /// persisted `trace_id` is used by default; this override lets a
    /// caller stitch a single submit into a different trace
    /// (e.g. a re-evaluation run replaying a recorded session).
    pub trace_id: Option<String>,
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub kind: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// SPEC §20.1 — content-identity of the artifact this evidence
    /// references. Convention: `sha256:` prefix + lowercase-hex digest of
    /// the artifact bytes. Optional; populate when the artifact is
    /// byte-stable (verifier-produced JUnit, SARIF, coverage JSON, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// SPEC §20.1 — model-stated confidence (0.0..=1.0) that this evidence
    /// supports the claim it's attached to. Out-of-range values fail
    /// validation with `INVALID_CONFIDENCE`. Deterministic executors
    /// typically omit; model-authored evidence SHOULD populate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

impl Evidence {
    /// SPEC §20.1 — validate that `confidence` (if present) is within the
    /// allowed range. Producers MUST call this before persisting.
    /// Returns the offending value on rejection so error messages can name
    /// the violator.
    ///
    /// ```
    /// use mcp_flowgate_core::model::Evidence;
    ///
    /// let ok = Evidence {
    ///     kind: "test".into(),
    ///     id: "ev_1".into(),
    ///     uri: None,
    ///     summary: None,
    ///     digest: None,
    ///     confidence: Some(0.85),
    /// };
    /// assert!(ok.validate_confidence().is_ok());
    ///
    /// let too_high = Evidence { confidence: Some(1.5), ..ok.clone() };
    /// assert_eq!(too_high.validate_confidence(), Err(1.5));
    ///
    /// let absent = Evidence { confidence: None, ..ok };
    /// assert!(absent.validate_confidence().is_ok());
    /// ```
    pub fn validate_confidence(&self) -> Result<(), f32> {
        match self.confidence {
            Some(c) if !(0.0..=1.0).contains(&c) => Err(c),
            _ => Ok(()),
        }
    }
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
    /// SPEC §7.2 — when this executor is `kind: workflow`, the id of the
    /// sub-workflow it started. Surfaced on the parent's transition record
    /// as `childWorkflowId` so audit reconstruction can follow the chain.
    /// `None` for every other executor kind.
    pub child_workflow_id: Option<String>,
}
