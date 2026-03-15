//! Tool failure tracking types shared between `db` and `agent` modules.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A tool that has been detected as broken (high failure rate).
///
/// Previously named `BrokenTool` in `agent::self_repair`. Renamed to
/// `ToolFailureRecord` to better reflect its role as a persistence DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFailureRecord {
    pub name: String,
    pub failure_count: u32,
    pub last_error: Option<String>,
    pub first_failure: DateTime<Utc>,
    pub last_failure: DateTime<Utc>,
    pub last_build_result: Option<serde_json::Value>,
    pub repair_attempts: u32,
}
