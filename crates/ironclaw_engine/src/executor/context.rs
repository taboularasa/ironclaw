//! Context building for LLM calls.
//!
//! Assembles the message sequence and action definitions from thread state,
//! active leases, and (Phase 4) project memory docs.

use std::sync::Arc;

use crate::types::capability::{ActionDef, CapabilityLease};
use crate::types::error::EngineError;
use crate::types::message::ThreadMessage;
use crate::traits::effect::EffectExecutor;

/// Build the context for an LLM call: messages and available actions.
///
/// Phase 1: passes through thread messages + resolves actions from leases.
/// Phase 4 will add memory doc retrieval and injection.
pub async fn build_step_context(
    messages: &[ThreadMessage],
    leases: &[CapabilityLease],
    effects: &Arc<dyn EffectExecutor>,
) -> Result<(Vec<ThreadMessage>, Vec<ActionDef>), EngineError> {
    let actions = effects.available_actions(leases).await?;
    Ok((messages.to_vec(), actions))
}
