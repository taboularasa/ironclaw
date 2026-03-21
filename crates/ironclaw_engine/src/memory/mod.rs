//! Memory document system.
//!
//! - [`MemoryStore`] — project-scoped document CRUD
//! - [`RetrievalEngine`] — context building from project docs (Phase 4)

pub mod retrieval;
pub mod store;

pub use retrieval::RetrievalEngine;
pub use store::MemoryStore;
