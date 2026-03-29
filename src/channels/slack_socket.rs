//! Native Slack channel using Socket Mode for long-lived connectivity.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use rand::Rng;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::channels::{
    AttachmentKind, Channel, IncomingAttachment, IncomingMessage, MessageStream, OutgoingResponse,
    StatusUpdate,
};
use crate::config::SlackSocketConfig;
use crate::error::ChannelError;

const CHANNEL_NAME: &str = "slack";
const MAX_DOWNLOAD_SIZE_BYTES: u64 = 20 * 1024 * 1024;
const STATUS_CACHE_LIMIT: usize = 4096;
const TYPING_CACHE_LIMIT: usize = 512;
const TYPING_INTERVAL: Duration = Duration::from_secs(3);
const SOCKET_BACKOFF_MIN: Duration = Duration::from_secs(2);
const SOCKET_BACKOFF_MAX: Duration = Duration::from_secs(60);
const THREAD_CONTEXT_MESSAGE_LIMIT: usize = 8;
const THREAD_CONTEXT_TEXT_LIMIT: usize = 280;

type SlackSocketStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SlackMessageMetadata {
    channel: String,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    placeholder_ts: Option<String>,
    #[serde(default)]
    message_ts: String,
    #[serde(default)]
    team_id: Option<String>,
    #[serde(default)]
    sender_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackSocketOpenResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackApiResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackPostMessageResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    ts: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackRepliesResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    messages: Vec<SlackReplyMessage>,
}

#[derive(Debug, Clone, Deserialize)]
struct SlackReplyMessage {
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackAuthTestResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackSocketEnvelope {
    #[serde(rename = "type")]
    envelope_type: String,
    #[serde(default)]
    envelope_id: Option<String>,
    #[serde(default)]
    payload: Option<SlackSocketPayload>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackSocketPayload {
    #[serde(default)]
    team_id: Option<String>,
    #[serde(default)]
    event_id: Option<String>,
    #[serde(default)]
    event: Option<SlackEvent>,
}

#[derive(Debug, Clone, Deserialize)]
struct SlackEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    files: Option<Vec<SlackFile>>,
}

#[derive(Debug, Clone, Deserialize)]
struct SlackFile {
    id: String,
    #[serde(default)]
    mimetype: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    url_private: Option<String>,
}

struct SlackSocketState {
    last_status_by_placeholder: Mutex<HashMap<String, String>>,
    last_typing_by_channel: Mutex<HashMap<String, Instant>>,
    listener_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    shutdown_tx: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct SlackSocketChannel {
    config: SlackSocketConfig,
    client: Client,
    state: Arc<SlackSocketState>,
}

enum ConnectionOutcome {
    Reconnect,
    Shutdown,
}

impl SlackSocketChannel {
    pub fn new(config: SlackSocketConfig) -> Result<Self, ChannelError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| ChannelError::Http(e.to_string()))?;
        let (shutdown_tx, _) = watch::channel(false);

        Ok(Self {
            config,
            client,
            state: Arc::new(SlackSocketState {
                last_status_by_placeholder: Mutex::new(HashMap::new()),
                last_typing_by_channel: Mutex::new(HashMap::new()),
                listener_task: Mutex::new(None),
                shutdown_tx,
            }),
        })
    }

    async fn open_socket_url(&self) -> Result<String, ChannelError> {
        let response = self
            .client
            .post("https://slack.com/api/apps.connections.open")
            .bearer_auth(&self.config.app_token)
            .send()
            .await
            .map_err(|e| ChannelError::AuthFailed {
                name: CHANNEL_NAME.to_string(),
                reason: format!("apps.connections.open failed: {e}"),
            })?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| ChannelError::AuthFailed {
                name: CHANNEL_NAME.to_string(),
                reason: format!("apps.connections.open body read failed: {e}"),
            })?;

        if !status.is_success() {
            return Err(ChannelError::AuthFailed {
                name: CHANNEL_NAME.to_string(),
                reason: format!("apps.connections.open returned {status}: {body}"),
            });
        }

