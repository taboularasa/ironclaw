//! Thread lifecycle management.
//!
//! - [`ThreadManager`] — top-level orchestrator for spawning and supervising threads
//! - [`ThreadTree`] — parent-child relationship tracking
//! - [`messaging`] — inter-thread signal channel

pub mod manager;
pub mod messaging;
pub mod tree;

pub use manager::ThreadManager;
pub use messaging::ThreadOutcome;
pub use tree::ThreadTree;
