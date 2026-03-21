//! Context retrieval engine.
//!
//! Builds context for thread steps by retrieving relevant memory docs
//! from the project. Phase 1: stub. Phase 4 implements keyword + semantic search.

use crate::types::error::EngineError;
use crate::types::memory::MemoryDoc;
use crate::types::project::ProjectId;

/// Retrieves relevant memory docs for a thread's context.
pub struct RetrievalEngine;

impl RetrievalEngine {
    pub fn new() -> Self {
        Self
    }

    /// Retrieve relevant memory docs for the given query.
    ///
    /// Phase 1: returns empty vec. Phase 4 implements search.
    pub async fn retrieve_context(
        &self,
        _project_id: ProjectId,
        _query: &str,
        _max_docs: usize,
    ) -> Result<Vec<MemoryDoc>, EngineError> {
        Ok(Vec::new())
    }
}

impl Default for RetrievalEngine {
    fn default() -> Self {
        Self::new()
    }
}
