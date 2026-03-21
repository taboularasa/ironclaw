//! Core execution loop — the replacement for `run_agentic_loop()`.
//!
//! The `ExecutionLoop` owns a thread and drives it through LLM call →
//! action execution → result processing → repeat cycles. Unlike the
//! existing delegate pattern, the loop is self-contained: all behavior
//! differences between thread types are handled via capability leases
//! and policy, not delegate implementations.

use std::sync::Arc;

use tracing::{debug, warn};

use crate::capability::lease::LeaseManager;
use crate::capability::policy::PolicyEngine;
use crate::executor::context::build_step_context;
use crate::executor::intent;
use crate::executor::structured::execute_action_calls;
use crate::runtime::messaging::{SignalReceiver, ThreadOutcome, ThreadSignal};
use crate::traits::effect::{EffectExecutor, ThreadExecutionContext};
use crate::traits::llm::{LlmBackend, LlmCallConfig};
use crate::types::error::EngineError;
use crate::types::event::EventKind;
use crate::types::message::ThreadMessage;
use crate::types::step::{LlmResponse, Step, StepStatus};
use crate::types::thread::{Thread, ThreadState};

/// The core execution loop for a thread.
pub struct ExecutionLoop {
    pub thread: Thread,
    llm: Arc<dyn LlmBackend>,
    effects: Arc<dyn EffectExecutor>,
    leases: Arc<LeaseManager>,
    policy: Arc<PolicyEngine>,
    signal_rx: SignalReceiver,
    user_id: String,
}

impl ExecutionLoop {
    pub fn new(
        thread: Thread,
        llm: Arc<dyn LlmBackend>,
        effects: Arc<dyn EffectExecutor>,
        leases: Arc<LeaseManager>,
        policy: Arc<PolicyEngine>,
        signal_rx: SignalReceiver,
        user_id: String,
    ) -> Self {
        Self {
            thread,
            llm,
            effects,
            leases,
            policy,
            signal_rx,
            user_id,
        }
    }

