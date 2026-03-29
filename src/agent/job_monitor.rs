//! Background job monitor that forwards Claude Code output to the main agent loop.
//!
//! When the main agent kicks off a sandbox job (especially Claude Code), this
//! monitor subscribes to the broadcast event channel and injects relevant
//! assistant messages back into the channel manager's stream. This lets the
//! main agent see what the sub-agent is producing and surface it to the user.
//!
//! ```text
//!   Container ──NDJSON──► Orchestrator ──broadcast──► JobMonitor
//!                                                        │
//!                                                  inject_tx (mpsc)
//!                                                        │
//!                                                        ▼
//!                                                   Agent Loop
//! ```

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::channels::IncomingMessage;
use crate::context::{ContextManager, JobState};
use ironclaw_common::AppEvent;

/// Route context for forwarding job monitor events back to the user's channel.
#[derive(Debug, Clone)]
pub struct JobMonitorRoute {
    pub channel: String,
    pub user_id: String,
    pub thread_id: Option<String>,
    pub response_metadata: serde_json::Value,
}

/// Spawn a background task that watches for events from a specific job and
/// injects assistant messages into the agent loop.
///
/// The monitor forwards:
/// - `AppEvent::JobMessage` (assistant role): injected as incoming messages so
///   the main agent can read and relay to the user.
/// - `AppEvent::JobResult`: injected as a completion notice, then the task exits.
///
/// Tool use/result and status events are intentionally skipped (too noisy for
/// the main agent's context window).
pub fn spawn_job_monitor(
    job_id: Uuid,
    event_rx: broadcast::Receiver<(Uuid, String, AppEvent)>,
    inject_tx: mpsc::Sender<IncomingMessage>,
    route: JobMonitorRoute,
) -> JoinHandle<()> {
    spawn_job_monitor_with_context(job_id, event_rx, inject_tx, route, None)
}

