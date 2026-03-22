//! Engine v2 router — handles user messages via the engine when enabled.

use std::sync::{Arc, OnceLock};

use tokio::sync::RwLock;
use tracing::{debug, info};

use ironclaw_engine::{
    Capability, CapabilityRegistry, ConversationManager, LeaseManager, PolicyEngine, Project,
    Store, ThreadConfig, ThreadManager, ThreadOutcome,
};

use crate::agent::Agent;
use crate::bridge::effect_adapter::EffectBridgeAdapter;
use crate::bridge::llm_adapter::LlmBridgeAdapter;
use crate::bridge::store_adapter::InMemoryStore;
use crate::channels::IncomingMessage;
use crate::error::Error;

/// Check if the engine v2 is enabled via `ENGINE_V2=true` environment variable.
pub fn is_engine_v2_enabled() -> bool {
    std::env::var("ENGINE_V2")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
}

/// Persistent engine state that lives across messages.
struct EngineState {
    thread_manager: Arc<ThreadManager>,
    conversation_manager: ConversationManager,
    #[allow(dead_code)]
    store: Arc<InMemoryStore>,
    default_project_id: ironclaw_engine::ProjectId,
}

/// Global engine state, initialized on first use.
static ENGINE_STATE: OnceLock<RwLock<Option<EngineState>>> = OnceLock::new();

/// Get or initialize the engine state using the agent's dependencies.
async fn get_or_init_engine(agent: &Agent) -> Result<(), Error> {
    let lock = ENGINE_STATE.get_or_init(|| RwLock::new(None));
    let guard = lock.read().await;
    if guard.is_some() {
        return Ok(());
    }
    drop(guard);

    // Initialize
    let mut guard = lock.write().await;
    if guard.is_some() {
        return Ok(()); // double-check after acquiring write lock
    }

    info!("engine v2: initializing engine state");

    let llm_adapter = Arc::new(LlmBridgeAdapter::new(
        agent.llm().clone(),
        Some(agent.cheap_llm().clone()),
    ));

    let effect_adapter = Arc::new(EffectBridgeAdapter::new(
        agent.tools().clone(),
        agent.safety().clone(),
    ));

    let store = Arc::new(InMemoryStore::new());

    // Build capability registry from available tools
    let mut capabilities = CapabilityRegistry::new();
    let tool_defs = agent.tools().tool_definitions().await;
    if !tool_defs.is_empty() {
        capabilities.register(Capability {
            name: "tools".into(),
            description: "Available tools".into(),
            actions: tool_defs
                .into_iter()
                .map(|td| ironclaw_engine::ActionDef {
                    name: td.name,
                    description: td.description,
                    parameters_schema: td.parameters,
                    effects: vec![],
                    requires_approval: false,
                })
                .collect(),
            knowledge: vec![],
            policies: vec![],
        });
    }

    let leases = Arc::new(LeaseManager::new());
    let policy = Arc::new(PolicyEngine::new());

    let thread_manager = Arc::new(ThreadManager::new(
        llm_adapter,
        effect_adapter,
        store.clone(),
        Arc::new(capabilities),
        leases,
        policy,
    ));

    // Create a default project
    let project = Project::new("default", "Default project for engine v2");
    let project_id = project.id;
    store.save_project(&project).await.map_err(|e| {
        crate::error::Error::from(crate::error::JobError::ContextError {
            id: uuid::Uuid::nil(),
            reason: format!("engine v2 store error: {e}"),
        })
    })?;

    let conversation_manager = ConversationManager::new(Arc::clone(&thread_manager));

    *guard = Some(EngineState {
        thread_manager,
        conversation_manager,
        store: store.clone(),
        default_project_id: project_id,
    });

    Ok(())
}

/// Handle a user message through the engine v2 pipeline.
///
/// Conversations and threads persist across messages within the same
/// agent lifetime. Each (channel, user) pair gets a conversation;
/// consecutive messages inject into the active thread or spawn a new one.
pub async fn handle_with_engine(
    agent: &Agent,
    message: &IncomingMessage,
    content: &str,
) -> Result<Option<String>, Error> {
    // Ensure engine is initialized
    get_or_init_engine(agent).await?;

    let lock = ENGINE_STATE.get().expect("engine initialized");
    let guard = lock.read().await;
    let state = guard.as_ref().expect("engine initialized");

    info!(
        user_id = %message.user_id,
        channel = %message.channel,
        "engine v2: handling message"
    );

    // Get or create conversation for this channel+user
    let conv_id = state
        .conversation_manager
        .get_or_create_conversation(&message.channel, &message.user_id)
        .await;

    // Handle the message — spawns a new thread or injects into active one
    let thread_id = state
        .conversation_manager
        .handle_user_message(
            conv_id,
            content,
            state.default_project_id,
            &message.user_id,
            ThreadConfig::default(),
        )
        .await
        .map_err(|e| {
            crate::error::Error::from(crate::error::JobError::ContextError {
                id: uuid::Uuid::nil(),
                reason: format!("engine v2 error: {e}"),
            })
        })?;

    debug!(thread_id = %thread_id, "engine v2: thread active, waiting for completion");

    // Wait for the thread to complete
    let outcome = state
        .thread_manager
        .join_thread(thread_id)
        .await
        .map_err(|e| {
            crate::error::Error::from(crate::error::JobError::ContextError {
                id: uuid::Uuid::nil(),
                reason: format!("engine v2 join error: {e}"),
            })
        })?;

    // Record outcome in conversation
    state
        .conversation_manager
        .record_thread_outcome(conv_id, thread_id, &outcome)
        .await;

    // Convert outcome to response
    match outcome {
        ThreadOutcome::Completed { response } => {
            debug!(thread_id = %thread_id, "engine v2: completed");
            Ok(response)
        }
        ThreadOutcome::Stopped => Ok(Some("Thread was stopped.".into())),
        ThreadOutcome::MaxIterations => {
            Ok(Some("Reached maximum iterations without completing.".into()))
        }
        ThreadOutcome::Failed { error } => Ok(Some(format!("Error: {error}"))),
        ThreadOutcome::NeedApproval {
            action_name,
            call_id: _,
            parameters: _,
        } => Ok(Some(format!(
            "Action '{action_name}' requires approval (not yet supported in engine v2)"
        ))),
    }
}