    /// Run the execution loop to completion.
    pub async fn run(&mut self) -> Result<ThreadOutcome, EngineError> {
        // Transition to Running
        self.thread.transition_to(ThreadState::Running, None)?;

        let max_iterations = self.thread.config.max_iterations;
        let max_nudges = self.thread.config.max_tool_intent_nudges;
        let nudge_enabled = self.thread.config.enable_tool_intent_nudge;
        let mut nudge_count: u32 = 0;

        for iteration in 0..max_iterations {
            // 1. Check signals
            match self.check_signals() {
                SignalAction::Continue => {}
                SignalAction::Stop => {
                    self.thread
                        .transition_to(ThreadState::Completed, Some("stopped by signal".into()))?;
                    return Ok(ThreadOutcome::Stopped);
                }
                SignalAction::Inject(msg) => {
                    self.thread.add_message(msg);
                }
            }

            // 2. Get active leases
            let active_leases = self.leases.active_for_thread(self.thread.id).await;

            // 3. Build context
            let (messages, actions) =
                build_step_context(&self.thread.messages, &active_leases, &self.effects).await?;

            // 4. Create step
            let mut step = Step::new(self.thread.id, iteration + 1);
            step.status = StepStatus::LlmCalling;
            self.thread.add_event(EventKind::StepStarted {
                step_id: step.id,
            });

            // 5. Call LLM
            let force_text = iteration >= max_iterations.saturating_sub(1);
            let config = LlmCallConfig {
                force_text,
                ..LlmCallConfig::default()
            };

            let llm_output = self.llm.complete(&messages, &actions, &config).await?;
            step.tokens_used = llm_output.usage;
            self.thread.total_tokens_used += llm_output.usage.total();
            step.llm_response = Some(llm_output.response.clone());

            // 6. Handle response
            match llm_output.response {
                LlmResponse::Text(text) => {
                    // Check for tool intent nudge
                    if nudge_enabled
                        && nudge_count < max_nudges
                        && intent::signals_tool_intent(&text)
                    {
                        nudge_count += 1;
                        debug!(
                            thread_id = %self.thread.id,
                            nudge_count,
                            "tool intent detected, injecting nudge"
                        );
                        self.thread
                            .add_message(ThreadMessage::assistant(text));
                        self.thread
                            .add_message(ThreadMessage::system(intent::TOOL_INTENT_NUDGE));

                        step.status = StepStatus::Completed;
                        step.completed_at = Some(chrono::Utc::now());
                        self.thread.add_event(EventKind::StepCompleted {
                            step_id: step.id,
                            tokens: step.tokens_used,
                        });
                        self.thread.step_count += 1;
                        continue;
                    }

                    // Final text response
                    self.thread
                        .add_message(ThreadMessage::assistant(text.clone()));

                    step.status = StepStatus::Completed;
                    step.completed_at = Some(chrono::Utc::now());
                    self.thread.add_event(EventKind::StepCompleted {
                        step_id: step.id,
                        tokens: step.tokens_used,
                    });
                    self.thread.step_count += 1;

                    self.thread.transition_to(
                        ThreadState::Completed,
                        Some("text response".into()),
                    )?;
                    return Ok(ThreadOutcome::Completed {
                        response: Some(text),
                    });
                }

                LlmResponse::ActionCalls { calls, content } => {
                    nudge_count = 0;

                    // Record assistant message with action calls
                    self.thread
                        .add_message(ThreadMessage::assistant_with_actions(content, calls.clone()));

                    step.status = StepStatus::Executing;

                    // Build execution context
                    let exec_ctx = ThreadExecutionContext {
                        thread_id: self.thread.id,
                        thread_type: self.thread.thread_type,
                        project_id: self.thread.project_id,
                        user_id: self.user_id.clone(),
                        step_id: step.id,
                    };

                    // Execute actions
                    let batch = execute_action_calls(
                        &calls,
                        &self.thread,
                        &self.effects,
                        &self.leases,
                        &self.policy,
                        &exec_ctx,
                        &[], // capability-level policies (TODO: resolve from registry)
                    )
                    .await?;

                    // Record events
                    for event_kind in batch.events {
                        self.thread.add_event(event_kind);
                    }

                    // Add action results as messages
                    for result in &batch.results {
                        self.thread.add_message(ThreadMessage::action_result(
                            &result.call_id,
                            &result.action_name,
                            serde_json::to_string(&result.output).unwrap_or_default(),
                        ));
                    }

                    step.action_results = batch.results;
                    step.status = StepStatus::Completed;
                    step.completed_at = Some(chrono::Utc::now());
                    self.thread.add_event(EventKind::StepCompleted {
                        step_id: step.id,
                        tokens: step.tokens_used,
                    });
                    self.thread.step_count += 1;

                    // Check if approval is needed
                    if let Some(outcome) = batch.need_approval {
                        self.thread
                            .transition_to(ThreadState::Waiting, Some("awaiting approval".into()))?;
                        return Ok(outcome);
                    }
                }
            }
        }

        // Max iterations reached
        warn!(
            thread_id = %self.thread.id,
            max_iterations,
            "max iterations reached"
        );
        self.thread.transition_to(
            ThreadState::Completed,
            Some("max iterations reached".into()),
        )?;
        Ok(ThreadOutcome::MaxIterations)
    }

    /// Check for pending signals without blocking.
    fn check_signals(&mut self) -> SignalAction {
        match self.signal_rx.try_recv() {
            Ok(ThreadSignal::Stop) => SignalAction::Stop,
            Ok(ThreadSignal::InjectMessage(msg)) => SignalAction::Inject(msg),
            Ok(ThreadSignal::Suspend) => {
                // For now, treat suspend as stop. Phase 3 adds proper suspend/resume.
                SignalAction::Stop
            }
            Ok(ThreadSignal::Resume) | Ok(ThreadSignal::ChildCompleted { .. }) => {
                SignalAction::Continue
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => SignalAction::Continue,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                // Channel closed — the manager dropped our sender. Treat as stop.
                SignalAction::Stop
            }
        }
    }
}

enum SignalAction {
    Continue,
    Stop,
    Inject(ThreadMessage),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::capability::{ActionDef, CapabilityLease, EffectType};
    use crate::types::project::ProjectId;
    use crate::types::step::{ActionResult, TokenUsage};
    use crate::types::thread::{ThreadConfig, ThreadType};
    use crate::traits::llm::{LlmCallConfig, LlmOutput};

    use std::sync::Mutex;
    use std::time::Duration;

    // ── Mock LLM ────────────────────────────────────────────

    struct MockLlm {
        responses: Mutex<Vec<LlmOutput>>,
    }

    impl MockLlm {
        fn new(responses: Vec<LlmOutput>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmBackend for MockLlm {
        async fn complete(
            &self,
            _messages: &[ThreadMessage],
            _actions: &[ActionDef],
            _config: &LlmCallConfig,
        ) -> Result<LlmOutput, EngineError> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(LlmOutput {
                    response: LlmResponse::Text("(no more responses)".into()),
                    usage: TokenUsage::default(),
                })
            } else {
                Ok(responses.remove(0))
            }
        }

        fn model_name(&self) -> &str {
            "mock"
        }
    }