        let parsed: SlackSocketOpenResponse =
            serde_json::from_str(&body).map_err(|e| ChannelError::AuthFailed {
                name: CHANNEL_NAME.to_string(),
                reason: format!("invalid apps.connections.open response: {e}"),
            })?;

        if !parsed.ok {
            return Err(ChannelError::AuthFailed {
                name: CHANNEL_NAME.to_string(),
                reason: parsed
                    .error
                    .unwrap_or_else(|| "apps.connections.open returned ok=false".to_string()),
            });
        }

        parsed.url.ok_or_else(|| ChannelError::AuthFailed {
            name: CHANNEL_NAME.to_string(),
            reason: "apps.connections.open response missing websocket url".to_string(),
        })
    }

    async fn socket_listener(
        self,
        tx: mpsc::Sender<IncomingMessage>,
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> Result<(), ChannelError> {
        let mut retry_delay = SOCKET_BACKOFF_MIN;

        loop {
            if *shutdown_rx.borrow() {
                return Ok(());
            }

            let socket_url = match self.open_socket_url().await {
                Ok(url) => url,
                Err(e) => {
                    tracing::warn!(error = %e, "Slack Socket Mode URL request failed");
                    self.sleep_with_backoff(&mut shutdown_rx, retry_delay)
                        .await?;
                    retry_delay = next_backoff(retry_delay);
                    continue;
                }
            };

            let socket = match connect_async(socket_url.as_str()).await {
                Ok((socket, _)) => socket,
                Err(e) => {
                    tracing::warn!(error = %e, "Slack Socket Mode websocket connect failed");
                    self.sleep_with_backoff(&mut shutdown_rx, retry_delay)
                        .await?;
                    retry_delay = next_backoff(retry_delay);
                    continue;
                }
            };

            retry_delay = SOCKET_BACKOFF_MIN;
            tracing::info!("Slack Socket Mode connected");

            match self
                .run_socket_connection(socket, &tx, &mut shutdown_rx)
                .await?
            {
                ConnectionOutcome::Reconnect => {
                    tracing::warn!("Slack Socket Mode disconnected, reconnecting");
                }
                ConnectionOutcome::Shutdown => {
                    return Ok(());
                }
            }
        }
    }

    async fn sleep_with_backoff(
        &self,
        shutdown_rx: &mut watch::Receiver<bool>,
        base_delay: Duration,
    ) -> Result<(), ChannelError> {
        let jitter_ms = rand::thread_rng().gen_range(0..1000_u64);
        let sleep_for = base_delay
            .saturating_add(Duration::from_millis(jitter_ms))
            .min(SOCKET_BACKOFF_MAX);
        tokio::select! {
            _ = tokio::time::sleep(sleep_for) => Ok(()),
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    Ok(())
                } else {
                    Ok(())
                }
            }
        }
    }

    async fn run_socket_connection(
        &self,
        mut socket: SlackSocketStream,
        tx: &mpsc::Sender<IncomingMessage>,
        shutdown_rx: &mut watch::Receiver<bool>,
    ) -> Result<ConnectionOutcome, ChannelError> {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        let _ = socket.close(None).await;
                        return Ok(ConnectionOutcome::Shutdown);
                    }
                }
                frame = socket.next() => {
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            if self
                                .handle_socket_text(&mut socket, text.as_str(), tx)
                                .await?
                            {
                                return Ok(ConnectionOutcome::Reconnect);
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            let _ = socket.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(Message::Close(frame))) => {
                            tracing::info!(?frame, "Slack Socket Mode closed by remote");
                            return Ok(ConnectionOutcome::Reconnect);
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "Slack Socket Mode websocket read failed");
                            return Ok(ConnectionOutcome::Reconnect);
                        }
                        None => {
                            return Ok(ConnectionOutcome::Reconnect);
                        }
                    }
                }
            }
        }
    }

    async fn handle_socket_text(
        &self,
        socket: &mut SlackSocketStream,
        text: &str,
        tx: &mpsc::Sender<IncomingMessage>,
    ) -> Result<bool, ChannelError> {
        let envelope: SlackSocketEnvelope =
            serde_json::from_str(text).map_err(|e| ChannelError::InvalidMessage(e.to_string()))?;

        if let Some(envelope_id) = envelope.envelope_id.as_deref() {
            self.ack_envelope(socket, envelope_id).await?;
        }

        match envelope.envelope_type.as_str() {
            "hello" => {
                tracing::info!("Slack Socket Mode hello received");
                Ok(false)
            }
            "disconnect" => {
                tracing::warn!(reason = ?envelope.reason, "Slack requested Socket Mode reconnect");
                Ok(true)
            }
            "events_api" => {
                if let Some(payload) = envelope.payload
                    && let Some(message) = self.handle_events_api(payload).await?
                    && tx.send(message).await.is_err()
                {
                    tracing::debug!("Slack Socket Mode receiver dropped");
                    return Ok(true);
                }
                Ok(false)
            }
            other => {
                tracing::debug!(envelope_type = other, "Ignoring Slack Socket Mode envelope");
                Ok(false)
            }
        }
    }

    async fn ack_envelope(
        &self,
        socket: &mut SlackSocketStream,
        envelope_id: &str,
    ) -> Result<(), ChannelError> {
        let ack = serde_json::json!({ "envelope_id": envelope_id });
        socket
            .send(Message::Text(ack.to_string().into()))
            .await
            .map_err(|e| ChannelError::Disconnected {
                name: CHANNEL_NAME.to_string(),
                reason: format!("failed to ack envelope: {e}"),
            })
    }

    async fn handle_events_api(
        &self,
        payload: SlackSocketPayload,
    ) -> Result<Option<IncomingMessage>, ChannelError> {
        let Some(event) = payload.event else {
            return Ok(None);
        };

        match event.event_type.as_str() {
            "app_mention" => {
                self.build_incoming_message(event, payload.team_id, payload.event_id, false)
                    .await
            }
            "message" => {
                self.build_incoming_message(event, payload.team_id, payload.event_id, true)
                    .await
            }
            _ => Ok(None),
        }
    }

    async fn build_incoming_message(
        &self,
        event: SlackEvent,
        team_id: Option<String>,
        _event_id: Option<String>,
        allow_dm_only: bool,
    ) -> Result<Option<IncomingMessage>, ChannelError> {
        if event.bot_id.is_some() || event.subtype.is_some() {
            return Ok(None);
        }

        let user = match event.user.clone() {
            Some(user) => user,
            None => return Ok(None),
        };
        let channel = match event.channel.clone() {
            Some(channel) => channel,
            None => return Ok(None),
        };
        let text = match event.text.clone() {
            Some(text) => text,
            None => return Ok(None),
        };
        let ts = match event.ts.clone() {
            Some(ts) => ts,
            None => return Ok(None),
        };

        if self
            .config
            .bot_user_id
            .as_deref()
            .is_some_and(|bot_user_id| bot_user_id == user)
        {
            return Ok(None);
        }

        let is_dm = channel.starts_with('D');
        if allow_dm_only && !is_dm {
            return Ok(None);
        }

        let cleaned_text = strip_bot_mention(&text);
        let enriched_text = self
            .enrich_with_thread_context(&channel, event.thread_ts.as_deref(), &ts, &cleaned_text)
            .await;
        let thread_ts = event.thread_ts.clone().or_else(|| Some(ts.clone()));
        let placeholder_ts = match self
            .chat_post_message(&channel, "Working :gear:...", thread_ts.as_deref(), None)
            .await
        {
            Ok(response) => response.ts,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to post Slack placeholder");
                None
            }
        };

        let attachments = self.extract_attachments(event.files.as_deref()).await;
        let metadata = SlackMessageMetadata {
            channel: channel.clone(),
            thread_ts: thread_ts.clone(),
            placeholder_ts,
            message_ts: ts,
            team_id,
            sender_id: Some(user.clone()),
        };
        let metadata = serde_json::to_value(metadata).map_err(|e| {
            ChannelError::InvalidMessage(format!("failed to serialize Slack metadata: {e}"))
        })?;

        let mut msg = IncomingMessage::new(CHANNEL_NAME, &user, enriched_text)
            .with_sender_id(user.clone())
            .with_metadata(metadata)
            .with_attachments(attachments);

        if let Some(thread_ts) = thread_ts {
            msg = msg.with_thread(thread_ts);
        }

        Ok(Some(msg))
    }

    async fn extract_attachments(&self, files: Option<&[SlackFile]>) -> Vec<IncomingAttachment> {
        let Some(files) = files else {
            return Vec::new();
        };

        let mut attachments = Vec::with_capacity(files.len());

        for file in files {
            let mime_type = file
                .mimetype
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_string());
            let mut data = Vec::new();

            if let Some(size) = file.size
                && size <= MAX_DOWNLOAD_SIZE_BYTES
                && let Some(url) = file.url_private.as_deref()
            {
                match self.download_file(url).await {
                    Ok(bytes) => {
                        if bytes.len() as u64 <= MAX_DOWNLOAD_SIZE_BYTES {
                            data = bytes;
                        } else {
                            tracing::warn!(file_id = %file.id, size = bytes.len(), "Discarding oversized Slack attachment");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(file_id = %file.id, error = %e, "Slack attachment download failed");
                    }
                }
            }

            attachments.push(IncomingAttachment {
                id: file.id.clone(),
                kind: AttachmentKind::from_mime_type(&mime_type),
                mime_type,
                filename: file.name.clone(),
                size_bytes: file.size,
                source_url: file.url_private.clone(),
                storage_key: None,
                extracted_text: None,
                data,
                duration_secs: None,
            });
        }

        attachments
    }

    async fn download_file(&self, url: &str) -> Result<Vec<u8>, ChannelError> {
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.config.bot_token)
            .send()
            .await
            .map_err(|e| ChannelError::Http(format!("Slack file download failed: {e}")))?;

        if !response.status().is_success() {
            return Err(ChannelError::Http(format!(
                "Slack file download returned {}",
                response.status()
            )));
        }

        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|e| ChannelError::Http(format!("Slack file download body read failed: {e}")))
    }

    async fn slack_api_post<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        payload: &serde_json::Value,
    ) -> Result<T, ChannelError> {
        let response = self
            .client
            .post(endpoint)
            .bearer_auth(&self.config.bot_token)
            .json(payload)
            .send()
            .await
            .map_err(|e| ChannelError::Http(format!("{endpoint} failed: {e}")))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| ChannelError::Http(format!("{endpoint} body read failed: {e}")))?;

        if !status.is_success() {
            return Err(ChannelError::Http(format!(
                "{endpoint} returned {status}: {body}"
            )));
        }

        serde_json::from_str(&body).map_err(|e| {
            ChannelError::Http(format!(
                "{endpoint} response parse failed: {e}; body={body}"
            ))
        })
    }

    async fn slack_api_get<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
    ) -> Result<T, ChannelError> {
        let response = self
            .client
            .get(endpoint)
            .bearer_auth(&self.config.bot_token)
            .send()
            .await
            .map_err(|e| ChannelError::Http(format!("{endpoint} failed: {e}")))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| ChannelError::Http(format!("{endpoint} body read failed: {e}")))?;

        if !status.is_success() {
            return Err(ChannelError::Http(format!(
                "{endpoint} returned {status}: {body}"
            )));
        }

        serde_json::from_str(&body).map_err(|e| {
            ChannelError::Http(format!(
                "{endpoint} response parse failed: {e}; body={body}"
            ))
        })
    }

    async fn get_thread_replies(
        &self,
        channel_id: &str,
        thread_ts: &str,
        limit: usize,
    ) -> Result<Vec<SlackReplyMessage>, ChannelError> {
        let endpoint = format!(
            "https://slack.com/api/conversations.replies?channel={}&ts={}&limit={}",
            urlencoding::encode(channel_id),
            urlencoding::encode(thread_ts),
            limit.max(1)
        );

        let response: SlackRepliesResponse = self.slack_api_get(&endpoint).await?;
        if response.ok {
            Ok(response.messages)
        } else {
            Err(ChannelError::Http(format!(
                "conversations.replies failed: {}",
                response
                    .error
                    .unwrap_or_else(|| "unknown Slack error".to_string())
            )))
        }
    }

    async fn enrich_with_thread_context(
        &self,
        channel_id: &str,
        actual_thread_ts: Option<&str>,
        current_ts: &str,
        cleaned_text: &str,
    ) -> String {
        let Some(thread_ts) = actual_thread_ts else {
            return cleaned_text.to_string();
        };

        let replies = match self
            .get_thread_replies(channel_id, thread_ts, THREAD_CONTEXT_MESSAGE_LIMIT + 4)
            .await
        {
            Ok(replies) => replies,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    channel = %channel_id,
                    thread_ts,
                    "Failed to fetch Slack thread context"
                );
                return cleaned_text.to_string();
            }
        };

        let context = format_thread_context(&replies, current_ts, cleaned_text);
        if context.is_empty() {
            cleaned_text.to_string()
        } else {
            format!("{context}\n\nCurrent Slack message:\n{cleaned_text}")
        }
    }

    async fn chat_post_message(
        &self,
        channel_id: &str,
        text: &str,
        thread_ts: Option<&str>,
        blocks: Option<serde_json::Value>,
    ) -> Result<SlackPostMessageResponse, ChannelError> {
        let mut payload = serde_json::json!({
            "channel": channel_id,
            "text": text,
        });
        if let Some(thread_ts) = thread_ts {
            payload["thread_ts"] = serde_json::Value::String(thread_ts.to_string());
        }
        if let Some(blocks) = blocks {
            payload["blocks"] = blocks;
        }

        let response: SlackPostMessageResponse = self
            .slack_api_post("https://slack.com/api/chat.postMessage", &payload)
            .await?;
        ensure_slack_ok(response, "chat.postMessage")
    }

    async fn chat_update(
        &self,
        channel_id: &str,
        ts: &str,
        text: &str,
        blocks: Option<serde_json::Value>,
    ) -> Result<SlackPostMessageResponse, ChannelError> {
        let mut payload = serde_json::json!({
            "channel": channel_id,
            "ts": ts,
            "text": text,
        });
        if let Some(blocks) = blocks {
            payload["blocks"] = blocks;
        }

        let response: SlackPostMessageResponse = self
            .slack_api_post("https://slack.com/api/chat.update", &payload)
            .await?;
        ensure_slack_ok(response, "chat.update")
    }

    async fn set_presence(&self, presence: &str) -> Result<(), ChannelError> {
        let payload = serde_json::json!({ "presence": presence });
        let response: SlackApiResponse = self
            .slack_api_post("https://slack.com/api/users.setPresence", &payload)
            .await?;
        ensure_simple_slack_ok(response, "users.setPresence")
    }

    async fn send_typing(&self, channel_id: &str) -> Result<(), ChannelError> {
        let payload = serde_json::json!({ "channel": channel_id });
        let response: SlackApiResponse = self
            .slack_api_post("https://slack.com/api/chat.meTyping", &payload)
            .await?;
        ensure_simple_slack_ok(response, "chat.meTyping")
    }

    async fn should_send_typing(&self, channel_id: &str) -> bool {
        let mut cache = self.state.last_typing_by_channel.lock().await;
        if cache.len() > TYPING_CACHE_LIMIT {
            cache.clear();
        }

        let now = Instant::now();
        if let Some(last_sent) = cache.get(channel_id)
            && now.duration_since(*last_sent) < TYPING_INTERVAL
        {
            return false;
        }

        cache.insert(channel_id.to_string(), now);
        true
    }

    async fn should_update_placeholder(&self, placeholder_ts: &str, text: &str) -> bool {
        let cache = self.state.last_status_by_placeholder.lock().await;
        cache
            .get(placeholder_ts)
            .map(|last| last != text)
            .unwrap_or(true)
    }

    async fn remember_placeholder_status(&self, placeholder_ts: &str, text: &str) {
        let mut cache = self.state.last_status_by_placeholder.lock().await;
        if cache.len() > STATUS_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(placeholder_ts.to_string(), text.to_string());
    }

    async fn clear_placeholder_status(&self, placeholder_ts: &str) {
        self.state
            .last_status_by_placeholder
            .lock()
            .await
            .remove(placeholder_ts);
    }
}

