//! Per-endpoint health tracking.
//!
//! Provides atomic, lock-free health counters for external service endpoints.
//! Used by the state bus to publish `EndpointHealthChanged` events.

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Health state for a single endpoint.
pub struct EndpointHealth {
    /// Number of consecutive failures.
    pub consecutive_failures: AtomicU32,
    /// Unix timestamp (seconds) of last successful call.
    pub last_success: AtomicU64,
    /// 0 = healthy, 1 = unhealthy.
    pub unhealthy: AtomicU32,
}

impl EndpointHealth {
    pub fn new() -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            last_success: AtomicU64::new(0),
            unhealthy: AtomicU32::new(0),
        }
    }

    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.last_success.store(now, Ordering::Relaxed);
        self.unhealthy.store(0, Ordering::Relaxed);
    }

    /// Record a failure. Returns true if this failure triggered the unhealthy threshold.
    pub fn record_failure(&self, threshold: u32) -> bool {
        let prev = self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        let new_count = prev + 1;
        if new_count >= threshold && self.unhealthy.swap(1, Ordering::Relaxed) == 0 {
            return true; // Just became unhealthy
        }
        false
    }

    pub fn is_healthy(&self) -> bool {
        self.unhealthy.load(Ordering::Relaxed) == 0
    }
}

impl Default for EndpointHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// Tracks health of multiple named endpoints.
pub struct HealthTracker {
    endpoints: RwLock<HashMap<String, EndpointHealth>>,
    failure_threshold: u32,
}

impl HealthTracker {
    pub fn new(failure_threshold: u32) -> Self {
        Self {
            endpoints: RwLock::new(HashMap::new()),
            failure_threshold,
        }
    }

    pub fn record_success(&self, name: &str) {
        let endpoints = self.endpoints.read().unwrap_or_else(|e| e.into_inner());
        if let Some(health) = endpoints.get(name) {
            health.record_success();
        } else {
            drop(endpoints);
            let mut endpoints = self.endpoints.write().unwrap_or_else(|e| e.into_inner());
            endpoints
                .entry(name.to_string())
                .or_default()
                .record_success();
        }
    }

    /// Record a failure. Returns true if this made the endpoint unhealthy.
    pub fn record_failure(&self, name: &str) -> bool {
        let endpoints = self.endpoints.read().unwrap_or_else(|e| e.into_inner());
        if let Some(health) = endpoints.get(name) {
            health.record_failure(self.failure_threshold)
        } else {
            drop(endpoints);
            let mut endpoints = self.endpoints.write().unwrap_or_else(|e| e.into_inner());
            let health = endpoints.entry(name.to_string()).or_default();
            health.record_failure(self.failure_threshold)
        }
    }

    pub fn is_healthy(&self, name: &str) -> bool {
        let endpoints = self.endpoints.read().unwrap_or_else(|e| e.into_inner());
        endpoints.get(name).map(|h| h.is_healthy()).unwrap_or(true) // Unknown endpoints are assumed healthy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_endpoint_health_starts_healthy() {
        let h = EndpointHealth::new();
        assert!(h.is_healthy()); // safety: test-only
    }

    #[test]
    fn test_endpoint_health_becomes_unhealthy() {
        let h = EndpointHealth::new();
        for i in 0..4 {
            assert!( // safety: test-only
                // safety: test-only
                !h.record_failure(5),
                "should not be unhealthy at failure {}",
                i + 1
            );
        }
        assert!(h.record_failure(5), "should become unhealthy at failure 5"); // safety: test-only
        assert!(!h.is_healthy()); // safety: test-only
    }

    #[test]
    fn test_endpoint_health_recovers() {
        let h = EndpointHealth::new();
        for _ in 0..5 {
            h.record_failure(5);
        }
        assert!(!h.is_healthy()); // safety: test-only
        h.record_success();
        assert!(h.is_healthy()); // safety: test-only
    }

    #[test]
    fn test_tracker_unknown_is_healthy() {
        let t = HealthTracker::new(3);
        assert!(t.is_healthy("unknown")); // safety: test-only
    }

    #[test]
    fn test_tracker_tracks_failures() {
        let t = HealthTracker::new(2);
        assert!(!t.record_failure("ep1")); // safety: test-only
        assert!(t.record_failure("ep1")); // safety: test-only
        assert!(!t.is_healthy("ep1")); // safety: test-only
    }

    #[test]
    fn test_tracker_recovery() {
        let t = HealthTracker::new(2);
        t.record_failure("ep1");
        t.record_failure("ep1");
        t.record_success("ep1");
        assert!(t.is_healthy("ep1")); // safety: test-only
    }
}
