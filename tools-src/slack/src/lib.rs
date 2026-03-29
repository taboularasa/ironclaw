//! Slack WASM Tool for IronClaw.
//!
//! This is a standalone WASM component that provides Slack integration.
//! It demonstrates how to build external tools that can be dynamically
//! loaded by the agent runtime.
//!
//! # Capabilities Required
//!
//! - HTTP: `slack.com/api/*` (GET, POST)
//! - Secrets: `slack_bot_token` (injected automatically by the host)
//!
//! # Required Bot Token Scopes
//!
//! - `chat:write`       -- send messages
//! - `channels:read`    -- list public channels, get channel info/members
//! - `channels:history` -- read public channel history and thread replies
//! - `groups:read`      -- list/info private channels, get private channel members
//! - `groups:history`   -- read private channel history and thread replies
//! - `reactions:write`  -- add emoji reactions
//! - `users:read`       -- look up user info
//!
//! # Additional Scope for search_messages
//!
//! - `search:read` -- requires a **user token** (xoxp-…), not a bot token.
//!   See the `search_messages` action docs for details.
//!
//! # Supported Actions
//!
//! - `send_message`        -- Send a message (or thread reply) to a channel
//! - `list_channels`       -- List all channels the bot can see
//! - `list_joined_channels`-- List only channels where the bot is a member
//! - `get_channel_history` -- Get recent messages from a channel
//! - `get_thread_replies`  -- Get all replies in a thread (conversations.replies)
//! - `get_channel_info`    -- Get full metadata for a single channel
//! - `get_channel_members` -- Get the member list of a channel
//! - `post_reaction`       -- Add an emoji reaction to a message
//! - `get_user_info`       -- Get information about a Slack user
//! - `search_messages`     -- Full-text search across the workspace (user token required)
//!
//! # Example Usage
//!
//! ```json
//! {"action": "send_message", "channel": "#general", "text": "Hello from the agent!"}
//! {"action": "get_thread_replies", "channel": "C1234567890", "ts": "1710000000.000100"}
//! {"action": "search_messages", "query": "deployment failed", "channel": "#ops"}
//! ```

mod api;
mod types;

use types::SlackAction;

// Generate bindings from the WIT interface.
// This creates the `bindings` module with types and traits.
wit_bindgen::generate!({
    world: "sandboxed-tool",
    path: "../../wit/tool.wit",
});

/// Implementation of the tool interface.
struct SlackTool;

impl exports::near::agent::tool::Guest for SlackTool {
    fn execute(req: exports::near::agent::tool::Request) -> exports::near::agent::tool::Response {
        match execute_inner(&req.params) {
            Ok(result) => exports::near::agent::tool::Response {
                output: Some(result),
                error: None,
            },
            Err(e) => exports::near::agent::tool::Response {
                output: None,
                error: Some(e),
            },
        }
    }

    fn schema() -> String {
        r#"{
            "type": "object",
            "required": ["action"],
            "oneOf": [
                {
                    "properties": {
                        "action": {
                            "const": "send_message",
                            "description": "Send a message to a channel or thread"
                        },
                        "channel": {
                            "type": "string",
                            "description": "Channel ID or name (e.g., '#general' or 'C1234567890')"
                        },
                        "text": {
                            "type": "string",
                            "description": "Message text (supports Slack mrkdwn formatting)"
                        },
                        "thread_ts": {
                            "type": "string",
                            "description": "Optional thread timestamp to reply in a thread"
                        }
                    },
                    "required": ["action", "channel", "text"]
                },
                {
                    "properties": {
                        "action": {
                            "const": "list_channels",
                            "description": "List channels the bot can access"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of channels to return (default 100)"
                        }
                    },
                    "required": ["action"]
                },
                {
                    "properties": {
                        "action": {
                            "const": "list_joined_channels",
                            "description": "List only channels where the bot is a member"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of channels to scan before filtering (default 200)"
                        }
                    },
                    "required": ["action"]
                },
                {
                    "properties": {
                        "action": {
                            "const": "get_channel_history",
                            "description": "Get recent messages from a channel"
                        },
                        "channel": {
                            "type": "string",
                            "description": "Channel ID (for example 'C1234567890')"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of messages to return (default 20)"
                        }
                    },
                    "required": ["action", "channel"]
                },
                {
                    "properties": {
                        "action": {
                            "const": "get_thread_replies",
                            "description": "Get all replies in a thread using conversations.replies"
                        },
                        "channel": {
                            "type": "string",
                            "description": "Channel ID containing the thread"
                        },
                        "ts": {
                            "type": "string",
                            "description": "Timestamp of the parent (root) message of the thread"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of thread messages to return (default 50)"
                        }
                    },
                    "required": ["action", "channel", "ts"]
                },
                {
                    "properties": {
                        "action": {
                            "const": "get_channel_info",
                            "description": "Get full metadata for a channel"
                        },
                        "channel": {
                            "type": "string",
                            "description": "Channel ID"
                        }
                    },
                    "required": ["action", "channel"]
                },
                {
                    "properties": {
                        "action": {
                            "const": "get_channel_members",
                            "description": "Get the member list of a channel"
                        },
                        "channel": {
                            "type": "string",
                            "description": "Channel ID"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum number of member IDs to return (default 100)"
                        }
                    },
                    "required": ["action", "channel"]
                },
                {
                    "properties": {
                        "action": {
                            "const": "post_reaction",
                            "description": "Add a reaction to a message"
                        },
                        "channel": {
                            "type": "string",
                            "description": "Channel ID containing the message"
                        },
                        "timestamp": {
                            "type": "string",
                            "description": "Timestamp of the message to react to"
                        },
                        "emoji": {
                            "type": "string",
                            "description": "Emoji name without colons (for example 'thumbsup')"
                        }
                    },
                    "required": ["action", "channel", "timestamp", "emoji"]
                },
                {
                    "properties": {
                        "action": {
                            "const": "get_user_info",
                            "description": "Get information about a Slack user"
                        },
                        "user_id": {
                            "type": "string",
                            "description": "User ID (for example 'U1234567890')"
                        }
                    },
                    "required": ["action", "user_id"]
                },
                {
                    "properties": {
                        "action": {
                            "const": "search_messages",
                            "description": "Search across the workspace; requires a user token with search:read"
                        },
                        "query": {
                            "type": "string",
                            "description": "Slack search query string. Supports modifiers like 'in:#channel', 'from:@user', 'before:2024-01-01'"
                        },
                        "channel": {
                            "type": "string",
                            "description": "Optional channel name or ID filter"
                        },
                        "count": {
                            "type": "integer",
                            "description": "Maximum number of search results to return (default 20, max 100)"
                        }
                    },
                    "required": ["action", "query"]
                }
            ]
        }"#
        .to_string()
    }

    fn description() -> String {
        "Slack integration tool. Supports sending messages (including thread replies), \
         listing all channels or only joined channels, reading channel history, reading \
         full thread replies (conversations.replies -- the fix for thread readback), \
         getting channel metadata, listing channel members, adding emoji reactions, \
         looking up user info, and full-text message search across the workspace. \
         Action-specific required params: send_message(channel,text), \
         get_channel_history(channel), get_thread_replies(channel,ts), \
         get_channel_info(channel), get_channel_members(channel), \
         post_reaction(channel,timestamp,emoji), get_user_info(user_id), \
         search_messages(query[,channel]). \
         Most actions require a bot token (xoxb-) with appropriate scopes: \
         chat:write, channels:read, channels:history, groups:read, groups:history, \
         reactions:write, users:read. \
         The search_messages action requires a user token (xoxp-) with the search:read scope."
            .to_string()
    }
}

