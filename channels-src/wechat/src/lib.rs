use std::collections::HashSet;

wit_bindgen::generate!({
    world: "sandboxed-channel",
    path: "../../wit/channel.wit",
});

mod api;
mod auth;
mod media;
mod state;
mod types;

use exports::near::agent::channel::{
    AgentResponse, ChannelConfig, Guest, PollConfig, StatusType, StatusUpdate,
};
use near::agent::channel_host::{self, EmittedMessage};
use serde_json::json;

use crate::auth::TOKEN_SECRET_NAME;
use crate::state::{
    load_config, load_context_tokens, load_get_updates_buf, load_pending_inbound_bundles,
    load_typing_tickets, persist_config, persist_context_tokens, persist_get_updates_buf,
    persist_pending_inbound_bundles, persist_typing_tickets, PendingInboundBundle,
    StoredInboundAttachment, TypingTicketEntry,
};
use crate::types::{
    OutboundMetadata, WechatConfig, WechatMessage, MESSAGE_ITEM_TEXT, MESSAGE_TYPE_USER,
    TYPING_STATUS_CANCEL, TYPING_STATUS_TYPING,
};

const TYPING_TICKET_TTL_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WechatStatusAction {
    Typing,
    Cancel,
}

struct WechatChannel;

fn log_channel(level: channel_host::LogLevel, message: &str) {
    #[cfg(not(test))]
    channel_host::log(level, message);

    #[cfg(test)]
    {
        let _ = level;
        let _ = message;
    }
}

impl Guest for WechatChannel {
    fn on_start(config_json: String) -> Result<ChannelConfig, String> {
        let config = serde_json::from_str::<WechatConfig>(&config_json)
            .map_err(|e| format!("Failed to parse WeChat config: {e}"))?;
        persist_config(&config)?;

        Ok(ChannelConfig {
            display_name: "WeChat".to_string(),
            http_endpoints: Vec::new(),
            poll: Some(PollConfig {
                interval_ms: config.poll_interval_ms.max(30_000),
                enabled: true,
            }),
        })
    }

    fn on_http_request(
        _req: exports::near::agent::channel::IncomingHttpRequest,
    ) -> exports::near::agent::channel::OutgoingHttpResponse {
        exports::near::agent::channel::OutgoingHttpResponse {
            status: 404,
            headers_json: "{}".to_string(),
            body: b"{\"error\":\"wechat channel does not expose webhooks\"}".to_vec(),
        }
    }