#[async_trait]
impl Channel for SlackSocketChannel {
    fn name(&self) -> &str {
        CHANNEL_NAME
    }

    async fn start(&self) -> Result<MessageStream, ChannelError> {
        let mut task_guard = self.state.listener_task.lock().await;
        if task_guard.is_some() {
            return Err(ChannelError::StartupFailed {
                name: CHANNEL_NAME.to_string(),
                reason: "Slack Socket Mode channel already started".to_string(),
            });
        }

        let (tx, rx) = mpsc::channel(256);
        let channel = self.clone();
        let shutdown_rx = self.state.shutdown_tx.subscribe();
        *task_guard = Some(tokio::spawn(async move {
            if let Err(e) = channel.socket_listener(tx, shutdown_rx).await {
                tracing::error!(error = %e, "Slack Socket Mode listener exited");
            }
        }));
        drop(task_guard);

        if let Err(e) = self.set_presence("auto").await {
            tracing::warn!(error = %e, "Failed to set Slack presence to auto");
        }

        tracing::info!("Slack Socket Mode channel started");
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    async fn respond(
        &self,
        msg: &IncomingMessage,
        response: OutgoingResponse,
    ) -> Result<(), ChannelError> {
        let metadata: SlackMessageMetadata =
            serde_json::from_value(msg.metadata.clone()).map_err(|e| ChannelError::SendFailed {
                name: CHANNEL_NAME.to_string(),
                reason: format!("Invalid Slack response metadata: {e}"),
            })?;

        if !msg.is_internal
            && let Some(ref placeholder_ts) = metadata.placeholder_ts
        {
            match self
                .chat_update(&metadata.channel, placeholder_ts, &response.content, None)
                .await
            {
                Ok(_) => {
                    self.clear_placeholder_status(placeholder_ts).await;
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(error = %e, placeholder_ts, "Slack placeholder update failed, falling back to chat.postMessage");
                }
            }
        }

        let thread_ts = response
            .thread_id
            .as_deref()
            .or(metadata.thread_ts.as_deref());
        self.chat_post_message(&metadata.channel, &response.content, thread_ts, None)
            .await
            .map(|_| ())
    }

    async fn send_status(
        &self,
        status: StatusUpdate,
        metadata: &serde_json::Value,
    ) -> Result<(), ChannelError> {
        let metadata: SlackMessageMetadata = match serde_json::from_value(metadata.clone()) {
            Ok(metadata) => metadata,
            Err(e) => {
                tracing::debug!(error = %e, "Skipping Slack status update with invalid metadata");
                return Ok(());
            }
        };

        if matches!(
            status,
            StatusUpdate::Thinking(_) | StatusUpdate::ToolStarted { .. }
        ) && self.should_send_typing(&metadata.channel).await
            && let Err(e) = self.send_typing(&metadata.channel).await
        {
            tracing::warn!(error = %e, channel = %metadata.channel, "Slack typing indicator failed");
        }

        let Some(placeholder_ts) = metadata.placeholder_ts.as_deref() else {
            return Ok(());
        };
        let Some(status_text) = status_text_for_slack(&status) else {
            return Ok(());
        };
        if !self
            .should_update_placeholder(placeholder_ts, &status_text)
            .await
        {
            return Ok(());
        }

        self.chat_update(&metadata.channel, placeholder_ts, &status_text, None)
            .await?;
        self.remember_placeholder_status(placeholder_ts, &status_text)
            .await;

        Ok(())
    }

    async fn broadcast(
        &self,
        user_id: &str,
        response: OutgoingResponse,
    ) -> Result<(), ChannelError> {
        let channel_id = response
            .metadata
            .get("channel")
            .and_then(|value| value.as_str())
            .or_else(|| {
                response
                    .metadata
                    .get("channel_id")
                    .and_then(|value| value.as_str())
            })
            .unwrap_or(user_id);
        let thread_ts = response.thread_id.as_deref().or_else(|| {
            response
                .metadata
                .get("thread_ts")
                .and_then(|value| value.as_str())
        });

        self.chat_post_message(channel_id, &response.content, thread_ts, None)
            .await
            .map(|_| ())
    }

    async fn health_check(&self) -> Result<(), ChannelError> {
        let response: SlackAuthTestResponse = self
            .slack_api_post("https://slack.com/api/auth.test", &serde_json::json!({}))
            .await?;
        if response.ok {
            Ok(())
        } else {
            Err(ChannelError::HealthCheckFailed {
                name: response.error.unwrap_or_else(|| CHANNEL_NAME.to_string()),
            })
        }
    }

    fn conversation_context(
        &self,
        metadata: &serde_json::Value,
    ) -> std::collections::HashMap<String, String> {
        let mut context = HashMap::new();
        if let Some(sender_id) = metadata.get("sender_id").and_then(|value| value.as_str()) {
            context.insert("sender_uuid".to_string(), sender_id.to_string());
        }
        if let Some(channel_id) = metadata.get("channel").and_then(|value| value.as_str()) {
            context.insert("group".to_string(), channel_id.to_string());
        }
        context.insert("platform".to_string(), CHANNEL_NAME.to_string());
        context
    }

    async fn shutdown(&self) -> Result<(), ChannelError> {
        if let Err(e) = self.set_presence("away").await {
            tracing::warn!(error = %e, "Failed to set Slack presence to away");
        }

        let _ = self.state.shutdown_tx.send(true);
        if let Some(handle) = self.state.listener_task.lock().await.take() {
            match tokio::time::timeout(Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "Slack Socket Mode listener join failed");
                }
                Err(_) => {
                    tracing::warn!("Slack Socket Mode shutdown timed out");
                }
            }
        }
        Ok(())
    }
}