/// Inner execution logic with proper error handling.
fn execute_inner(params: &str) -> Result<String, String> {
    // Check if the Slack token is configured
    if !crate::near::agent::host::secret_exists("slack_bot_token") {
        return Err(
            "Slack bot token not configured. Please add the 'slack_bot_token' secret.".to_string(),
        );
    }

    // Parse the action from JSON
    let action: SlackAction =
        serde_json::from_str(params).map_err(|e| format!("Invalid parameters: {}", e))?;

    crate::near::agent::host::log(
        crate::near::agent::host::LogLevel::Info,
        &format!("Executing Slack action: {:?}", action),
    );

    // Dispatch to the appropriate handler
    let result = match action {
        SlackAction::SendMessage {
            channel,
            text,
            thread_ts,
        } => {
            let result = api::send_message(&channel, &text, thread_ts.as_deref())?;
            serde_json::to_string(&result).map_err(|e| e.to_string())?
        }

        SlackAction::ListChannels { limit } => {
            let result = api::list_channels(limit)?;
            serde_json::to_string(&result).map_err(|e| e.to_string())?
        }

        SlackAction::GetChannelHistory { channel, limit } => {
            let result = api::get_channel_history(&channel, limit)?;
            serde_json::to_string(&result).map_err(|e| e.to_string())?
        }

        SlackAction::PostReaction {
            channel,
            timestamp,
            emoji,
        } => {
            let result = api::post_reaction(&channel, &timestamp, &emoji)?;
            serde_json::to_string(&result).map_err(|e| e.to_string())?
        }

        SlackAction::GetUserInfo { user_id } => {
            let result = api::get_user_info(&user_id)?;
            serde_json::to_string(&result).map_err(|e| e.to_string())?
        }

        // ── New actions ──────────────────────────────────────────────────────

        SlackAction::GetThreadReplies { channel, ts, limit } => {
            let result = api::get_thread_replies(&channel, &ts, limit)?;
            serde_json::to_string(&result).map_err(|e| e.to_string())?
        }

        SlackAction::SearchMessages {
            query,
            channel,
            count,
        } => {
            // NOTE: search_messages requires a user token (xoxp-) with the
            // `search:read` scope. The host injects the token automatically,
            // but the token configured in secrets must be a user token for
            // this action to succeed. Bot tokens will receive
            // `error: "not_allowed_token_type"` from the Slack API.
            let result = api::search_messages(&query, channel.as_deref(), count)?;
            serde_json::to_string(&result).map_err(|e| e.to_string())?
        }

        SlackAction::GetChannelMembers { channel, limit } => {
            let result = api::get_channel_members(&channel, limit)?;
            serde_json::to_string(&result).map_err(|e| e.to_string())?
        }

        SlackAction::ListJoinedChannels { limit } => {
            let result = api::list_joined_channels(limit)?;
            serde_json::to_string(&result).map_err(|e| e.to_string())?
        }

        SlackAction::GetChannelInfo { channel } => {
            let result = api::get_channel_info(&channel)?;
            serde_json::to_string(&result).map_err(|e| e.to_string())?
        }
    };

    Ok(result)
}

// Export the tool implementation.
export!(SlackTool);