/// Like `spawn_job_monitor`, but also transitions the job's in-memory state
/// when it receives a `JobResult` event. This ensures fire-and-forget sandbox
/// jobs don't stay `InProgress` forever in the `ContextManager`.
pub fn spawn_job_monitor_with_context(
    job_id: Uuid,
    mut event_rx: broadcast::Receiver<(Uuid, String, AppEvent)>,
    inject_tx: mpsc::Sender<IncomingMessage>,
    route: JobMonitorRoute,
    context_manager: Option<Arc<ContextManager>>,
) -> JoinHandle<()> {
    let short_id = job_id.to_string()[..8].to_string();

    tokio::spawn(async move {
        tracing::info!(
            job_id = %short_id,
            channel = %route.channel,
            user_id = %route.user_id,
            thread_id = ?route.thread_id,
            has_response_metadata = !route.response_metadata.is_null(),
            "Job monitor started successfully"
        );

        loop {
            match event_rx.recv().await {
                Ok((ev_job_id, _user_id, event)) => {
                    if ev_job_id != job_id {
                        continue;
                    }

                    match event {
                        AppEvent::JobMessage { role, content, .. } if role == "assistant" => {
                            let mut msg = IncomingMessage::new(
                                route.channel.clone(),
                                route.user_id.clone(),
                                format!("[Job {}] Claude Code: {}", short_id, content),
                            )
                            .into_internal();
                            if let Some(ref thread_id) = route.thread_id {
                                msg = msg.with_thread(thread_id.clone());
                            }
                            if !route.response_metadata.is_null() {
                                msg = msg.with_metadata(route.response_metadata.clone());
                            }
                            match inject_tx.send(msg).await {
                                Ok(()) => {
                                    tracing::debug!(
                                        job_id = %short_id,
                                        channel = %route.channel,
                                        thread_id = ?route.thread_id,
                                        "Forwarded job assistant message to agent loop"
                                    );
                                }
                                Err(_) => {
                                    tracing::debug!(
                                        job_id = %short_id,
                                        channel = %route.channel,
                                        thread_id = ?route.thread_id,
                                        "Inject channel closed, stopping monitor"
                                    );
                                    break;
                                }
                            }
                        }
                        AppEvent::JobResult {
                            status,
                            fallback_deliverable,
                            ..
                        } => {
                            tracing::info!(
                                job_id = %short_id,
                                channel = %route.channel,
                                thread_id = ?route.thread_id,
                                status = %status,
                                has_fallback_deliverable = fallback_deliverable.is_some(),
                                "Job monitor received completion event"
                            );
                            // Transition in-memory state so the job frees its
                            // max_jobs slot and query tools show the final state.
                            if let Some(ref cm) = context_manager {
                                let target = if status == "completed" {
                                    JobState::Completed
                                } else {
                                    JobState::Failed
                                };
                                let reason = if status != "completed" {
                                    Some(format!("Container finished: {}", status))
                                } else {
                                    None
                                };
                                let _ = cm
                                    .update_context(job_id, |ctx| {
                                        let _ = ctx.transition_to(target, reason);
                                    })
                                    .await;
                            }

                            let completion_message = fallback_deliverable
                                .and_then(json_value_to_notification_text)
                                .filter(|text| !text.is_empty())
                                .map(|text| format!("[Job {}] {}", short_id, text))
                                .unwrap_or_else(|| {
                                    format!(
                                        "[Job {}] Container finished (status: {})",
                                        short_id, status
                                    )
                                });

                            let mut msg = IncomingMessage::new(
                                route.channel.clone(),
                                route.user_id.clone(),
                                completion_message,
                            )
                            .into_internal();
                            if let Some(ref thread_id) = route.thread_id {
                                msg = msg.with_thread(thread_id.clone());
                            }
                            if !route.response_metadata.is_null() {
                                msg = msg.with_metadata(route.response_metadata.clone());
                            }
                            match inject_tx.send(msg).await {
                                Ok(()) => {
                                    tracing::info!(
                                        job_id = %short_id,
                                        channel = %route.channel,
                                        thread_id = ?route.thread_id,
                                        status = %status,
                                        "Forwarded job completion message to agent loop"
                                    );
                                }
                                Err(_) => {
                                    tracing::warn!(
                                        job_id = %short_id,
                                        channel = %route.channel,
                                        thread_id = ?route.thread_id,
                                        status = %status,
                                        "Failed to forward job completion message because inject channel is closed"
                                    );
                                }
                            }
                            tracing::debug!(
                                job_id = %short_id,
                                status = %status,
                                "Job monitor exiting (job finished)"
                            );
                            break;
                        }
                        _ => {
                            // Skip tool_use, tool_result, status events
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        job_id = %short_id,
                        skipped = n,
                        "Job monitor lagged, some events were dropped"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::debug!(
                        job_id = %short_id,
                        "Broadcast channel closed, stopping monitor"
                    );
                    break;
                }
            }
        }
    })
}

fn json_value_to_notification_text(value: serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        other => {
            let text = other.to_string();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
    }
}

/// Lightweight watcher that only transitions ContextManager state on job
/// completion. Used when monitor routing metadata is absent (no channel to
/// inject messages into) but we still need to free the `max_jobs` slot.
pub fn spawn_completion_watcher(
    job_id: Uuid,
    mut event_rx: broadcast::Receiver<(Uuid, String, AppEvent)>,
    context_manager: Arc<ContextManager>,
) -> JoinHandle<()> {
    let short_id = job_id.to_string()[..8].to_string();

    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok((ev_job_id, _user_id, AppEvent::JobResult { status, .. }))
                    if ev_job_id == job_id =>
                {
                    let target = if status == "completed" {
                        JobState::Completed
                    } else {
                        JobState::Failed
                    };
                    let reason = if status != "completed" {
                        Some(format!("Container finished: {}", status))
                    } else {
                        None
                    };
                    let _ = context_manager
                        .update_context(job_id, |ctx| {
                            let _ = ctx.transition_to(target, reason);
                        })
                        .await;
                    tracing::debug!(
                        job_id = %short_id,
                        status = %status,
                        "Completion watcher exiting (job finished)"
                    );
                    break;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        job_id = %short_id,
                        skipped = n,
                        "Completion watcher lagged"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::debug!(
                        job_id = %short_id,
                        "Broadcast channel closed, stopping completion watcher"
                    );
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_route() -> JobMonitorRoute {
        JobMonitorRoute {
            channel: "cli".to_string(),
            user_id: "user-1".to_string(),
            thread_id: Some("thread-1".to_string()),
            response_metadata: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn test_monitor_forwards_assistant_messages() {
        let (event_tx, _) = broadcast::channel::<(Uuid, String, AppEvent)>(16);
        let (inject_tx, mut inject_rx) = mpsc::channel::<IncomingMessage>(16);

        let job_id = Uuid::new_v4();
        let _handle = spawn_job_monitor(job_id, event_tx.subscribe(), inject_tx, test_route());

        // Send an assistant message
        event_tx
            .send((
                job_id,
                "test-user".to_string(),
                AppEvent::JobMessage {
                    job_id: job_id.to_string(),
                    role: "assistant".to_string(),
                    content: "I found a bug".to_string(),
                },
            ))
            .unwrap();

        let msg = tokio::time::timeout(std::time::Duration::from_secs(1), inject_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(msg.channel, "cli");
        assert_eq!(msg.user_id, "user-1");
        assert_eq!(msg.thread_id, Some("thread-1".to_string()));
        assert!(msg.content.contains("I found a bug"));
        assert!(msg.is_internal, "monitor messages must be marked internal");
    }

    #[tokio::test]
    async fn test_monitor_ignores_other_jobs() {
        let (event_tx, _) = broadcast::channel::<(Uuid, String, AppEvent)>(16);
        let (inject_tx, mut inject_rx) = mpsc::channel::<IncomingMessage>(16);

        let job_id = Uuid::new_v4();
        let other_job_id = Uuid::new_v4();
        let _handle = spawn_job_monitor(job_id, event_tx.subscribe(), inject_tx, test_route());

        // Send a message for a different job
        event_tx
            .send((
                other_job_id,
                "test-user".to_string(),
                AppEvent::JobMessage {
                    job_id: other_job_id.to_string(),
                    role: "assistant".to_string(),
                    content: "wrong job".to_string(),
                },
            ))
            .unwrap();

        // Should not receive anything
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(100), inject_rx.recv()).await;
        assert!(
            result.is_err(),
            "should have timed out, no message expected"
        );
    }

    #[tokio::test]
    async fn test_monitor_exits_on_job_result() {
        let (event_tx, _) = broadcast::channel::<(Uuid, String, AppEvent)>(16);
        let (inject_tx, mut inject_rx) = mpsc::channel::<IncomingMessage>(16);

        let job_id = Uuid::new_v4();
        let handle = spawn_job_monitor(job_id, event_tx.subscribe(), inject_tx, test_route());

        // Send a completion event
        event_tx
            .send((
                job_id,
                "test-user".to_string(),
                AppEvent::JobResult {
                    job_id: job_id.to_string(),
                    status: "completed".to_string(),
                    session_id: None,
                    fallback_deliverable: None,
                },
            ))
            .unwrap();

        // Should receive the completion message
        let msg = tokio::time::timeout(std::time::Duration::from_secs(1), inject_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(msg.content.contains("finished"));

        // The monitor task should exit
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("monitor should have exited")
            .expect("monitor task should not panic");
    }

    #[tokio::test]
    async fn test_monitor_skips_tool_events() {
        let (event_tx, _) = broadcast::channel::<(Uuid, String, AppEvent)>(16);
        let (inject_tx, mut inject_rx) = mpsc::channel::<IncomingMessage>(16);

        let job_id = Uuid::new_v4();
        let _handle = spawn_job_monitor(job_id, event_tx.subscribe(), inject_tx, test_route());

        // Send tool use event (should be skipped)
        event_tx
            .send((
                job_id,
                "test-user".to_string(),
                AppEvent::JobToolUse {
                    job_id: job_id.to_string(),
                    tool_name: "shell".to_string(),
                    input: serde_json::json!({"command": "ls"}),
                },
            ))
            .unwrap();

        // Send user message (should be skipped)
        event_tx
            .send((
                job_id,
                "test-user".to_string(),
                AppEvent::JobMessage {
                    job_id: job_id.to_string(),
                    role: "user".to_string(),
                    content: "user prompt".to_string(),
                },
            ))
            .unwrap();

        // Should not receive anything for tool events or user messages
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(100), inject_rx.recv()).await;
        assert!(
            result.is_err(),
            "should have timed out, no message expected"
        );
    }

    /// Regression test: external channels must not be able to spoof the
    /// `is_internal` flag via metadata keys. A message created through
    /// the normal `IncomingMessage::new` + `with_metadata` path must
    /// always have `is_internal == false`, regardless of metadata content.
    #[test]
    fn test_external_metadata_cannot_spoof_internal_flag() {
        let msg = IncomingMessage::new("wasm_channel", "attacker", "pwned").with_metadata(
            serde_json::json!({
                "__internal_job_monitor": true,
                "is_internal": true,
            }),
        );
        assert!(
            !msg.is_internal,
            "with_metadata must not set is_internal — only into_internal() can"
        );
    }

    #[test]
    fn test_into_internal_sets_flag() {
        let msg = IncomingMessage::new("monitor", "system", "test").into_internal();
        assert!(msg.is_internal);
    }

    #[tokio::test]
    async fn test_monitor_preserves_response_metadata() {
        let (event_tx, _) = broadcast::channel::<(Uuid, String, AppEvent)>(16);
        let (inject_tx, mut inject_rx) = mpsc::channel::<IncomingMessage>(16);

        let job_id = Uuid::new_v4();
        let route = JobMonitorRoute {
            channel: "slack".to_string(),
            user_id: "user-1".to_string(),
            thread_id: Some("thread-1".to_string()),
            response_metadata: serde_json::json!({
                "channel": "C123",
                "thread_ts": "thread-1",
                "message_ts": "123.456"
            }),
        };
        let _handle = spawn_job_monitor(job_id, event_tx.subscribe(), inject_tx, route.clone());

        event_tx
            .send((
                job_id,
                "test-user".to_string(),
                AppEvent::JobResult {
                    job_id: job_id.to_string(),
                    status: "completed".to_string(),
                    session_id: None,
                    fallback_deliverable: Some(serde_json::Value::String("All done".to_string())),
                },
            ))
            .unwrap();

        let msg = tokio::time::timeout(std::time::Duration::from_secs(1), inject_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(msg.metadata, route.response_metadata);
        assert!(msg.content.contains("All done"));
    }

    // === Regression: fire-and-forget sandbox jobs must transition out of InProgress ===
    // Before this fix, spawn_job_monitor only forwarded SSE messages but never
    // updated ContextManager. Background sandbox jobs stayed InProgress forever,
    // permanently consuming a max_jobs slot.

    #[tokio::test]
    async fn test_monitor_transitions_context_on_completion() {
        use crate::context::{ContextManager, JobState};

        let cm = Arc::new(ContextManager::new(5));
        let job_id = Uuid::new_v4();
        cm.register_sandbox_job(job_id, "user-1", "Build app", "desc")
            .await
            .unwrap();

        let (event_tx, _) = broadcast::channel::<(Uuid, String, AppEvent)>(16);
        let (inject_tx, mut inject_rx) = mpsc::channel::<IncomingMessage>(16);

        let handle = spawn_job_monitor_with_context(
            job_id,
            event_tx.subscribe(),
            inject_tx,
            test_route(),
            Some(Arc::clone(&cm)),
        );

        // Send completion event
        event_tx
            .send((
                job_id,
                "test-user".to_string(),
                AppEvent::JobResult {
                    job_id: job_id.to_string(),
                    status: "completed".to_string(),
                    session_id: None,
                    fallback_deliverable: None,
                },
            ))
            .unwrap();

        // Drain the injected message
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), inject_rx.recv()).await;

        // Wait for monitor to exit
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("monitor should exit")
            .expect("monitor should not panic");

        // Job should now be Completed, not InProgress
        let ctx = cm.get_context(job_id).await.unwrap();
        assert_eq!(ctx.state, JobState::Completed);
    }

    #[tokio::test]
    async fn test_monitor_transitions_context_on_failure() {
        use crate::context::{ContextManager, JobState};

        let cm = Arc::new(ContextManager::new(5));
        let job_id = Uuid::new_v4();
        cm.register_sandbox_job(job_id, "user-1", "Build app", "desc")
            .await
            .unwrap();

        let (event_tx, _) = broadcast::channel::<(Uuid, String, AppEvent)>(16);
        let (inject_tx, mut inject_rx) = mpsc::channel::<IncomingMessage>(16);

        let handle = spawn_job_monitor_with_context(
            job_id,
            event_tx.subscribe(),
            inject_tx,
            test_route(),
            Some(Arc::clone(&cm)),
        );

        // Send failure event
        event_tx
            .send((
                job_id,
                "test-user".to_string(),
                AppEvent::JobResult {
                    job_id: job_id.to_string(),
                    status: "failed".to_string(),
                    session_id: None,
                    fallback_deliverable: None,
                },
            ))
            .unwrap();

        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), inject_rx.recv()).await;
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("monitor should exit")
            .expect("monitor should not panic");

        let ctx = cm.get_context(job_id).await.unwrap();
        assert_eq!(ctx.state, JobState::Failed);
    }

    // === Regression: completion watcher (no route metadata) ===
    // When monitor_route_from_ctx() returns None, spawn_completion_watcher
    // must still transition the job so the max_jobs slot is freed.

    #[tokio::test]
    async fn test_completion_watcher_transitions_on_result() {
        use crate::context::{ContextManager, JobState};

        let cm = Arc::new(ContextManager::new(5));
        let job_id = Uuid::new_v4();
        cm.register_sandbox_job(job_id, "user-1", "Build app", "desc")
            .await
            .unwrap();

        let (event_tx, _) = broadcast::channel::<(Uuid, String, AppEvent)>(16);
        let handle = spawn_completion_watcher(job_id, event_tx.subscribe(), Arc::clone(&cm));

        event_tx
            .send((
                job_id,
                "test-user".to_string(),
                AppEvent::JobResult {
                    job_id: job_id.to_string(),
                    status: "completed".to_string(),
                    session_id: None,
                    fallback_deliverable: None,
                },
            ))
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("watcher should exit")
            .expect("watcher should not panic");

        let ctx = cm.get_context(job_id).await.unwrap();
        assert_eq!(ctx.state, JobState::Completed);
    }
}
