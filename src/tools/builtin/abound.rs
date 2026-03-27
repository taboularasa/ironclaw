//! Mock Abound API tools for demo purposes.
//!
//! These tools simulate Abound's backend API (account info, wire transfers,
//! exchange rates, notifications, forex scoring) with realistic mock data.
//! They are feature-gated behind `--features demo` and will be replaced by
//! real WASM tools once Abound's backend is live.

use std::time::Instant;

use async_trait::async_trait;
use chrono::{Datelike, Utc};
use rand::Rng;
use serde_json::json;

use crate::context::JobContext;
use crate::tools::tool::{ApprovalRequirement, Tool, ToolError, ToolOutput, require_str};

// ---------------------------------------------------------------------------
// Tool 1: Get Account Info
// ---------------------------------------------------------------------------

/// Returns mock Abound account data (limits, recipients, funding sources).
pub struct AboundGetAccountInfoTool;

#[async_trait]
impl Tool for AboundGetAccountInfoTool {
    fn name(&self) -> &str {
        "abound_get_account_info"
    }

    fn description(&self) -> &str {
        "Retrieve the authenticated user's Abound account information including \
         transfer limits, payment reasons, recipients, and funding sources."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        let data = json!({
            "status": "success",
            "data": {
                "user_id": "acc_123456",
                "user_name": "John Doe",
                "limits": {
                    "ach_limit": {
                        "limit": 5000,
                        "formatted_limit": "$5,000"
                    }
                },
                "payment_reasons": [
                    { "key": "FAMILY_MAINTENANCE", "value": "Family Maintenance" },
                    { "key": "GIFT", "value": "Gift" },
                    { "key": "EDUCATION_SUPPORT", "value": "Education Support" },
                    { "key": "MEDICAL_SUPPORT", "value": "Medical Support" }
                ],
                "recipients": [
                    {
                        "beneficiary_ref_id": "ben_001",
                        "name": "Rahul Sharma",
                        "mask": "****2222"
                    }
                ],
                "funding_sources": [
                    {
                        "funding_source_id": "fs_001",
                        "bank_name": "HDFC Bank",
                        "mask": "****2222"
                    }
                ]
            }
        });

        Ok(ToolOutput::success(data, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Tool 2: Get Exchange Rate
// ---------------------------------------------------------------------------

/// Returns mock USD/INR exchange rate with slight randomization.
pub struct AboundGetExchangeRateTool;

/// Generate a mock USD/INR rate with slight jitter around 85.42.
fn mock_exchange_rate() -> f64 {
    let mut rng = rand::thread_rng();
    let jitter: f64 = rng.gen_range(-0.30..=0.30);
    ((85.42 + jitter) * 100.0).round() / 100.0
}

#[async_trait]
impl Tool for AboundGetExchangeRateTool {
    fn name(&self) -> &str {
        "abound_get_exchange_rate"
    }

    fn description(&self) -> &str {
        "Get the current USD to INR exchange rate including the effective rate \
         after fees. Use this before initiating any wire transfer."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        let rate = mock_exchange_rate();
        let effective = ((rate - 0.32) * 100.0).round() / 100.0;

        let data = json!({
            "status": "success",
            "data": {
                "from_currency": "USD",
                "to_currency": "INR",
                "current_exchange_rate": {
                    "formatted_value": format!("{rate:.2}"),
                    "value": rate
                },
                "effective_exchange_rate": {
                    "formatted_value": format!("{effective:.2}"),
                    "value": effective
                }
            }
        });

        Ok(ToolOutput::success(data, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Tool 3: Send Wire
// ---------------------------------------------------------------------------

/// Simulates a wire transfer. Requires approval before execution.
pub struct AboundSendWireTool;

#[async_trait]
impl Tool for AboundSendWireTool {
    fn name(&self) -> &str {
        "abound_send_wire"
    }

    fn description(&self) -> &str {
        "Submit a wire transfer to send USD to an INR recipient. Requires a \
         funding source, beneficiary, amount in USD, and payment reason. \
         The transfer amount must not exceed the user's ACH limit."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "funding_source_id": {
                    "type": "string",
                    "description": "Funding source ID (e.g. 'fs_001')"
                },
                "beneficiary_ref_id": {
                    "type": "string",
                    "description": "Beneficiary reference ID (e.g. 'ben_001')"
                },
                "amount": {
                    "type": "number",
                    "description": "Amount in USD to send"
                },
                "payment_reason_key": {
                    "type": "string",
                    "description": "Payment reason key: FAMILY_MAINTENANCE, GIFT, EDUCATION_SUPPORT, or MEDICAL_SUPPORT"
                }
            },
            "required": ["funding_source_id", "beneficiary_ref_id", "amount", "payment_reason_key"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        let _funding_source = require_str(&params, "funding_source_id")?;
        let _beneficiary = require_str(&params, "beneficiary_ref_id")?;
        let _reason = require_str(&params, "payment_reason_key")?;

        let amount = params
            .get("amount")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| {
                ToolError::InvalidParameters("missing or invalid 'amount' parameter".to_string())
            })?;

        // Enforce mock ACH limit
        if amount > 5000.0 {
            let data = json!({
                "status": "error",
                "error": {
                    "code": "TRANSFER_NOT_ALLOWED",
                    "message": format!(
                        "Transfer amount ${:.2} exceeds your ACH limit of $5,000.00",
                        amount
                    )
                }
            });
            return Ok(ToolOutput::success(data, start.elapsed()));
        }

        if amount <= 0.0 {
            return Err(ToolError::InvalidParameters(
                "amount must be greater than zero".to_string(),
            ));
        }

        let txn_id = uuid::Uuid::new_v4();
        let trk_id = uuid::Uuid::new_v4();

        let data = json!({
            "status": "success",
            "data": {
                "transaction_id": format!("txn_{}", &txn_id.to_string()[..8]),
                "tracking_id": format!("trk_{}", &trk_id.to_string()[..8]),
                "amount_usd": amount,
                "completion_time": {
                    "min_calendar_days": 1,
                    "min_business_days": 1,
                    "max_calendar_days": 3,
                    "max_business_days": 2
                }
            }
        });

        Ok(ToolOutput::success(data, start.elapsed()))
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> ApprovalRequirement {
        ApprovalRequirement::UnlessAutoApproved
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Tool 4: Create Notification
// ---------------------------------------------------------------------------

/// Simulates sending a notification to the Abound system.
pub struct AboundCreateNotificationTool;

#[async_trait]
impl Tool for AboundCreateNotificationTool {
    fn name(&self) -> &str {
        "abound_create_notification"
    }

    fn description(&self) -> &str {
        "Create a notification in the Abound app (e.g. rate alert, transfer \
         confirmation, forex scoring signal). Returns 202 accepted."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "message_id": {
                    "type": "string",
                    "description": "Unique message identifier"
                },
                "action_type": {
                    "type": "string",
                    "description": "Notification type: 'notification' or 'token_refresh'"
                },
                "meta_data": {
                    "type": "object",
                    "description": "Additional metadata (e.g. score, rate, signal)"
                }
            },
            "required": ["message_id", "action_type"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        let message_id = require_str(&params, "message_id")?;

        let data = json!({
            "status": "accepted",
            "message": "Notification request accepted for processing",
            "data": {
                "message_id": message_id
            }
        });

        Ok(ToolOutput::success(data, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Tool 5: Forex Score
// ---------------------------------------------------------------------------

/// Seasonal bias factors by month (1-indexed: Jan=1 .. Dec=12).
/// High bias in Oct-Mar (favorable remittance window), low Apr-Sep.
const SEASONAL_BIAS: [f64; 13] = [
    0.0, // placeholder for 0-index
    0.75, // Jan
    0.70, // Feb
    0.65, // Mar
    0.35, // Apr
    0.30, // May
    0.25, // Jun
    0.25, // Jul
    0.30, // Aug
    0.35, // Sep
    0.70, // Oct
    0.75, // Nov
    0.65, // Dec
];

/// Computes a forex timing score for USD/INR, biased toward interesting
/// signals (60-80 range) for demo purposes.
pub struct AboundGetForexScoreTool;

#[async_trait]
impl Tool for AboundGetForexScoreTool {
    fn name(&self) -> &str {
        "abound_get_forex_score"
    }

    fn description(&self) -> &str {
        "Compute a forex timing score (0-100) for USD/INR transfers. Returns \
         a score with a signal: 'convert_now' (>=60, good time to send), \
         'split_transfer' (40-59, send half now), or 'wait' (<40, hold off). \
         Use this to advise users on optimal transfer timing."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        let mut rng = rand::thread_rng();

        // Mock current rate and MA50
        let rate = mock_exchange_rate();
        let ma50_jitter: f64 = rng.gen_range(-0.20..=0.20);
        let ma50 = ((84.50 + ma50_jitter) * 100.0).round() / 100.0;

        // Get seasonal bias for current month
        let month = Utc::now().month() as usize;
        let month_bias = SEASONAL_BIAS[month.clamp(1, 12)];

        // Scoring weights
        let w_ma = 0.7;
        let w_s = 0.3;

        // MA-based signal: how far current rate is above the 50-day average
        let ma_signal = (50.0 + ((rate - ma50) / ma50 * 100.0) * 15.0).clamp(0.0, 100.0);

        // Combined score
        let raw_score = (ma_signal * w_ma + month_bias * 100.0 * w_s) / (w_ma + w_s);

        // Bias toward 60-80 for demo (blend raw score with a favorable base)
        let demo_base = 68.0;
        let score = ((raw_score * 0.4 + demo_base * 0.6) as u32).clamp(55, 85);

        let signal = if score >= 60 {
            "convert_now"
        } else if score >= 40 {
            "split_transfer"
        } else {
            "wait"
        };

        let explanation = match signal {
            "convert_now" => format!(
                "The current USD/INR rate of {rate:.2} is above the 50-day moving average \
                 of {ma50:.2}, and seasonal trends are favorable. This is a good time to \
                 convert and send money."
            ),
            "split_transfer" => format!(
                "The rate of {rate:.2} is near the 50-day average of {ma50:.2}. Consider \
                 splitting your transfer \u{2014} send half now and hold the rest for a \
                 potentially better rate."
            ),
            _ => format!(
                "The current rate of {rate:.2} is below the 50-day average of {ma50:.2}. \
                 Unless urgent, consider waiting for a better rate."
            ),
        };

        let data = json!({
            "score": score,
            "signal": signal,
            "rate": rate,
            "ma50": ma50,
            "month_bias": month_bias,
            "explanation": explanation
        });

        Ok(ToolOutput::success(data, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::JobContext;

    fn test_ctx() -> JobContext {
        JobContext::with_user("test-user", "test", "mock abound tool test")
    }

    #[tokio::test]
    async fn account_info_returns_valid_json() {
        let tool = AboundGetAccountInfoTool;
        let result = tool.execute(json!({}), &test_ctx()).await.unwrap();
        let data = &result.result["data"];
        assert_eq!(data["user_id"], "acc_123456");
        assert_eq!(data["limits"]["ach_limit"]["limit"], 5000);
        assert_eq!(data["recipients"].as_array().unwrap().len(), 1);
        assert_eq!(data["funding_sources"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn exchange_rate_returns_valid_range() {
        let tool = AboundGetExchangeRateTool;
        let result = tool.execute(json!({}), &test_ctx()).await.unwrap();
        let rate = result.result["data"]["current_exchange_rate"]["value"]
            .as_f64()
            .unwrap();
        // Rate should be within jitter range of base 85.42
        assert!(rate > 85.0 && rate < 85.8, "rate {rate} out of range");
    }

    #[tokio::test]
    async fn send_wire_succeeds_within_limit() {
        let tool = AboundSendWireTool;
        let params = json!({
            "funding_source_id": "fs_001",
            "beneficiary_ref_id": "ben_001",
            "amount": 1000.0,
            "payment_reason_key": "FAMILY_MAINTENANCE"
        });
        let result = tool.execute(params, &test_ctx()).await.unwrap();
        assert_eq!(result.result["status"], "success");
        assert!(result.result["data"]["transaction_id"]
            .as_str()
            .unwrap()
            .starts_with("txn_"));
    }

    #[tokio::test]
    async fn send_wire_rejects_over_limit() {
        let tool = AboundSendWireTool;
        let params = json!({
            "funding_source_id": "fs_001",
            "beneficiary_ref_id": "ben_001",
            "amount": 6000.0,
            "payment_reason_key": "FAMILY_MAINTENANCE"
        });
        let result = tool.execute(params, &test_ctx()).await.unwrap();
        assert_eq!(result.result["status"], "error");
        assert_eq!(result.result["error"]["code"], "TRANSFER_NOT_ALLOWED");
    }

    #[test]
    fn send_wire_requires_approval() {
        let tool = AboundSendWireTool;
        assert!(matches!(
            tool.requires_approval(&json!({})),
            ApprovalRequirement::UnlessAutoApproved
        ));
    }

    #[tokio::test]
    async fn create_notification_returns_accepted() {
        let tool = AboundCreateNotificationTool;
        let params = json!({
            "message_id": "msg_001",
            "action_type": "notification",
            "meta_data": { "score": 72 }
        });
        let result = tool.execute(params, &test_ctx()).await.unwrap();
        assert_eq!(result.result["status"], "accepted");
    }

    #[tokio::test]
    async fn forex_score_in_demo_range() {
        let tool = AboundGetForexScoreTool;
        // Run multiple times to check range stability
        for _ in 0..20 {
            let result = tool.execute(json!({}), &test_ctx()).await.unwrap();
            let score = result.result["score"].as_u64().unwrap();
            assert!(
                (55..=85).contains(&score),
                "score {score} outside demo range [55, 85]"
            );
            let signal = result.result["signal"].as_str().unwrap();
            assert!(
                signal == "convert_now" || signal == "split_transfer" || signal == "wait",
                "unexpected signal: {signal}"
            );
        }
    }
}
