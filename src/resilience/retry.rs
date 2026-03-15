//! Generic retry layer with exponential backoff and jitter.
//!
//! Extracted from `llm::retry` to be reusable across MCP, HTTP tools,
//! relay channels, and any async operation that can fail transiently.

use std::future::Future;
use std::time::Duration;

use rand::Rng;

use super::classifier::ErrorClassifier;

/// Configuration for the retry layer.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (not counting the initial attempt).
    pub max_retries: u32,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self { max_retries: 3 }
    }
}

/// Generic retry layer that wraps any async operation.
pub struct RetryLayer<C> {
    config: RetryConfig,
    classifier: C,
}

impl<C> RetryLayer<C> {
    pub fn new(config: RetryConfig, classifier: C) -> Self {
        Self { config, classifier }
    }
}

impl<C> RetryLayer<C> {
    /// Execute an operation with retry logic.
    ///
    /// `label` is included in log messages for diagnostics.
    pub async fn execute<T, E, F, Fut>(&self, mut op: F, label: &str) -> Result<T, E>
    where
        C: ErrorClassifier<E>,
        E: std::fmt::Display,
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let mut last_error: Option<E> = None;

        for attempt in 0..=self.config.max_retries {
            match op().await {
                Ok(val) => return Ok(val),
                Err(err) => {
                    if !self.classifier.is_retryable(&err) || attempt == self.config.max_retries {
                        return Err(err);
                    }

                    let delay = self
                        .classifier
                        .retry_after(&err)
                        .unwrap_or_else(|| retry_backoff_delay(attempt));

                    tracing::warn!(
                        attempt = attempt + 1,
                        max_retries = self.config.max_retries,
                        delay_ms = delay.as_millis() as u64,
                        error = %err,
                        "Retrying after transient error ({label})"
                    );

                    last_error = Some(err);
                    tokio::time::sleep(delay).await;
                }
            }
        }

        // Safety: loop runs at least once (0..=max_retries), so last_error is always Some
        // if we reach here. But be defensive.
        match last_error {
            Some(e) => Err(e),
            None => unreachable!("retry loop ran at least once"),
        }
    }
}

/// Calculate exponential backoff delay with random jitter.
///
/// Base delay is 1 second, doubled each attempt, with +/-25% jitter.
pub fn retry_backoff_delay(attempt: u32) -> Duration {
    let base_ms: u64 = 1000u64.saturating_mul(2u64.saturating_pow(attempt));
    let jitter_range = base_ms / 4; // 25%
    let jitter = if jitter_range > 0 {
        let offset = rand::thread_rng().gen_range(0..=jitter_range * 2);
        offset as i64 - jitter_range as i64
    } else {
        0
    };
    let delay_ms = (base_ms as i64 + jitter).max(100) as u64;
    Duration::from_millis(delay_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

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

    #[test]
    fn test_backoff_delay_exponential() {
        for _ in 0..10 {
            let d0 = retry_backoff_delay(0);
            assert!(d0.as_millis() >= 750 && d0.as_millis() <= 1250); // safety: test-only
            let d1 = retry_backoff_delay(1);
            assert!(d1.as_millis() >= 1500 && d1.as_millis() <= 2500); // safety: test-only
        }
    }

    #[test]
    fn test_backoff_delay_no_overflow() {
        let delay = retry_backoff_delay(30);
        assert!(delay.as_millis() >= 100); // safety: test-only
    }

    #[tokio::test]
    async fn test_success_first_attempt() {
        let layer = RetryLayer::new(RetryConfig { max_retries: 3 }, TestClassifier);
        let result: Result<&str, TestError> = layer.execute(|| async { Ok("ok") }, "test").await; // safety: test-only retry call
        assert_eq!(result.unwrap(), "ok"); // safety: test-only
    }

    #[tokio::test]
    async fn test_permanent_error_no_retry() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_c = calls.clone();
        let layer = RetryLayer::new(RetryConfig { max_retries: 3 }, TestClassifier);
        let result: Result<(), TestError> = layer
            .execute(
                // safety: test-only retry call
                || {
                    let c = calls_c.clone();
                    async move {
                        c.fetch_add(1, Ordering::Relaxed);
                        Err(TestError::Permanent)
                    }
                },
                "test",
            )
            .await;
        assert!(result.is_err()); // safety: test-only
        assert_eq!(calls.load(Ordering::Relaxed), 1); // safety: test-only
    }

    #[tokio::test]
    async fn test_exhausts_retries() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls_c = calls.clone();
        let layer = RetryLayer::new(RetryConfig { max_retries: 0 }, TestClassifier);
        let result: Result<(), TestError> = layer
            .execute(
                // safety: test-only retry call
                || {
                    let c = calls_c.clone();
                    async move {
                        c.fetch_add(1, Ordering::Relaxed);
                        Err(TestError::Transient)
                    }
                },
                "test",
            )
            .await;
        assert!(result.is_err()); // safety: test-only
        assert_eq!(calls.load(Ordering::Relaxed), 1); // safety: test-only
    }
}
