//! Boundary chaos tests — exercise failure modes at module seams.
//!
//! These tests verify that the architectural hardening (domain event decoupling,
//! generic resilience layers, state bus) works correctly under failure conditions.
//!
//! Organized by boundary, not by module:
//! - Resilience layers (retry, circuit breaker, health tracker)
//! - State bus propagation
//! - Domain event type compatibility

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use ironclaw::events::DomainEvent;
use ironclaw::resilience::circuit_breaker::{
    CircuitBreakerConfig, CircuitBreakerLayer, CircuitState,
};
use ironclaw::resilience::classifier::ErrorClassifier;
use ironclaw::resilience::health::HealthTracker;
use ironclaw::resilience::retry::{RetryConfig, RetryLayer};
use ironclaw::state_bus::{StateBus, StateChange};

// ── Test error type ──────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("transient failure")]
    Transient,
    #[error("permanent failure")]
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

// ── Resilience: Retry layer ──────────────────────────────────────────

#[tokio::test]
async fn retry_layer_recovers_after_transient_failures() {
    let call_count = Arc::new(AtomicU32::new(0));
    let cc = call_count.clone();

    let layer = RetryLayer::new(RetryConfig { max_retries: 3 }, TestClassifier);
    let result: Result<&str, TestError> = layer
        .execute(
            // safety: test-only
            || {
                let c = cc.clone();
                async move {
                    let n = c.fetch_add(1, Ordering::Relaxed);
                    if n < 2 {
                        Err(TestError::Transient)
                    } else {
                        Ok("recovered")
                    }
                }
            },
            "test",
        )
        .await;

    assert_eq!(result.unwrap(), "recovered"); // safety: test-only
    assert_eq!(call_count.load(Ordering::Relaxed), 3); // 2 failures + 1 success // safety: test-only
}

#[tokio::test]
async fn retry_layer_stops_on_permanent_error() {
    let call_count = Arc::new(AtomicU32::new(0));
    let cc = call_count.clone();

    let layer = RetryLayer::new(RetryConfig { max_retries: 5 }, TestClassifier);
    let result: Result<(), TestError> = layer
        .execute(
            // safety: test-only
            || {
                let c = cc.clone();
                async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    Err(TestError::Permanent)
                }
            },
            "test",
        )
        .await;

    assert!(result.is_err()); // safety: test-only
    assert_eq!(call_count.load(Ordering::Relaxed), 1); // No retries for permanent // safety: test-only
}

// ── Resilience: Circuit breaker ──────────────────────────────────────

#[tokio::test]
async fn circuit_breaker_opens_after_threshold() {
    let cb = CircuitBreakerLayer::new(
        CircuitBreakerConfig {
            failure_threshold: 3,
            recovery_timeout: Duration::from_millis(100),
            half_open_successes_needed: 1,
        },
        TestClassifier,
        "test-endpoint",
    );

    // Record failures up to threshold
    for _ in 0..3 {
        cb.record_failure(&TestError::Transient).await;
    }

    assert_eq!(cb.circuit_state().await, CircuitState::Open); // safety: test-only
    assert!(cb.check_allowed().await.is_err()); // safety: test-only
}

#[tokio::test]
async fn circuit_breaker_recovers_via_half_open() {
    let cb = CircuitBreakerLayer::new(
        CircuitBreakerConfig {
            failure_threshold: 2,
            recovery_timeout: Duration::from_millis(50),
            half_open_successes_needed: 1,
        },
        TestClassifier,
        "test-recovery",
    );

    // Trip the circuit
    cb.record_failure(&TestError::Transient).await;
    cb.record_failure(&TestError::Transient).await;
    assert_eq!(cb.circuit_state().await, CircuitState::Open); // safety: test-only

    // Wait for recovery timeout
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Should transition to HalfOpen
    assert!(cb.check_allowed().await.is_ok()); // safety: test-only
    assert_eq!(cb.circuit_state().await, CircuitState::HalfOpen); // safety: test-only

    // Success should close the circuit
    cb.record_success().await;
    assert_eq!(cb.circuit_state().await, CircuitState::Closed); // safety: test-only
}

#[tokio::test]
async fn circuit_breaker_ignores_permanent_errors() {
    let cb = CircuitBreakerLayer::new(
        CircuitBreakerConfig {
            failure_threshold: 2,
            recovery_timeout: Duration::from_secs(30),
            half_open_successes_needed: 1,
        },
        TestClassifier,
        "test-perm",
    );

    // Permanent errors should never trip the breaker
    for _ in 0..100 {
        cb.record_failure(&TestError::Permanent).await;
    }
    assert_eq!(cb.circuit_state().await, CircuitState::Closed); // safety: test-only
}

// ── Resilience: Health tracker ───────────────────────────────────────

#[test]
fn health_tracker_marks_unhealthy_after_threshold() {
    let tracker = HealthTracker::new(3);
    assert!(tracker.is_healthy("mcp-server-1")); // safety: test-only

    tracker.record_failure("mcp-server-1");
    tracker.record_failure("mcp-server-1");
    assert!(tracker.is_healthy("mcp-server-1")); // Not yet // safety: test-only

    tracker.record_failure("mcp-server-1");
    assert!(!tracker.is_healthy("mcp-server-1")); // Now unhealthy // safety: test-only
}

#[test]
fn health_tracker_recovers_on_success() {
    let tracker = HealthTracker::new(2);
    tracker.record_failure("ep1");
    tracker.record_failure("ep1");
    assert!(!tracker.is_healthy("ep1")); // safety: test-only

    tracker.record_success("ep1");
    assert!(tracker.is_healthy("ep1")); // safety: test-only
}

