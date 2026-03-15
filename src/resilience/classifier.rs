//! Generic error classification for resilience layers.

use std::time::Duration;

/// Classifies errors to determine how resilience layers should respond.
///
/// Each client type (LLM, MCP, HTTP tool, etc.) implements this trait
/// to tell the resilience layers how to handle its specific error type.
pub trait ErrorClassifier<E> {
    /// Should the same request be retried against the same endpoint?
    fn is_retryable(&self, err: &E) -> bool;

    /// Does this error indicate the backend is degraded?
    /// Used by circuit breakers to track health.
    fn is_transient(&self, err: &E) -> bool;

    /// Provider-suggested retry delay (e.g. from Retry-After header).
    fn retry_after(&self, _err: &E) -> Option<Duration> {
        None
    }
}
