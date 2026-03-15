//! HTTP interception trait for trace recording and replay.
//!
//! Lives in `observability` rather than `llm::recording` so that `context::state`
//! can depend on it without pulling in the LLM module.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// The request side of an HTTP exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpExchangeRequest {
    pub method: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// The response side of an HTTP exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpExchangeResponse {
    pub status: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// A matched request/response pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpExchange {
    pub request: HttpExchangeRequest,
    pub response: HttpExchangeResponse,
}

/// Trait for intercepting HTTP requests from tools.
///
/// During recording, the interceptor captures exchanges after the real
/// request completes. During replay, it short-circuits with a recorded response.
#[async_trait]
pub trait HttpInterceptor: Send + Sync + std::fmt::Debug {
    /// Called before making an HTTP request.
    ///
    /// Return `Some(response)` to short-circuit (replay mode).
    /// Return `None` to let the real request proceed (recording mode).
    async fn before_request(&self, request: &HttpExchangeRequest) -> Option<HttpExchangeResponse>;

    /// Called after a real HTTP request completes (recording mode only).
    async fn after_response(&self, request: &HttpExchangeRequest, response: &HttpExchangeResponse);
}
