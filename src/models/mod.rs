//! Shared domain types used across module boundaries.
//!
//! Types in this module are imported by both the persistence layer (`db`) and
//! the domain logic (`agent`), breaking the circular dependency that existed
//! when these types lived inside `agent/`.

pub mod routine;
pub mod tool_failure;

pub use routine::{
    NotifyConfig, Routine, RoutineAction, RoutineGuardrails, RoutineRun, RunStatus, Trigger,
};
pub use tool_failure::ToolFailureRecord;
