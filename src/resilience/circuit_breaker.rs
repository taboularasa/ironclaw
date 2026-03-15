//! Generic circuit breaker with Closed/Open/HalfOpen state machine.
//!
//! Extracted from `llm::circuit_breaker` to be reusable across any
//! external service client.

use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use super::classifier::ErrorClassifier;

/// Configuration for the circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Consecutive transient failures before the circuit opens.
    pub failure_threshold: u32,
    /// How long the circuit stays open before allowing a probe.
    pub recovery_timeout: Duration,
    /// Successful probes needed in half-open to close the circuit.
    pub half_open_successes_needed: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            recovery_timeout: Duration::from_secs(30),
            half_open_successes_needed: 2,
        }
    }
}

/// Circuit breaker states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

struct BreakerState {
    state: CircuitState,
    consecutive_failures: u32,
    opened_at: Option<Instant>,
    half_open_successes: u32,
}

impl BreakerState {
    fn new() -> Self {
        Self {
            state: CircuitState::Closed,
            consecutive_failures: 0,
            opened_at: None,
            half_open_successes: 0,
        }
    }
}

/// Generic circuit breaker layer.
///
/// Wraps any async operation. Tracks consecutive transient failures and
/// trips open after the threshold, fast-failing subsequent calls until
/// the recovery timeout elapses.
pub struct CircuitBreakerLayer<C> {
    state: Mutex<BreakerState>,
    config: CircuitBreakerConfig,
    classifier: C,
    /// Label for log messages.
    label: String,
}

impl<C> CircuitBreakerLayer<C> {
    pub fn new(config: CircuitBreakerConfig, classifier: C, label: impl Into<String>) -> Self {
        Self {
            state: Mutex::new(BreakerState::new()),
            config,
            classifier,
            label: label.into(),
        }
    }

    /// Current circuit state.
    pub async fn circuit_state(&self) -> CircuitState {
        self.state.lock().await.state
    }

    /// Number of consecutive failures.
    pub async fn consecutive_failures(&self) -> u32 {
        self.state.lock().await.consecutive_failures
    }
}

impl<C> CircuitBreakerLayer<C> {
    /// Check if a call is currently allowed.
    ///
    /// Returns `Ok(())` if allowed, `Err(message)` if the circuit is open.
    pub async fn check_allowed(&self) -> Result<(), String> {
        let mut state = self.state.lock().await;
        match state.state {
            CircuitState::Closed | CircuitState::HalfOpen => Ok(()),
            CircuitState::Open => {
                if let Some(opened_at) = state.opened_at {
                    if opened_at.elapsed() >= self.config.recovery_timeout {
                        state.state = CircuitState::HalfOpen;
                        state.half_open_successes = 0;
                        tracing::info!(
                            label = %self.label,
                            "Circuit breaker: Open -> HalfOpen, allowing probe"
                        );
                        Ok(())
                    } else {
                        let remaining = self
                            .config
                            .recovery_timeout
                            .checked_sub(opened_at.elapsed())
                            .unwrap_or(Duration::ZERO);
                        Err(format!(
                            "Circuit breaker open for '{}' ({} consecutive failures, \
                             recovery in {:.0}s)",
                            self.label,
                            state.consecutive_failures,
                            remaining.as_secs_f64()
                        ))
                    }
                } else {
                    state.state = CircuitState::Closed;
                    Ok(())
                }
            }
        }
    }

    /// Record a successful call.
    pub async fn record_success(&self) {
        let mut state = self.state.lock().await;
        match state.state {
            CircuitState::Closed => {
                state.consecutive_failures = 0;
            }
            CircuitState::HalfOpen => {
                state.half_open_successes += 1;
                if state.half_open_successes >= self.config.half_open_successes_needed {
                    state.state = CircuitState::Closed;
                    state.consecutive_failures = 0;
                    state.opened_at = None;
                    tracing::info!(
                        label = %self.label,
                        "Circuit breaker: HalfOpen -> Closed (recovered)"
                    );
                }
            }
            CircuitState::Open => {
                state.state = CircuitState::Closed;
                state.consecutive_failures = 0;
                state.opened_at = None;
            }
        }
    }

