//! Failure classification for the Chain-of-Responsibility resolver.
//!
//! **The one rule that prevents a whole class of silent-fallback bugs**
//! (FMECA R1): unknown response status defaults to `ContentOther`, which
//! is **not** infrastructure — so unmapped failures surface to the caller
//! rather than silently triggering CoR fall-through.
//!
//! Closed enum; exhaustive `match` in `from_response`. Tested for 400,
//! 422, an unmapped 4xx (418), and 500-range.

/// What happened when an attempt against a binding failed.
///
/// `is_infrastructure() == true` for failures the resolver treats as
/// "try the next binding"; everything else surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// HTTP 401 — bad/expired API key.
    Auth401,
    /// HTTP 403 — key is valid but lacks permission.
    Auth403,
    /// HTTP 429 — rate-limited.
    RateLimit429,
    /// HTTP 404 — model name unknown / deprecated.
    NotFound404,
    /// Network unreachable, connection timed out, DNS failure, etc.
    NetworkTimeout,
    /// Response body did not match the expected schema.
    ContentSchema,
    /// Provider refused the request on safety grounds.
    ContentSafety,
    /// Any other content-level error, including unmapped HTTP statuses.
    /// **Surfaces** — never triggers CoR fall-through.
    ContentOther,
}

impl FailureClass {
    /// True iff the failure represents *infrastructure* trouble that the
    /// resolver should try to route around (next binding in the list).
    /// False for content-level failures, which the resolver surfaces.
    pub fn is_infrastructure(self) -> bool {
        matches!(
            self,
            FailureClass::Auth401
                | FailureClass::Auth403
                | FailureClass::RateLimit429
                | FailureClass::NotFound404
                | FailureClass::NetworkTimeout
        )
    }

    /// Classify an HTTP response by status code. The body is logged at
    /// the call site for diagnostics; classification itself only reads
    /// the status code so it stays deterministic and testable.
    ///
    /// Unmapped statuses (incl. unusual 4xx like 418 and any 5xx that
    /// isn't a connection failure) → `ContentOther`. The caller surfaces.
    pub fn from_status(status: u16) -> Self {
        match status {
            401 => FailureClass::Auth401,
            403 => FailureClass::Auth403,
            429 => FailureClass::RateLimit429,
            404 => FailureClass::NotFound404,
            502..=504 => FailureClass::NetworkTimeout,
            _ => FailureClass::ContentOther,
        }
    }

    /// Classify a transport-layer error (no HTTP status — connection
    /// never completed). Anything resembling a network timeout/connection
    /// failure maps to `NetworkTimeout`; everything else → `ContentOther`
    /// (surface). The conservative default holds even here.
    pub fn from_io_error(kind: std::io::ErrorKind) -> Self {
        use std::io::ErrorKind::*;
        match kind {
            TimedOut | ConnectionRefused | ConnectionReset | ConnectionAborted
            | NotConnected | HostUnreachable | NetworkUnreachable | NetworkDown
            | Interrupted => FailureClass::NetworkTimeout,
            _ => FailureClass::ContentOther,
        }
    }
}