    // ── Mock EffectExecutor ─────────────────────────────────

    struct MockEffects {
        results: Mutex<Vec<Result<ActionResult, EngineError>>>,
        actions: Vec<ActionDef>,
    }

    impl MockEffects {
        fn new(actions: Vec<ActionDef>, results: Vec<Result<ActionResult, EngineError>>) -> Self {
            Self {
                results: Mutex::new(results),
                actions,
            }
        }
    }

    #[async_trait::async_trait]
    impl EffectExecutor for MockEffects {
        async fn execute_action(
            &self,
            _action_name: &str,
            _parameters: serde_json::Value,
            _lease: &CapabilityLease,
            _context: &ThreadExecutionContext,
        ) -> Result<ActionResult, EngineError> {
            let mut results = self.results.lock().unwrap();
            if results.is_empty() {
                Ok(ActionResult {
                    call_id: String::new(),
                    action_name: String::new(),
                    output: serde_json::json!({"result": "ok"}),
                    is_error: false,
                    duration: Duration::from_millis(1),
                })
            } else {
                results.remove(0)
            }
        }

        async fn available_actions(
            &self,
            _leases: &[CapabilityLease],
        ) -> Result<Vec<ActionDef>, EngineError> {
            Ok(self.actions.clone())
        }
    }

    // ── Helpers ─────────────────────────────────────────────

    fn text_response(text: &str) -> LlmOutput {
        LlmOutput {
            response: LlmResponse::Text(text.into()),
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
        }
    }

    fn action_response(action_name: &str, call_id: &str) -> LlmOutput {
        LlmOutput {
            response: LlmResponse::ActionCalls {
                calls: vec![crate::types::step::ActionCall {
                    id: call_id.into(),
                    action_name: action_name.into(),
                    parameters: serde_json::json!({}),
                }],
                content: None,
            },
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
        }
    }

    fn test_action() -> ActionDef {
        ActionDef {
            name: "test_tool".into(),
            description: "A test tool".into(),
            parameters_schema: serde_json::json!({"type": "object"}),
            effects: vec![EffectType::ReadLocal],
            requires_approval: false,
        }
    }

    async fn make_loop(
        llm_responses: Vec<LlmOutput>,
        effect_results: Vec<Result<ActionResult, EngineError>>,
        config: ThreadConfig,
    ) -> (ExecutionLoop, crate::runtime::messaging::SignalSender) {
        let project_id = ProjectId::new();
        let thread = Thread::new("test goal", ThreadType::Foreground, project_id, config);
        let tid = thread.id;

        let llm = Arc::new(MockLlm::new(llm_responses));
        let effects = Arc::new(MockEffects::new(vec![test_action()], effect_results));
        let leases = Arc::new(LeaseManager::new());
        let policy = Arc::new(PolicyEngine::new());

        // Grant a default lease
        leases
            .grant(tid, "test_cap", vec![], None, None)
            .await;

        let (tx, rx) = crate::runtime::messaging::signal_channel(16);

        let exec = ExecutionLoop::new(
            thread,
            llm,
            effects,
            leases,
            policy,
            rx,
            "test-user".into(),
        );
        (exec, tx)
    }

    // ── Tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn text_response_completes() {
        let (mut exec, _tx) = make_loop(
            vec![text_response("Hello!")],
            vec![],
            ThreadConfig::default(),
        )
        .await;