    /// Record a failed call. Only transient errors count toward the threshold.
    pub async fn record_failure<E>(&self, err: &E)
    where
        C: ErrorClassifier<E>,
    {
        if !self.classifier.is_transient(err) {
            return;
        }

        let mut state = self.state.lock().await;
        match state.state {
            CircuitState::Closed => {
                state.consecutive_failures += 1;
                if state.consecutive_failures >= self.config.failure_threshold {
                    state.state = CircuitState::Open;
                    state.opened_at = Some(Instant::now());
                    tracing::warn!(
                        label = %self.label,
                        failures = state.consecutive_failures,
                        "Circuit breaker: Closed -> Open"
                    );
                }
            }
            CircuitState::HalfOpen => {
                state.state = CircuitState::Open;
                state.opened_at = Some(Instant::now());
                state.half_open_successes = 0;
                tracing::warn!(
                    label = %self.label,
                    "Circuit breaker: HalfOpen -> Open (probe failed)"
                );
            }
            CircuitState::Open => {
                // Already open, nothing to do
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum TestError {
        #[error("transient")]
        Transient,
        #[error("permanent")]
        Permanent,
    }

    struct TestClassifier;
    impl ErrorClassifier<TestError> for TestClassifier {
        fn is_retryable(&self, err: &TestError) -> bool {
            matches!(err, TestError::Transient)
        }
        fn is_transient(&self, err: &TestError) -> bool {
            matches!(err, TestError::Transient)
        }
    }

    fn make_breaker(threshold: u32) -> CircuitBreakerLayer<TestClassifier> {
        CircuitBreakerLayer::new(
            CircuitBreakerConfig {
                failure_threshold: threshold,
                recovery_timeout: Duration::from_millis(100),
                half_open_successes_needed: 2,
            },
            TestClassifier,
            "test",
        )
    }

    #[tokio::test]
    async fn test_closed_allows_calls() {
        let cb = make_breaker(3);
        assert!(cb.check_allowed().await.is_ok()); // safety: test-only
        assert_eq!(cb.circuit_state().await, CircuitState::Closed); // safety: test-only
    }

    #[tokio::test]
    async fn test_opens_after_threshold() {
        let cb = make_breaker(3);
        for _ in 0..3 {
            cb.record_failure(&TestError::Transient).await;
        }
        assert_eq!(cb.circuit_state().await, CircuitState::Open); // safety: test-only
        assert!(cb.check_allowed().await.is_err()); // safety: test-only
    }

    #[tokio::test]
    async fn test_permanent_errors_dont_trip() {
        let cb = make_breaker(3);
        for _ in 0..10 {
            cb.record_failure(&TestError::Permanent).await;
        }
        assert_eq!(cb.circuit_state().await, CircuitState::Closed); // safety: test-only
    }

    #[tokio::test]
    async fn test_success_resets_count() {
        let cb = make_breaker(3);
        cb.record_failure(&TestError::Transient).await;
        cb.record_failure(&TestError::Transient).await;
        cb.record_success().await;
        assert_eq!(cb.consecutive_failures().await, 0); // safety: test-only
        // Should still be closed since we reset
        cb.record_failure(&TestError::Transient).await;
        cb.record_failure(&TestError::Transient).await;
        assert_eq!(cb.circuit_state().await, CircuitState::Closed); // safety: test-only
    }

    #[tokio::test]
    async fn test_recovery_to_half_open() {
        let cb = make_breaker(1);
        cb.record_failure(&TestError::Transient).await;
        assert_eq!(cb.circuit_state().await, CircuitState::Open); // safety: test-only

        // Wait for recovery timeout
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Should transition to HalfOpen
        assert!(cb.check_allowed().await.is_ok()); // safety: test-only
        assert_eq!(cb.circuit_state().await, CircuitState::HalfOpen); // safety: test-only
    }

    #[tokio::test]
    async fn test_half_open_closes_on_successes() {
        let cb = make_breaker(1);
        cb.record_failure(&TestError::Transient).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let _ = cb.check_allowed().await; // transition to HalfOpen

        cb.record_success().await;
        assert_eq!(cb.circuit_state().await, CircuitState::HalfOpen); // needs 2 // safety: test-only
        cb.record_success().await;
        assert_eq!(cb.circuit_state().await, CircuitState::Closed); // safety: test-only
    }

    #[tokio::test]
    async fn test_half_open_reopens_on_failure() {
        let cb = make_breaker(1);
        cb.record_failure(&TestError::Transient).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let _ = cb.check_allowed().await; // HalfOpen

        cb.record_failure(&TestError::Transient).await;
        assert_eq!(cb.circuit_state().await, CircuitState::Open); // safety: test-only
    }
}
