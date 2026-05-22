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
}

impl RuntimeError {
    /// Stable error code token, mirroring the `code` strings used elsewhere in
    /// the runtime (e.g. `ACTOR_MISMATCH`, `STALE_WORKFLOW_VERSION`).
    pub fn code(&self) -> &'static str {
        match self {
            RuntimeError::RecordWriteFailed { .. } => "RECORD_WRITE_FAILED",
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