        let outcome = exec.run().await.unwrap();
        assert!(matches!(outcome, ThreadOutcome::Completed { response: Some(r) } if r == "Hello!"));
        assert!(exec.thread.state.is_terminal() || exec.thread.state == ThreadState::Completed);
        assert_eq!(exec.thread.step_count, 1);
        assert!(exec.thread.total_tokens_used > 0);
    }

    #[tokio::test]
    async fn action_then_text() {
        let (mut exec, _tx) = make_loop(
            vec![
                action_response("test_tool", "call_1"),
                text_response("Done!"),
            ],
            vec![Ok(ActionResult {
                call_id: "call_1".into(),
                action_name: "test_tool".into(),
                output: serde_json::json!({"data": "result"}),
                is_error: false,
                duration: Duration::from_millis(5),
            })],
            ThreadConfig::default(),
        )
        .await;

        let outcome = exec.run().await.unwrap();
        assert!(matches!(outcome, ThreadOutcome::Completed { response: Some(r) } if r == "Done!"));
        assert_eq!(exec.thread.step_count, 2);
        // Should have: system(nudge not counted), assistant+actions, action_result, assistant
        assert!(exec.thread.messages.len() >= 3);
    }

    #[tokio::test]
    async fn max_iterations_reached() {
        // LLM always returns actions, so it never exits naturally
        let many_actions: Vec<LlmOutput> = (0..5)
            .map(|i| action_response("test_tool", &format!("call_{i}")))
            .collect();

        let many_results: Vec<Result<ActionResult, EngineError>> = (0..5)
            .map(|i| {
                Ok(ActionResult {
                    call_id: format!("call_{i}"),
                    action_name: "test_tool".into(),
                    output: serde_json::json!({"i": i}),
                    is_error: false,
                    duration: Duration::from_millis(1),
                })
            })
            .collect();

        let config = ThreadConfig {
            max_iterations: 3,
            ..ThreadConfig::default()
        };

        let (mut exec, _tx) = make_loop(many_actions, many_results, config).await;

        let outcome = exec.run().await.unwrap();
        // The last iteration forces text mode, and MockLlm returns action_response
        // which gets treated as the 3rd iteration, then on the 3rd iteration force_text
        // is set. But MockLlm ignores force_text. So we get MaxIterations after 3 iterations.
        // Actually, max_iterations=3, and force_text is set when iteration >= max-1 = 2,
        // so iteration 2 (0-indexed) has force_text. The MockLlm still returns action calls,
        // so we loop 3 times and exit.
        assert!(matches!(
            outcome,
            ThreadOutcome::MaxIterations | ThreadOutcome::Completed { .. }
        ));
        assert!(exec.thread.step_count <= 3);
    }

    #[tokio::test]
    async fn stop_signal_exits() {
        // LLM would loop forever, but we send a stop signal
        let many_actions: Vec<LlmOutput> = (0..100)
            .map(|i| action_response("test_tool", &format!("call_{i}")))
            .collect();

        let many_results: Vec<Result<ActionResult, EngineError>> = (0..100)
            .map(|i| {
                Ok(ActionResult {
                    call_id: format!("call_{i}"),
                    action_name: "test_tool".into(),
                    output: serde_json::json!({}),
                    is_error: false,
                    duration: Duration::from_millis(1),
                })
            })
            .collect();

        let (mut exec, tx) = make_loop(many_actions, many_results, ThreadConfig::default()).await;

        // Send stop before first iteration
        tx.send(ThreadSignal::Stop).await.unwrap();

        let outcome = exec.run().await.unwrap();
        assert!(matches!(outcome, ThreadOutcome::Stopped));
    }

    #[tokio::test]
    async fn inject_message_appears_in_context() {
        let (mut exec, tx) = make_loop(
            vec![text_response("Got your message")],
            vec![],
            ThreadConfig::default(),
        )
        .await;

        tx.send(ThreadSignal::InjectMessage(ThreadMessage::user(
            "injected!",
        )))
        .await
        .unwrap();

        let outcome = exec.run().await.unwrap();
        assert!(matches!(outcome, ThreadOutcome::Completed { .. }));
        assert!(exec
            .thread
            .messages
            .iter()
            .any(|m| m.content == "injected!"));
    }

    #[tokio::test]
    async fn tool_intent_nudge_injected() {
        let (mut exec, _tx) = make_loop(
            vec![
                text_response("Let me search for that"),
                text_response("The answer is 42"),
            ],
            vec![],
            ThreadConfig {
                enable_tool_intent_nudge: true,
                max_tool_intent_nudges: 2,
                ..ThreadConfig::default()
            },
        )
        .await;

        let outcome = exec.run().await.unwrap();
        assert!(
            matches!(outcome, ThreadOutcome::Completed { response: Some(r) } if r == "The answer is 42")
        );
        assert_eq!(exec.thread.step_count, 2);
        // Should have nudge system message
        assert!(exec
            .thread
            .messages
            .iter()
            .any(|m| m.content.contains("didn't make an action call")));
    }

    #[tokio::test]
    async fn events_are_recorded() {
        let (mut exec, _tx) = make_loop(
            vec![text_response("Hello!")],
            vec![],
            ThreadConfig::default(),
        )
        .await;

        exec.run().await.unwrap();

        let _event_kinds: Vec<String> = exec
            .thread
            .events
            .iter()
            .map(|e| format!("{:?}", std::mem::discriminant(&e.kind)))
            .collect();

        // Should have: StateChanged(Created->Running), StepStarted, MessageAdded,
        // StepCompleted, StateChanged(Running->Completed)
        assert!(exec.thread.events.len() >= 4);

        // Verify first event is state change to Running
        assert!(matches!(
            &exec.thread.events[0].kind,
            EventKind::StateChanged {
                from: ThreadState::Created,
                to: ThreadState::Running,
                ..
            }
        ));
    }
}