fn ensure_slack_ok(
    response: SlackPostMessageResponse,
    method: &str,
) -> Result<SlackPostMessageResponse, ChannelError> {
    if response.ok {
        Ok(response)
    } else {
        Err(ChannelError::SendFailed {
            name: CHANNEL_NAME.to_string(),
            reason: format!(
                "{method} failed: {}",
                response
                    .error
                    .unwrap_or_else(|| "unknown Slack error".to_string())
            ),
        })
    }
}

fn ensure_simple_slack_ok(response: SlackApiResponse, method: &str) -> Result<(), ChannelError> {
    if response.ok {
        Ok(())
    } else {
        Err(ChannelError::SendFailed {
            name: CHANNEL_NAME.to_string(),
            reason: format!(
                "{method} failed: {}",
                response
                    .error
                    .unwrap_or_else(|| "unknown Slack error".to_string())
            ),
        })
    }
}

fn next_backoff(current: Duration) -> Duration {
    std::cmp::min(current * 2, SOCKET_BACKOFF_MAX)
}

fn strip_bot_mention(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with("<@")
        && let Some(end) = trimmed.find('>')
    {
        return trimmed[end + 1..].trim_start().to_string();
    }
    trimmed.to_string()
}

fn format_thread_context(
    replies: &[SlackReplyMessage],
    current_ts: &str,
    current_text: &str,
) -> String {
    let mut lines = Vec::new();

    for reply in replies {
        if reply.ts.as_deref() == Some(current_ts) {
            continue;
        }
        let Some(text) = reply.text.as_deref() else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() || text == current_text.trim() {
            continue;
        }

        let author = if let Some(user) = reply.user.as_deref() {
            user.to_string()
        } else if reply.bot_id.is_some() || reply.subtype.as_deref() == Some("bot_message") {
            "bot".to_string()
        } else {
            "unknown".to_string()
        };

        lines.push(format!(
            "- {}: {}",
            author,
            truncate_slack_context_text(text, THREAD_CONTEXT_TEXT_LIMIT)
        ));
    }

    if lines.is_empty() {
        String::new()
    } else {
        let keep_from = lines.len().saturating_sub(THREAD_CONTEXT_MESSAGE_LIMIT);
        format!(
            "Recent Slack thread context:\n{}",
            lines[keep_from..].join("\n")
        )
    }
}

