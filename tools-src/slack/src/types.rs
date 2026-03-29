//! Types for Slack API requests and responses.

use serde::{Deserialize, Serialize};

/// Input parameters for the Slack tool.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SlackAction {
    /// Send a message to a channel.
    SendMessage {
        /// Channel ID or name (e.g., "#general" or "C1234567890").
        channel: String,
        /// Message text (supports Slack mrkdwn formatting).
        text: String,
        /// Optional thread timestamp to reply in a thread.
        #[serde(default)]
        thread_ts: Option<String>,
    },

    /// List channels the bot has access to.
    ListChannels {
        /// Maximum number of channels to return (default: 100).
        #[serde(default = "default_limit")]
        limit: u32,
    },

    /// Get message history from a channel.
    GetChannelHistory {
        /// Channel ID (e.g., "C1234567890").
        channel: String,
        /// Maximum number of messages to return (default: 20).
        #[serde(default = "default_history_limit")]
        limit: u32,
    },

    /// Add a reaction (emoji) to a message.
    PostReaction {
        /// Channel ID containing the message.
        channel: String,
        /// Timestamp of the message to react to.
        timestamp: String,
        /// Emoji name without colons (e.g., "thumbsup").
        emoji: String,
    },

    /// Get information about a user.
    GetUserInfo {
        /// User ID (e.g., "U1234567890").
        user_id: String,
    },

    /// Get all replies in a thread.
    ///
    /// Calls `conversations.replies`. This is the correct way to read back an
    /// entire thread -- `conversations.history` only returns the root message,
    /// not the replies. Use the timestamp (`ts`) of the parent message as `ts`.
    GetThreadReplies {
        /// Channel ID containing the thread (e.g., "C1234567890").
        channel: String,
        /// Timestamp of the parent (root) message of the thread.
        ts: String,
        /// Maximum number of messages to return, including the root (default: 50).
        #[serde(default = "default_thread_limit")]
        limit: u32,
    },

    /// Search for messages across the workspace.
    ///
    /// Calls `search.messages`.
    ///
    /// NOTE: This action requires a **user token** (xoxp-…) with the
    /// `search:read` scope, NOT a bot token. Bot tokens cannot call
    /// `search.messages`. Ensure the configured secret is a user token when
    /// using this action.
    SearchMessages {
        /// Slack search query string (supports modifiers like `in:#channel`,
        /// `from:@user`, `before:2024-01-01`, etc.).
        query: String,
        /// Optional channel name or ID to restrict the search (appended to
        /// the query as `in:<channel>`).
        #[serde(default)]
        channel: Option<String>,
        /// Maximum number of results to return (default: 20, max: 100).
        #[serde(default = "default_history_limit")]
        count: u32,
    },

    /// Get the member list of a channel.
    ///
    /// Calls `conversations.members` and returns Slack user IDs. For large
    /// channels this may be a long list; pagination is not yet supported so
    /// the result is capped by `limit`.
    GetChannelMembers {
        /// Channel ID (e.g., "C1234567890").
        channel: String,
        /// Maximum number of member IDs to return (default: 100).
        #[serde(default = "default_limit")]
        limit: u32,
    },

    /// List only the channels the bot is a member of.
    ///
    /// Like `list_channels` but filters `conversations.list` results to those
    /// where `is_member == true`. Useful for agents that should only operate
    /// in channels they have been explicitly invited to.
    ListJoinedChannels {
        /// Maximum number of channels to return before filtering (default: 200).
        /// The actual result count may be lower after filtering.
        #[serde(default = "default_joined_limit")]
        limit: u32,
    },

    /// Get full metadata for a single channel.
    ///
    /// Calls `conversations.info`. Returns richer data than `list_channels`,
    /// including member count, creation time, and the full topic/purpose
    /// strings.
    GetChannelInfo {
        /// Channel ID (e.g., "C1234567890").
        channel: String,
    },
}

// ── Default value helpers ────────────────────────────────────────────────────

fn default_limit() -> u32 {
    100
}

fn default_history_limit() -> u32 {
    20
}

fn default_thread_limit() -> u32 {
    50
}