    fn on_poll() {
        if !channel_host::secret_exists(TOKEN_SECRET_NAME) {
            channel_host::log(
                channel_host::LogLevel::Warn,
                "WeChat bot token is missing; skipping poll",
            );
            return;
        }

        let config = load_config();
        let cursor = load_get_updates_buf();
        let mut context_tokens = load_context_tokens();
        let mut pending_inbound = match load_pending_inbound_bundles() {
            Ok(bundles) => bundles,
            Err(error) => {
                channel_host::log(
                    channel_host::LogLevel::Error,
                    &format!("Failed to load WeChat pending inbound bundles: {error}"),
                );
                return;
            }
        };
        let carried_pending_keys: HashSet<String> = pending_inbound.keys().cloned().collect();
        let mut pending_inbound_changed = false;

        match api::get_updates(&config, &cursor) {
            Ok(response) => {
                if response.errcode == Some(-14) {
                    channel_host::log(
                        channel_host::LogLevel::Error,
                        "WeChat getUpdates returned errcode=-14; reconnect the channel",
                    );
                    return;
                }

                if response.ret.unwrap_or(0) != 0 {
                    let errmsg = response
                        .errmsg
                        .as_deref()
                        .unwrap_or("unknown WeChat polling error");
                    channel_host::log(
                        channel_host::LogLevel::Warn,
                        &format!(
                            "WeChat getUpdates returned ret={} errmsg={errmsg}",
                            response.ret.unwrap_or(-1)
                        ),
                    );
                }

                if let Some(next_cursor) = response.get_updates_buf.as_deref() {
                    if next_cursor != cursor {
                        if let Err(error) = persist_get_updates_buf(next_cursor) {
                            channel_host::log(
                                channel_host::LogLevel::Warn,
                                &format!("Failed to persist WeChat polling cursor: {error}"),
                            );
                        }
                    }
                }

                let mut context_tokens_changed = false;
                for message in response.msgs {
                    if let Some(from_user_id) = message.from_user_id.as_deref() {
                        if let Some(context_token) = message.context_token.as_deref() {
                            let changed = context_tokens
                                .insert(from_user_id.to_string(), context_token.to_string())
                                .as_deref()
                                != Some(context_token);
                            context_tokens_changed |= changed;
                        }
                    }
                    match incoming_bundle_from_message(&config, message) {
                        Ok(Some(bundle)) => {
                            let emitted = process_incoming_bundle(
                                &mut pending_inbound,
                                bundle,
                                &mut pending_inbound_changed,
                            );
                            for emitted_bundle in emitted {
                                emit_buffered_bundle(emitted_bundle);
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            channel_host::log(
                                channel_host::LogLevel::Error,
                                &format!("Failed to map WeChat inbound message: {error}"),
                            );
                        }
                    }
                }

                for key in carried_pending_keys {
                    if let Some(bundle) = pending_inbound.remove(&key) {
                        pending_inbound_changed = true;
                        log_channel(
                            channel_host::LogLevel::Info,
                            &format!(
                                "Flushing buffered WeChat image-only message for {} after waiting one poll cycle",
                                bundle.from_user_id
                            ),
                        );
                        emit_buffered_bundle(bundle);
                    }
                }

                if context_tokens_changed {
                    if let Err(error) = persist_context_tokens(&context_tokens) {
                        channel_host::log(
                            channel_host::LogLevel::Warn,
                            &format!("Failed to persist WeChat context tokens: {error}"),
                        );
                    }
                }

                if pending_inbound_changed {
                    if let Err(error) = persist_pending_inbound_bundles(&pending_inbound) {
                        channel_host::log(
                            channel_host::LogLevel::Warn,
                            &format!("Failed to persist WeChat pending inbound bundles: {error}"),
                        );
                    }
                }
            }
            Err(error) => {
                channel_host::log(
                    channel_host::LogLevel::Error,
                    &format!("WeChat polling failed: {error}"),
                );
            }
        }
    }

    fn on_respond(response: AgentResponse) -> Result<(), String> {
        let metadata = serde_json::from_str::<OutboundMetadata>(&response.metadata_json)
            .map_err(|e| format!("Invalid WeChat response metadata: {e}"))?;
        let config = load_config();
        let context_tokens = load_context_tokens();
        let context_token = metadata
            .context_token
            .clone()
            .or_else(|| context_tokens.get(&metadata.from_user_id).cloned());
        if let Err(error) = send_typing_indicator(
            &config,
            &metadata,
            context_token.as_deref(),
            TYPING_STATUS_CANCEL,
            false,
        ) {
            channel_host::log(
                channel_host::LogLevel::Debug,
                &format!("Failed to cancel WeChat typing indicator before reply: {error}"),
            );
        }

        send_response(&config, &metadata, &response, context_token.as_deref())
    }

    fn on_status(update: StatusUpdate) {
        let Some(action) = classify_status_update(&update) else {
            return;
        };
        let metadata = match serde_json::from_str::<OutboundMetadata>(&update.metadata_json) {
            Ok(metadata) => metadata,
            Err(_) => {
                channel_host::log(
                    channel_host::LogLevel::Debug,
                    "on_status: no valid WeChat metadata, skipping typing update",
                );
                return;
            }
        };
        let config = load_config();
        let context_tokens = load_context_tokens();
        let context_token = resolve_context_token(&metadata, &context_tokens);

        let (typing_status, allow_ticket_fetch) = match action {
            WechatStatusAction::Typing => (TYPING_STATUS_TYPING, true),
            WechatStatusAction::Cancel => (TYPING_STATUS_CANCEL, false),
        };

        if let Err(error) = send_typing_indicator(
            &config,
            &metadata,
            context_token.as_deref(),
            typing_status,
            allow_ticket_fetch,
        ) {
            channel_host::log(
                channel_host::LogLevel::Debug,
                &format!("WeChat typing update failed: {error}"),
            );
        }
    }

    fn on_broadcast(_user_id: String, _response: AgentResponse) -> Result<(), String> {
        Ok(())
    }

    fn on_shutdown() {}
}

fn incoming_bundle_from_message(
    config: &WechatConfig,
    message: WechatMessage,
) -> Result<Option<PendingInboundBundle>, String> {
    if message.message_type != Some(MESSAGE_TYPE_USER) {
        return Ok(None);
    }

    let from_user_id = match message.from_user_id.as_deref() {
        Some(user_id) => user_id,
        None => return Ok(None),
    };

    let text = extract_text(&message);
    let attachments = media::extract_image_attachments(config, &message)?
        .into_iter()
        .map(StoredInboundAttachment::from)
        .collect::<Vec<_>>();
    if text.trim().is_empty() && attachments.is_empty() {
        return Ok(None);
    }

    Ok(Some(PendingInboundBundle {
        from_user_id: from_user_id.to_string(),
        to_user_id: message.to_user_id,
        session_id: message.session_id,
        context_token: message.context_token,
        message_id: message.message_id,
        text,
        attachments,
    }))
}

fn process_incoming_bundle(
    pending_inbound: &mut std::collections::HashMap<String, PendingInboundBundle>,
    bundle: PendingInboundBundle,
    pending_inbound_changed: &mut bool,
) -> Vec<PendingInboundBundle> {
    let key = bundle.from_user_id.clone();
    let bundle_has_text = !bundle.text.trim().is_empty();
    let bundle_has_attachments = !bundle.attachments.is_empty();

    if let Some(mut pending) = pending_inbound.remove(&key) {
        *pending_inbound_changed = true;

        if bundle_has_text {
            let incoming_metadata = bundle.clone();
            pending.text = merge_text(&pending.text, &bundle.text);
            pending.attachments.extend(bundle.attachments);
            merge_bundle_metadata(&mut pending, &incoming_metadata);
            log_channel(
                channel_host::LogLevel::Info,
                &format!(
                    "Merged buffered WeChat attachment message with follow-up text for {}",
                    pending.from_user_id
                ),
            );
            return vec![pending];
        }

        let incoming_metadata = bundle.clone();
        pending.attachments.extend(bundle.attachments);
        merge_bundle_metadata(&mut pending, &incoming_metadata);
        pending_inbound.insert(key, pending);
        log_channel(
            channel_host::LogLevel::Info,
            &format!(
                "Buffered additional WeChat attachment for {} while waiting for follow-up text",
                bundle.from_user_id
            ),
        );
        return Vec::new();
    }

    if bundle_has_attachments && !bundle_has_text {
        *pending_inbound_changed = true;
        log_channel(
            channel_host::LogLevel::Info,
            &format!(
                "Buffered WeChat image-only message for {} and will wait one poll cycle for follow-up text",
                bundle.from_user_id
            ),
        );
        pending_inbound.insert(key, bundle);
        Vec::new()
    } else {
        vec![bundle]
    }
}

fn emit_buffered_bundle(bundle: PendingInboundBundle) {
    let metadata = json!({
        "from_user_id": bundle.from_user_id,
        "to_user_id": bundle.to_user_id,
        "message_id": bundle.message_id,
        "session_id": bundle.session_id,
        "context_token": bundle.context_token,
    });

    channel_host::emit_message(&EmittedMessage {
        user_id: bundle.from_user_id.clone(),
        user_name: None,
        content: bundle.text,
        thread_id: Some(format!("wechat:{}", bundle.from_user_id)),
        metadata_json: metadata.to_string(),
        attachments: bundle.attachments.into_iter().map(Into::into).collect(),
    });
}

fn merge_bundle_metadata(target: &mut PendingInboundBundle, incoming: &PendingInboundBundle) {
    if incoming.to_user_id.is_some() {
        target.to_user_id = incoming.to_user_id.clone();
    }
    if incoming.session_id.is_some() {
        target.session_id = incoming.session_id.clone();
    }
    if incoming.context_token.is_some() {
        target.context_token = incoming.context_token.clone();
    }
    if incoming.message_id.is_some() {
        target.message_id = incoming.message_id;
    }
}

fn merge_text(existing: &str, incoming: &str) -> String {
    let existing = existing.trim();
    let incoming = incoming.trim();
    match (existing.is_empty(), incoming.is_empty()) {
        (true, true) => String::new(),
        (true, false) => incoming.to_string(),
        (false, true) => existing.to_string(),
        (false, false) => format!("{existing}\n{incoming}"),
    }
}

fn send_response(
    config: &WechatConfig,
    metadata: &OutboundMetadata,
    response: &AgentResponse,
    context_token: Option<&str>,
) -> Result<(), String> {
    let mut remaining_text = response.content.trim().to_string();
    let mut sent_attachment = false;

    for attachment in &response.attachments {
        if !attachment.mime_type.starts_with("image/") {
            return Err(format!(
                "WeChat currently supports image attachments only, got {} ({})",
                attachment.filename, attachment.mime_type
            ));
        }

        let caption = if sent_attachment {
            ""
        } else {
            remaining_text.as_str()
        };
        media::send_image_attachment(
            config,
            &metadata.from_user_id,
            attachment,
            context_token,
            caption,
        )?;
        sent_attachment = true;
        remaining_text.clear();
    }

    if !remaining_text.is_empty() || !sent_attachment {
        api::send_text_message(
            config,
            &metadata.from_user_id,
            &remaining_text,
            context_token,
        )?;
    }

    Ok(())
}

fn extract_text(message: &WechatMessage) -> String {
    message
        .item_list
        .iter()
        .find_map(|item| {
            if item.r#type == Some(MESSAGE_ITEM_TEXT) {
                item.text_item.as_ref().map(|item| item.text.clone())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

fn is_terminal_text_status(message: &str) -> bool {
    let trimmed = message.trim();
    trimmed.eq_ignore_ascii_case("done")
        || trimmed.eq_ignore_ascii_case("interrupted")
        || trimmed.eq_ignore_ascii_case("awaiting approval")
        || trimmed.eq_ignore_ascii_case("rejected")
}

fn classify_status_update(update: &StatusUpdate) -> Option<WechatStatusAction> {
    match update.status {
        StatusType::Thinking => Some(WechatStatusAction::Typing),
        StatusType::Done
        | StatusType::Interrupted
        | StatusType::ApprovalNeeded
        | StatusType::AuthRequired => Some(WechatStatusAction::Cancel),
        StatusType::Status if is_terminal_text_status(&update.message) => {
            Some(WechatStatusAction::Cancel)
        }
        StatusType::ToolStarted
        | StatusType::ToolCompleted
        | StatusType::ToolResult
        | StatusType::Status
        | StatusType::JobStarted
        | StatusType::AuthCompleted => None,
    }
}

fn resolve_context_token(
    metadata: &OutboundMetadata,
    context_tokens: &std::collections::HashMap<String, String>,
) -> Option<String> {
    metadata
        .context_token
        .clone()
        .or_else(|| context_tokens.get(&metadata.from_user_id).cloned())
}

fn cached_typing_ticket(user_id: &str) -> Option<String> {
    let tickets = load_typing_tickets();
    let ticket = tickets.get(user_id)?;
    let trimmed = ticket.ticket.trim();
    if trimmed.is_empty() {
        return None;
    }

    let age_ms = channel_host::now_millis().saturating_sub(ticket.fetched_at_ms);
    if age_ms >= TYPING_TICKET_TTL_MS {
        return None;
    }

    Some(trimmed.to_string())
}

fn persist_typing_ticket(user_id: &str, ticket: &str) -> Result<(), String> {
    let mut tickets = load_typing_tickets();
    tickets.insert(
        user_id.to_string(),
        TypingTicketEntry {
            ticket: ticket.to_string(),
            fetched_at_ms: channel_host::now_millis(),
        },
    );
    persist_typing_tickets(&tickets)
}

fn clear_typing_ticket(user_id: &str) -> Result<(), String> {
    let mut tickets = load_typing_tickets();
    if tickets.remove(user_id).is_some() {
        persist_typing_tickets(&tickets)?;
    }
    Ok(())
}

fn resolve_typing_ticket(
    config: &WechatConfig,
    user_id: &str,
    context_token: Option<&str>,
) -> Result<Option<String>, String> {
    if let Some(ticket) = cached_typing_ticket(user_id) {
        return Ok(Some(ticket));
    }

    let response = api::get_config(config, user_id, context_token)?;
    if response.ret.unwrap_or(0) != 0 {
        let errmsg = response
            .errmsg
            .as_deref()
            .unwrap_or("unknown WeChat getConfig error");
        return Err(format!(
            "WeChat getConfig returned ret={} errmsg={errmsg}",
            response.ret.unwrap_or(-1)
        ));
    }

    let Some(ticket) = response
        .typing_ticket
        .as_deref()
        .map(str::trim)
        .filter(|ticket| !ticket.is_empty())
    else {
        return Ok(None);
    };

    if let Err(error) = persist_typing_ticket(user_id, ticket) {
        channel_host::log(
            channel_host::LogLevel::Warn,
            &format!("Failed to persist WeChat typing ticket: {error}"),
        );
    }

    Ok(Some(ticket.to_string()))
}

fn send_typing_indicator(
    config: &WechatConfig,
    metadata: &OutboundMetadata,
    context_token: Option<&str>,
    status: i32,
    allow_ticket_fetch: bool,
) -> Result<(), String> {
    let ticket = if allow_ticket_fetch {
        resolve_typing_ticket(config, &metadata.from_user_id, context_token)?
    } else {
        cached_typing_ticket(&metadata.from_user_id)
    };

    let Some(ticket) = ticket else {
        return Ok(());
    };

    if let Err(error) = api::send_typing(config, &metadata.from_user_id, &ticket, status) {
        let _ = clear_typing_ticket(&metadata.from_user_id);
        return Err(error);
    }

    Ok(())
}

export!(WechatChannel);

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        classify_status_update, merge_text, process_incoming_bundle, PendingInboundBundle,
        StoredInboundAttachment, WechatStatusAction,
    };
    use crate::exports::near::agent::channel::{StatusType, StatusUpdate};

    fn make_bundle(user_id: &str, text: &str, image_count: usize) -> PendingInboundBundle {
        PendingInboundBundle {
            from_user_id: user_id.to_string(),
            to_user_id: Some("bot".to_string()),
            session_id: Some("session-1".to_string()),
            context_token: Some("ctx-1".to_string()),
            message_id: Some(1),
            text: text.to_string(),
            attachments: (0..image_count)
                .map(|index| StoredInboundAttachment {
                    id: format!("att-{index}"),
                    mime_type: "image/jpeg".to_string(),
                    filename: Some(format!("photo-{index}.jpg")),
                    size_bytes: Some(128),
                    source_url: Some("https://example.com/image.jpg".to_string()),
                    storage_key: None,
                    extracted_text: None,
                    extras_json: "{}".to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn test_classify_status_update_thinking_starts_typing() {
        let update = StatusUpdate {
            status: StatusType::Thinking,
            message: "Thinking...".to_string(),
            metadata_json: "{}".to_string(),
        };

        assert_eq!(
            classify_status_update(&update),
            Some(WechatStatusAction::Typing)
        );
    }

    #[test]
    fn test_classify_status_update_done_cancels_typing() {
        let update = StatusUpdate {
            status: StatusType::Done,
            message: "Done".to_string(),
            metadata_json: "{}".to_string(),
        };

        assert_eq!(
            classify_status_update(&update),
            Some(WechatStatusAction::Cancel)
        );
    }

    #[test]
    fn test_classify_status_update_approval_needed_cancels_typing() {
        let update = StatusUpdate {
            status: StatusType::ApprovalNeeded,
            message: "Approval needed".to_string(),
            metadata_json: "{}".to_string(),
        };

        assert_eq!(
            classify_status_update(&update),
            Some(WechatStatusAction::Cancel)
        );
    }

    #[test]
    fn test_classify_status_update_tool_started_is_ignored() {
        let update = StatusUpdate {
            status: StatusType::ToolStarted,
            message: "Tool started".to_string(),
            metadata_json: "{}".to_string(),
        };

        assert_eq!(classify_status_update(&update), None);
    }

    #[test]
    fn test_classify_status_update_terminal_text_status_cancels_typing() {
        let update = StatusUpdate {
            status: StatusType::Status,
            message: "Awaiting approval".to_string(),
            metadata_json: "{}".to_string(),
        };

        assert_eq!(
            classify_status_update(&update),
            Some(WechatStatusAction::Cancel)
        );
    }

    #[test]
    fn test_classify_status_update_progress_status_is_ignored() {
        let update = StatusUpdate {
            status: StatusType::Status,
            message: "Context compaction started".to_string(),
            metadata_json: "{}".to_string(),
        };

        assert_eq!(classify_status_update(&update), None);
    }

    #[test]
    fn test_merge_text_joins_non_empty_segments() {
        assert_eq!(merge_text("", "hello"), "hello");
        assert_eq!(merge_text("look", "what is this"), "look\nwhat is this");
        assert_eq!(merge_text("look", ""), "look");
    }

    #[test]
    fn test_process_incoming_bundle_merges_buffered_image_with_follow_up_text() {
        let mut pending = HashMap::new();
        let mut changed = false;

        let emitted = process_incoming_bundle(&mut pending, make_bundle("u1", "", 1), &mut changed);
        assert!(emitted.is_empty());
        assert!(changed);
        assert_eq!(pending.len(), 1);

        changed = false;
        let emitted = process_incoming_bundle(
            &mut pending,
            make_bundle("u1", "What is in this image?", 0),
            &mut changed,
        );
        assert!(changed);
        assert!(pending.is_empty());
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].text, "What is in this image?");
        assert_eq!(emitted[0].attachments.len(), 1);
    }

    #[test]
    fn test_process_incoming_bundle_emits_text_and_images_together_without_buffering() {
        let mut pending = HashMap::new();
        let mut changed = false;

        let emitted = process_incoming_bundle(
            &mut pending,
            make_bundle("u1", "Look at this image", 1),
            &mut changed,
        );
        assert!(!changed);
        assert!(pending.is_empty());
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].text, "Look at this image");
        assert_eq!(emitted[0].attachments.len(), 1);
    }
}
