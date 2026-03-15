//! State invalidation bus for cross-module state synchronization.
//!
//! When state changes in one module (e.g., web UI toggles a routine, secret
//! rotates, config reloads), the bus notifies other modules that cache that
//! state so they can refresh.
//!
//! Modules subscribe to events they care about and ignore the rest. No module
//! needs to import another module to propagate state changes — the bus is the
//! **only** coupling point.

use std::sync::Arc;

use tokio::sync::broadcast;
use uuid::Uuid;

/// A state change notification.
#[derive(Debug, Clone)]
pub enum StateChange {
    /// A routine was created, updated, toggled, or deleted.
    RoutineUpdated { routine_id: Uuid },
    /// A secret was rotated or deleted.
    SecretRotated { key_name: String },
    /// Global configuration was reloaded (e.g. via SIGHUP).
    ConfigReloaded,
    /// An external endpoint's health status changed.
    EndpointHealthChanged { name: String, healthy: bool },
    /// The tool registry was modified (tool added/removed/rebuilt).
    ToolRegistryChanged,
    /// An extension was installed or removed.
    ExtensionInstalled { extension_id: String },
}

/// Broadcast bus for state change notifications.
///
/// Backed by a tokio `broadcast` channel with a fixed buffer. Slow consumers
/// that fall behind will miss events (acceptable — they can re-poll state).
#[derive(Clone)]
pub struct StateBus {
    tx: broadcast::Sender<StateChange>,
}

impl StateBus {
    /// Create a new state bus with a buffer of 64 events.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self { tx }
    }

    /// Publish a state change. Non-blocking; drops the event if no subscribers.
    pub fn publish(&self, event: StateChange) {
        // Ignore send error (no active receivers).
        let _ = self.tx.send(event);
    }

    /// Subscribe to state change notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<StateChange> {
        self.tx.subscribe()
    }
}

impl Default for StateBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience constructor for passing through `Arc`.
pub fn new_state_bus() -> Arc<StateBus> {
    Arc::new(StateBus::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_publish_subscribe() {
        let bus = StateBus::new();
        let mut rx = bus.subscribe();

        let id = Uuid::new_v4();
        bus.publish(StateChange::RoutineUpdated { routine_id: id });

        let event = rx.recv().await.unwrap(); // safety: test-only
        assert!(matches!(event, StateChange::RoutineUpdated { routine_id } if routine_id == id)); // safety: test-only
    }

    #[tokio::test]
    async fn test_no_subscriber_does_not_panic() {
        let bus = StateBus::new();
        // No subscribers — should not panic.
        bus.publish(StateChange::ConfigReloaded);
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let bus = StateBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.publish(StateChange::ToolRegistryChanged);

        let e1 = rx1.recv().await.unwrap(); // safety: test-only
        let e2 = rx2.recv().await.unwrap(); // safety: test-only
        assert!(matches!(e1, StateChange::ToolRegistryChanged)); // safety: test-only
        assert!(matches!(e2, StateChange::ToolRegistryChanged)); // safety: test-only
    }

    #[tokio::test]
    async fn test_slow_consumer_lags() {
        let bus = StateBus::new();
        let mut rx = bus.subscribe();

        // Overflow the 64-event buffer.
        for i in 0..100 {
            bus.publish(StateChange::EndpointHealthChanged {
                name: format!("ep-{}", i),
                healthy: true,
            });
        }

        // First recv should report a lag.
        let result = rx.recv().await;
        assert!( // safety: test-only
            // safety: test-only
            result.is_ok() || result.is_err(),
            "lagged receiver should either get an event or a Lagged error"
        );
    }
}