#[test]
fn health_tracker_isolates_endpoints() {
    let tracker = HealthTracker::new(2);

    // Fail ep1
    tracker.record_failure("ep1");
    tracker.record_failure("ep1");
    assert!(!tracker.is_healthy("ep1")); // safety: test-only

    // ep2 should be unaffected
    assert!(tracker.is_healthy("ep2")); // safety: test-only
}

// ── State bus ────────────────────────────────────────────────────────

#[tokio::test]
async fn state_bus_delivers_to_all_subscribers() {
    let bus = StateBus::new();
    let mut rx1 = bus.subscribe();
    let mut rx2 = bus.subscribe();

    let id = uuid::Uuid::new_v4();
    bus.publish(StateChange::RoutineUpdated { routine_id: id });

    let e1 = rx1.recv().await.unwrap(); // safety: test-only
    let e2 = rx2.recv().await.unwrap(); // safety: test-only
    assert!(matches!(e1, StateChange::RoutineUpdated { routine_id } if routine_id == id)); // safety: test-only
    assert!(matches!(e2, StateChange::RoutineUpdated { routine_id } if routine_id == id)); // safety: test-only
}

#[tokio::test]
async fn state_bus_no_subscriber_is_harmless() {
    let bus = StateBus::new();
    // Publishing with no subscribers should not panic
    bus.publish(StateChange::ConfigReloaded);
    bus.publish(StateChange::ToolRegistryChanged);
    bus.publish(StateChange::SecretRotated {
        key_name: "api_key".to_string(),
    });
}

#[tokio::test]
async fn state_bus_subscriber_receives_only_after_subscribe() {
    let bus = StateBus::new();

    // Publish before subscribing
    bus.publish(StateChange::ConfigReloaded);

    // Subscribe after
    let mut rx = bus.subscribe();

    // Publish after subscribing
    bus.publish(StateChange::ToolRegistryChanged);

    let event = rx.recv().await.unwrap(); // safety: test-only
    assert!(matches!(event, StateChange::ToolRegistryChanged)); // safety: test-only
}

// ── Domain event compatibility ───────────────────────────────────────

#[test]
fn domain_event_serializes_as_sse_wire_format() {
    let event = DomainEvent::Response {
        content: "Hello!".to_string(),
        thread_id: "t1".to_string(),
    };
    let json = serde_json::to_string(&event).unwrap(); // safety: test-only
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap(); // safety: test-only

    assert_eq!(parsed["type"], "response"); // safety: test-only
    assert_eq!(parsed["content"], "Hello!"); // safety: test-only
    assert_eq!(parsed["thread_id"], "t1"); // safety: test-only
}

#[test]
fn domain_event_all_variants_serialize() {
    // Verify all variants can be serialized without panicking
    let variants: Vec<DomainEvent> = vec![
        DomainEvent::Response {
            content: "ok".into(),
            thread_id: "t".into(),
        },
        DomainEvent::Thinking {
            message: "...".into(),
            thread_id: None,
        },
        DomainEvent::ToolStarted {
            name: "shell".into(),
            thread_id: None,
        },
        DomainEvent::ToolCompleted {
            name: "shell".into(),
            success: true,
            error: None,
            parameters: None,
            thread_id: None,
        },
        DomainEvent::Heartbeat,
        DomainEvent::JobMessage {
            job_id: "j1".into(),
            role: "assistant".into(),
            content: "msg".into(),
        },
        DomainEvent::JobResult {
            job_id: "j1".into(),
            status: "completed".into(),
            session_id: None,
        },
        DomainEvent::Suggestions {
            suggestions: vec!["a".into(), "b".into()],
            thread_id: Some("t1".into()),
        },
    ];

    for variant in &variants {
        let json = serde_json::to_string(variant).unwrap(); // safety: test-only
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap(); // safety: test-only
        assert!( // safety: test-only
            // safety: test-only
            parsed.get("type").is_some(),
            "missing 'type' field in {:?}",
            variant
        );
    }
}

#[test]
fn domain_event_broadcast_channel_works() {
    // Verify DomainEvent can be used with tokio broadcast (Clone required)
    let (tx, mut rx) = tokio::sync::broadcast::channel::<DomainEvent>(16);
    tx.send(DomainEvent::Heartbeat).unwrap(); // safety: test-only
    let received = rx.try_recv().unwrap(); // safety: test-only
    assert!(matches!(received, DomainEvent::Heartbeat)); // safety: test-only
}

// ── Cross-boundary: Retry + Circuit Breaker composition ──────────────

#[tokio::test]
async fn retry_and_circuit_breaker_compose() {
    let cb = Arc::new(CircuitBreakerLayer::new(
        CircuitBreakerConfig {
            failure_threshold: 5,
            recovery_timeout: Duration::from_secs(30),
            half_open_successes_needed: 1,
        },
        TestClassifier,
        "composed",
    ));
    let retry = RetryLayer::new(RetryConfig { max_retries: 2 }, TestClassifier);

    let call_count = Arc::new(AtomicU32::new(0));
    let cc = call_count.clone();
    let cb_clone = cb.clone();

    // Simulate an operation that fails then succeeds, tracked by circuit breaker
    let result: Result<&str, TestError> = retry
        .execute(
            // safety: test-only
            || {
                let c = cc.clone();
                let cb = cb_clone.clone();
                async move {
                    let n = c.fetch_add(1, Ordering::Relaxed);
                    if n == 0 {
                        cb.record_failure(&TestError::Transient).await;
                        Err(TestError::Transient)
                    } else {
                        cb.record_success().await;
                        Ok("ok")
                    }
                }
            },
            "composed",
        )
        .await;

    assert_eq!(result.unwrap(), "ok"); // safety: test-only
    assert_eq!(cb.circuit_state().await, CircuitState::Closed); // safety: test-only
    assert_eq!(cb.consecutive_failures().await, 0); // safety: test-only
}
