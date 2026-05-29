use thiserror::Error;

/// Errors raised by the workflow runtime when applying a transition.
///
/// Distinct from [`ExecutorError`]: those classify *executor* failures so
/// reliability policies can retry. A `RuntimeError` is a hard stop in the
/// commit path that must abort the transition and propagate to the caller.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// The transition record (a `workflow.transition` audit event) could not be
    /// written. Because records are emitted *record-first* — before the
    /// authoritative state snapshot is committed — a record-write failure means
    /// the transition must fail fast and the snapshot must NOT be committed.
    /// The message names the workflow id and the `seq` (resulting version) so
    /// operators can pinpoint exactly which transition was aborted.
    #[error("RECORD_WRITE_FAILED: failed to write transition record for workflow '{workflow_id}' at seq {seq}: {source}")]
    RecordWriteFailed {
        workflow_id: String,
        seq: u64,
        #[source]
        source: anyhow::Error,
    },

    /// SPEC §32 — `run_id` uniqueness assertion on `workflow.start`. When the
    /// caller supplies a `runId` and the store already has a live instance
    /// indexed under that id, `start` is rejected here rather than creating a
    /// duplicate. The MCP layer surfaces this as a structured
    /// `RUN_ID_ALREADY_RUNNING` response with a HATEOAS `get` link to the
    /// existing instance.
    #[error(
        "run_id '{run_id}' is already in flight (existing workflow id: {existing_workflow_id})"
    )]
    RunIdAlreadyRunning {
        run_id: String,
        existing_workflow_id: String,
    },

    /// SPEC §30.10.4-5 — pre-start subject walk found a placeholder subject.
    ///
    /// Raised in `WorkflowRuntime::start` when the workflow definition's
    /// `_lexiconLibrary` contains an entry with `state: "PENDING_DEFINITION"`.
    /// The runtime must NOT create the workflow instance. The MCP layer
    /// translates this into a structured `SUBJECT_NEEDS_DEFINITION` interaction
    /// response per §30.10.5.
    #[error("subject '{unknown_subject}' is unresolved in workflow '{workflow_id_context}'")]
    SubjectNeedsDefinition {
        /// The placeholder term that has no lexicon definition.
        unknown_subject: String,
        /// Optional bounded context from the placeholder entry (if any).
        bounded_context: Option<String>,
        /// The `encountered_in` context, formatted as `"workflow:<id>"`.
        workflow_id_context: String,
    },

    /// SPEC §30.10.10 — the configured embedding backend failed during a
    /// lexicon write or a SUBJECT_NEEDS_DEFINITION candidate ranking call.
    ///
    /// When the operator has configured a non-`none` embedding backend,
    /// failures at write time must be surfaced as a structured error so
    /// callers can distinguish "backend down" from other write errors.
    #[error("EMBEDDING_BACKEND_FAILED: {message}")]
    EmbeddingBackendFailed { message: String },
}

impl RuntimeError {
    /// Stable error code token, mirroring the `code` strings used elsewhere in
    /// the runtime (e.g. `ACTOR_MISMATCH`, `STALE_WORKFLOW_VERSION`).
    pub fn code(&self) -> &'static str {
        match self {
            RuntimeError::RecordWriteFailed { .. } => "RECORD_WRITE_FAILED",
            RuntimeError::RunIdAlreadyRunning { .. } => "RUN_ID_ALREADY_RUNNING",
            RuntimeError::SubjectNeedsDefinition { .. } => "SUBJECT_NEEDS_DEFINITION",
            RuntimeError::EmbeddingBackendFailed { .. } => "EMBEDDING_BACKEND_FAILED",
        }
    }
}

/// Classified executor errors. Reliability policies retry / fall back based on
/// the variant, so executors should classify failures here rather than wrapping
/// everything as `Other`.
#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("timeout after {0} ms")]
    Timeout(u64),

    #[error("rate limited: {0}")]
    RateLimited(String),

    #[error("connection error: {0}")]
    Connection(String),

    #[error("transient error: {0}")]
    Transient(String),

    #[error("permanent error: {0}")]
    Permanent(String),

    /// SPEC §5.3 — a capability produced an output that failed validation
    /// against its declared `snippet.outputs` schema. The message carries
    /// the structured violation diff (slot name + jsonschema reason). The
    /// variant is distinct from [`ExecutorError::Permanent`] so reliability
    /// policy can refuse to retry contract-typing failures explicitly and
    /// so audit emitters can recognize this class of failure as a
    /// `cap.output.schema_violation` event without text-matching the
    /// `Permanent(..)` payload. Classifies as `ErrorClass::Permanent`
    /// (never retryable).
    #[error("schema violation: {0}")]
    SchemaViolation(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl ExecutorError {
    pub fn class(&self) -> ErrorClass {
        match self {
            ExecutorError::Timeout(_) => ErrorClass::Timeout,
            ExecutorError::RateLimited(_) => ErrorClass::RateLimited,
            ExecutorError::Connection(_) => ErrorClass::Connection,
            ExecutorError::Transient(_) => ErrorClass::Transient,
            ExecutorError::Permanent(_) => ErrorClass::Permanent,
            ExecutorError::SchemaViolation(_) => ErrorClass::Permanent,
            ExecutorError::Other(_) => ErrorClass::Permanent,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Timeout,
    RateLimited,
    Connection,
    Transient,
    Permanent,
}

impl ErrorClass {
    pub fn token(self) -> &'static str {
        match self {
            ErrorClass::Timeout => "timeout",
            ErrorClass::RateLimited => "rate_limited",
            ErrorClass::Connection => "connection_error",
            ErrorClass::Transient => "transient_error",
            ErrorClass::Permanent => "permanent_error",
        }
    }
}
