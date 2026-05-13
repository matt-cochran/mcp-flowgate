use thiserror::Error;

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