fn truncate_slack_context_text(text: &str, max_chars: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }

    let byte_offset = normalized
        .char_indices()
        .nth(max_chars)
        .map(|(index, _)| index)
        .unwrap_or(normalized.len());
    format!("{}...", &normalized[..byte_offset])
}

fn status_text_for_slack(status: &StatusUpdate) -> Option<String> {
    match status {
        StatusUpdate::Thinking(message) => Some(if message.trim().is_empty() {
            "Thinking...".to_string()
        } else {
            message.trim().to_string()
        }),
        StatusUpdate::ToolStarted { name } => Some(format!("Using tool: {name}...")),
        StatusUpdate::ToolCompleted {
            name,
            success,
            error,
            ..
        } => Some(if *success {
            format!("Finished tool: {name}.")
        } else if let Some(error) = error.as_deref() {
            format!("Tool {name} failed: {error}")
        } else {
            format!("Tool {name} failed.")
        }),
        StatusUpdate::ToolResult { name, .. } => Some(format!("Processing result from {name}...")),
        StatusUpdate::StreamChunk(_) => None,
        StatusUpdate::Status(message) => {
            let message = message.trim();
            if message.is_empty()
                || message.eq_ignore_ascii_case("done")
                || message.eq_ignore_ascii_case("awaiting approval")
                || message.eq_ignore_ascii_case("rejected")
                || message.eq_ignore_ascii_case("interrupted")
            {
                None
            } else {
                Some(message.to_string())
            }
        }
        StatusUpdate::JobStarted { title, .. } => Some(format!("Starting background job: {title}")),
        StatusUpdate::ApprovalNeeded { tool_name, .. } => {
            Some(format!("Waiting for approval for {tool_name}..."))
        }
        StatusUpdate::AuthRequired { extension_name, .. } => {
            Some(format!("Authentication required for {extension_name}."))
        }
        StatusUpdate::AuthCompleted {
            extension_name,
            success,
            message,
        } => {
            let mut text = if *success {
                format!("Authentication completed for {extension_name}.")
            } else {
                format!("Authentication failed for {extension_name}.")
            };
            if !message.trim().is_empty() {
                text.push(' ');
                text.push_str(message.trim());
            }
            Some(text)
        }
        StatusUpdate::ImageGenerated { .. } => Some("Image generated.".to_string()),
        StatusUpdate::Suggestions { .. } => None,
        StatusUpdate::ReasoningUpdate { narrative, .. } => {
            let narrative = narrative.trim();
            if narrative.is_empty() {
                None
            } else {
                Some(narrative.to_string())
            }
        }
        StatusUpdate::TurnCost { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{SlackReplyMessage, format_thread_context, truncate_slack_context_text};

    #[test]
    fn thread_context_skips_current_message() {
        let replies = vec![
            SlackReplyMessage {
                ts: Some("1.0".to_string()),
                text: Some("first message".to_string()),
                user: Some("U1".to_string()),
                bot_id: None,
                subtype: None,
            },
            SlackReplyMessage {
                ts: Some("2.0".to_string()),
                text: Some("latest message".to_string()),
                user: Some("U2".to_string()),
                bot_id: None,
                subtype: None,
            },
        ];

        let context = format_thread_context(&replies, "2.0", "latest message");
        assert!(context.contains("first message"));
        assert!(!context.contains("latest message"));
    }

    #[test]
    fn truncate_context_collapses_whitespace() {
        let text = truncate_slack_context_text("hello\n\nworld   from   slack", 11);
        assert_eq!(text, "hello world...");
    }
}