fn default_joined_limit() -> u32 {
    200
}

// ── Existing response types ──────────────────────────────────────────────────

/// Result from send_message.
#[derive(Debug, Serialize)]
pub struct SendMessageResult {
    pub ok: bool,
    pub channel: String,
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<MessageInfo>,
}

/// Basic message info.
#[derive(Debug, Serialize)]
pub struct MessageInfo {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub ts: String,
}

/// A Slack channel (summary view, as returned by `conversations.list`).
#[derive(Debug, Serialize)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub is_private: bool,
    pub is_member: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

/// Result from list_channels and list_joined_channels.
#[derive(Debug, Serialize)]
pub struct ListChannelsResult {
    pub ok: bool,
    pub channels: Vec<Channel>,
}

/// Result from get_channel_history.
#[derive(Debug, Serialize)]
pub struct ChannelHistoryResult {
    pub ok: bool,
    pub messages: Vec<HistoryMessage>,
}

/// A message from channel history.
#[derive(Debug, Serialize)]
pub struct HistoryMessage {
    pub ts: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(rename = "type")]
    pub msg_type: String,
}

/// Result from post_reaction.
#[derive(Debug, Serialize)]
pub struct PostReactionResult {
    pub ok: bool,
}

/// User information.
#[derive(Debug, Serialize)]
pub struct UserInfo {
    pub id: String,
    pub name: String,
    pub real_name: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub is_bot: bool,
}

/// Result from get_user_info.
#[derive(Debug, Serialize)]
pub struct GetUserInfoResult {
    pub ok: bool,
    pub user: UserInfo,
}

// ── New response types ───────────────────────────────────────────────────────

/// A single message in a thread, as returned by `conversations.replies`.
#[derive(Debug, Serialize)]
pub struct ThreadMessage {
    /// Slack timestamp that uniquely identifies this message.
    pub ts: String,
    /// Message body text.
    pub text: String,
    /// User ID of the sender (`None` for bot/app messages without a user).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Slack message type (almost always `"message"`).
    #[serde(rename = "type")]
    pub msg_type: String,
    /// `true` when this is the root (parent) message of the thread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_root: Option<bool>,
}

/// Result from get_thread_replies.
#[derive(Debug, Serialize)]
pub struct GetThreadRepliesResult {
    pub ok: bool,
    /// All messages in the thread, starting with the root message.
    pub messages: Vec<ThreadMessage>,
    /// Total number of replies (not counting the root message), as reported
    /// by Slack. May be larger than `messages.len()` if `limit` was reached.
    pub reply_count: u32,
}

/// A single search match as returned by `search.messages`.
#[derive(Debug, Serialize)]
pub struct SearchMatch {
    /// Unique message timestamp.
    pub ts: String,
    /// Message body text.
    pub text: String,
    /// Slack user ID of the author.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Username of the author (display name at time of message).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Channel the message belongs to.
    pub channel_id: String,
    pub channel_name: String,
    /// If the message is a thread reply, the timestamp of the parent message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    /// Permanent link to this message in the Slack web client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permalink: Option<String>,
}

/// Result from search_messages.
#[derive(Debug, Serialize)]
pub struct SearchMessagesResult {
    pub ok: bool,
    pub query: String,
    pub total: u32,
    pub matches: Vec<SearchMatch>,
}

/// Result from get_channel_members.
#[derive(Debug, Serialize)]
pub struct GetChannelMembersResult {
    pub ok: bool,
    pub channel: String,
    /// Slack user IDs of channel members.
    pub members: Vec<String>,
}

/// Full channel metadata as returned by `conversations.info`.
#[derive(Debug, Serialize)]
pub struct ChannelInfo {
    pub id: String,
    pub name: String,
    pub is_private: bool,
    pub is_member: bool,
    /// Number of members in the channel (may be absent for very large channels).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_members: Option<u32>,
    /// Unix timestamp of channel creation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// Whether the channel has been archived.
    pub is_archived: bool,
}

/// Result from get_channel_info.
#[derive(Debug, Serialize)]
pub struct GetChannelInfoResult {
    pub ok: bool,
    pub channel: ChannelInfo,
}
