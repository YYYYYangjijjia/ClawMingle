use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use futures_util::{
    StreamExt,
    stream::{self, TryStreamExt},
};
use reqwest::{Client, Method, StatusCode, Url};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha1::{Digest, Sha1};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    sync::Mutex,
};
use tracing::{info, warn};

#[path = "profile.rs"]
mod profile;
pub use profile::{QqReadError, QqReadRequest};
#[path = "material.rs"]
mod material;
pub use material::{MaterialError, MaterialImage};

use crate::config::QqConfig;
use crate::qq::{group::GroupMessageAck, types::GatewayInfo};

const CHUNKED_UPLOAD_THRESHOLD_BYTES: u64 = 5 * 1024 * 1024;
const MD5_10M_SIZE: u64 = 10_002_432;
const DEFAULT_CHUNK_UPLOAD_CONCURRENCY: usize = 1;
const MAX_CHUNK_UPLOAD_CONCURRENCY: usize = 10;
const MAX_PART_FINISH_RETRY_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const PART_UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const PART_UPLOAD_MAX_RETRIES: u32 = 2;
const COMPLETE_UPLOAD_MAX_RETRIES: u32 = 2;
const PART_FINISH_MAX_RETRIES: u32 = 2;
const PART_FINISH_RETRYABLE_CODE: i64 = 40093001;
const PART_FINISH_RETRYABLE_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_PART_FINISH_RETRY_TIMEOUT: Duration = Duration::from_secs(2 * 60);
pub const MAX_PASSIVE_IMAGE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct QqApiClient {
    client: Client,
    group_client: Client,
    config: QqConfig,
    token_cache: std::sync::Arc<Mutex<Option<CachedToken>>>,
    msg_seq: std::sync::Arc<Mutex<MsgSeqCache>>,
}

/// Per-target monotonic message-seq counters with FIFO eviction. The seq only
/// needs to increase within a single message's reply window, so bounding the
/// map (dropping the oldest ids first) keeps it from growing without limit over
/// the process lifetime.
#[derive(Debug, Default)]
struct MsgSeqCache {
    counters: HashMap<String, u32>,
    order: VecDeque<String>,
}

impl MsgSeqCache {
    /// Maximum number of distinct reply targets kept at once. Far above any
    /// realistic count of concurrently-in-flight reply windows.
    const CAP: usize = 4096;

    fn next(&mut self, key: &str) -> u32 {
        if let Some(seq) = self.counters.get_mut(key) {
            *seq += 1;
            return *seq;
        }
        while self.order.len() >= Self::CAP {
            if let Some(old) = self.order.pop_front() {
                self.counters.remove(&old);
            } else {
                break;
            }
        }
        self.counters.insert(key.to_string(), 1);
        self.order.push_back(key.to_string());
        1
    }
}

#[derive(Debug, Clone)]
struct CachedToken {
    value: String,
    expires_at: std::time::Instant,
}

#[derive(Debug, Deserialize)]
struct AccessTokenResponse {
    access_token: String,
    #[serde(
        default = "default_expires_in",
        deserialize_with = "deserialize_u64_value"
    )]
    expires_in: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct MessageReference<'a> {
    message_id: &'a str,
}

#[derive(Debug, Serialize)]
struct SendTextBody<'a> {
    content: &'a str,
    msg_type: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    msg_id: Option<&'a str>,
    msg_seq: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_reference: Option<MessageReference<'a>>,
}

#[derive(Debug, Serialize)]
struct SendMarkdownBody<'a> {
    msg_type: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    msg_id: Option<&'a str>,
    msg_seq: u32,
    markdown: MarkdownPayload<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_reference: Option<MessageReference<'a>>,
}

#[derive(Debug, Serialize)]
struct MarkdownPayload<'a> {
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct UploadFileBody<'a> {
    file_type: u8,
    file_data: &'a str,
    file_name: &'a str,
    srv_send_msg: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct UploadFileResponse {
    file_info: String,
}

#[derive(Debug, Clone, Serialize)]
struct UploadPrepareBody<'a> {
    file_type: u8,
    file_name: &'a str,
    file_size: u64,
    md5: &'a str,
    sha1: &'a str,
    md5_10m: &'a str,
}

#[derive(Debug, Clone, Serialize)]
struct UploadPartFinishBody<'a> {
    upload_id: &'a str,
    part_index: u64,
    block_size: u64,
    md5: &'a str,
}

#[derive(Debug, Clone, Serialize)]
struct UploadCompleteBody<'a> {
    upload_id: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
struct UploadPrepareResponse {
    upload_id: String,
    #[serde(deserialize_with = "deserialize_u64_value")]
    block_size: u64,
    parts: Vec<UploadPart>,
    #[serde(default, deserialize_with = "deserialize_optional_u64_value")]
    concurrency: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64_value")]
    retry_timeout: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct UploadPart {
    #[serde(deserialize_with = "deserialize_u64_value")]
    index: u64,
    presigned_url: String,
}

#[derive(Debug, Serialize)]
struct SendMediaBody<'a> {
    msg_type: u8,
    msg_id: &'a str,
    msg_seq: u32,
    media: MediaFileInfo<'a>,
}

#[derive(Debug, Serialize)]
struct MediaFileInfo<'a> {
    file_info: &'a str,
}

#[derive(Debug, Clone)]
struct FileHashes {
    md5: String,
    sha1: String,
    md5_10m: String,
}

#[derive(Debug, Deserialize)]
struct QqErrorBody {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    msg: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("QQ API request failed with status {status}: {message}")]
struct QqApiError {
    status: StatusCode,
    code: Option<i64>,
    message: String,
    raw: String,
}

impl QqApiError {
    fn from_response(status: StatusCode, raw: String) -> Self {
        let parsed = serde_json::from_str::<QqErrorBody>(&raw).ok();
        let message = parsed
            .as_ref()
            .and_then(|value| value.message.as_deref().or(value.msg.as_deref()))
            .unwrap_or(raw.as_str())
            .to_string();
        Self {
            status,
            code: parsed.and_then(|value| value.code),
            message,
            raw,
        }
    }
}

/// Errors deliberately contain no upstream body, URL or transport source.
/// An unknown result must be reconciled by the delivery coordinator, rather
/// than retried with a newly allocated message sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GroupSendError {
    #[error("QQ group message was not sent: {reason}")]
    NotSent { reason: GroupNotSentReason },
    #[error("QQ group message rejected (HTTP {status}, code {code:?})")]
    Rejected { status: u16, code: Option<i64> },
    #[error("QQ group message delivery is unknown: {reason}")]
    Unknown { reason: GroupSendUnknownReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GroupNotSentReason {
    #[error("invalid request")]
    InvalidRequest,
    #[error("invalid API endpoint")]
    InvalidEndpoint,
    #[error("access token unavailable")]
    Authentication,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GroupSendUnknownReason {
    #[error("request timed out")]
    Timeout,
    #[error("transport failed")]
    Transport,
    #[error("response could not be read")]
    ResponseBody,
    #[error("upstream returned HTTP {status}")]
    UpstreamStatus { status: u16 },
    #[error("acknowledgement was invalid or incomplete")]
    InvalidAcknowledgement,
}

/// An in-memory upload capability, bound to its original scene and recipient.
/// Deliberately has no Debug/Serialize implementation: file_info is private.
#[derive(Clone)]
pub struct UploadedImage {
    file_info: String,
    pub ttl: u64,
    expires_at: Option<Instant>,
    scope: super::message_content::MessageScope,
    target_openid: String,
}

/// Uploading uses srv_send_msg=false, so even an unknown upload response cannot
/// mean a chat bubble was sent. Never turn these errors into delivery Unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ImageUploadError {
    #[error("invalid image upload request")]
    InvalidRequest,
    #[error("invalid image upload endpoint")]
    InvalidEndpoint,
    #[error("image upload access token unavailable")]
    Authentication,
    #[error("image upload rejected (HTTP {status}, code {code:?})")]
    Rejected { status: u16, code: Option<i64> },
    #[error("image upload timed out")]
    Timeout,
    #[error("image upload transport failed")]
    Transport,
    #[error("image upload response could not be read")]
    ResponseBody,
    #[error("image upload returned HTTP {status}")]
    UpstreamStatus { status: u16 },
    #[error("image upload response was invalid or incomplete")]
    InvalidResponse,
}

#[derive(Deserialize)]
struct PassiveImageUploadResponse {
    file_info: String,
    ttl: u64,
}

impl QqApiClient {
    pub fn new(config: QqConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build reqwest client")?;
        let group_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to build QQ group HTTP client")?;
        Ok(Self {
            client,
            group_client,
            config,
            token_cache: std::sync::Arc::new(Mutex::new(None)),
            msg_seq: std::sync::Arc::new(Mutex::new(MsgSeqCache::default())),
        })
    }

    /// Send exactly one proactive text message to an explicit conversation.
    /// No passive msg_id, event_id, format fallback or transport retry is used.
    /// Permission/quota rejection is surfaced to the durable outbox coordinator.
    pub async fn send_proactive_text(
        &self,
        scope: super::message_content::MessageScope,
        target_openid: &str,
        msg_seq: u32,
        text: &str,
    ) -> std::result::Result<GroupMessageAck, GroupSendError> {
        self.send_proactive_message(
            scope,
            target_openid,
            msg_seq,
            super::message_content::PassiveMessage {
                text,
                style: super::message_content::OutboundStyle::Text,
                help_buttons: false,
                command_buttons: &[],
                reference_idx: None,
            },
        )
        .await
    }

    /// Reference and formatting are independent of a passive reply anchor.
    /// This sends once; even an uncertain result never triggers a plain-text retry.
    pub async fn send_proactive_message(
        &self,
        scope: super::message_content::MessageScope,
        target_openid: &str,
        msg_seq: u32,
        content: super::message_content::PassiveMessage<'_>,
    ) -> std::result::Result<GroupMessageAck, GroupSendError> {
        let text = content.text;
        if target_openid.is_empty()
            || matches!(target_openid, "." | "..")
            || msg_seq == 0
            || text.trim().is_empty()
            || text.chars().count() > 4500
            || content
                .reference_idx
                .is_some_and(|v| v.trim().is_empty() || v.chars().any(char::is_control))
            || (content.reference_idx.is_some()
                && content.style == super::message_content::OutboundStyle::Markdown)
        {
            return Err(GroupSendError::NotSent {
                reason: GroupNotSentReason::InvalidRequest,
            });
        }
        let url = self
            .passive_endpoint(scope, target_openid, "messages")
            .map_err(|reason| GroupSendError::NotSent { reason })?;
        let mut body = match content.style {
            super::message_content::OutboundStyle::Text => {
                serde_json::json!({"msg_type":0,"msg_seq":msg_seq,"content":text})
            }
            super::message_content::OutboundStyle::Markdown => {
                super::message_content::proactive_markdown_body(
                    text,
                    msg_seq,
                    scope,
                    content.help_buttons,
                )
                .map_err(|_| GroupSendError::NotSent {
                    reason: GroupNotSentReason::InvalidRequest,
                })?
            }
        };
        if content.style == super::message_content::OutboundStyle::Markdown
            && !content.command_buttons.is_empty()
        {
            body["keyboard"] = super::message_content::command_keyboard(content.command_buttons)
                .map_err(|_| GroupSendError::NotSent {
                    reason: GroupNotSentReason::InvalidRequest,
                })?;
        }
        if let Some(message_id) = content.reference_idx {
            body["message_reference"] = serde_json::json!({"message_id":message_id});
        }
        self.post_passive_body(url, &body, None).await
    }

    /// Send one uploaded image or ordinary file to its original conversation.
    /// The durable outbox assigns/persists a positive proactive sequence and
    /// checks permission before calling. No passive anchor, retry or fallback.
    pub async fn send_proactive_image(
        &self,
        scope: super::message_content::MessageScope,
        target_openid: &str,
        msg_seq: u32,
        image: &UploadedImage,
    ) -> std::result::Result<GroupMessageAck, GroupSendError> {
        if target_openid.is_empty()
            || matches!(target_openid, "." | "..")
            || target_openid.contains('\0')
            || target_openid != image.target_openid
            || scope != image.scope
            || msg_seq == 0
            || image
                .expires_at
                .is_some_and(|expiry| expiry <= Instant::now())
        {
            return Err(GroupSendError::NotSent {
                reason: GroupNotSentReason::InvalidRequest,
            });
        }
        let url = self
            .passive_endpoint(scope, target_openid, "messages")
            .map_err(|reason| GroupSendError::NotSent { reason })?;
        let body = serde_json::json!({
            "msg_type": 7,
            "msg_seq": msg_seq,
            "media": { "file_info": image.file_info },
        });
        // Also rechecks upload lifetime after a potentially slow token refresh.
        self.post_passive_body(url, &body, image.expires_at).await
    }

    /// Send exactly one passive, plain-text group bubble. The owner assigns
    /// and persists `msg_seq`; this method never retries, splits or falls back.
    /// `reference_idx` is a scene msg_idx / prior ACK ref_idx, not `msg_id`.
    pub async fn send_group_text(
        &self,
        group_openid: &str,
        passive_msg_id: &str,
        msg_seq: u32,
        text: &str,
        reference_idx: Option<&str>,
    ) -> std::result::Result<GroupMessageAck, GroupSendError> {
        self.send_passive_message(
            super::message_content::MessageScope::Group,
            group_openid,
            passive_msg_id,
            msg_seq,
            super::message_content::PassiveMessage {
                text,
                style: super::message_content::OutboundStyle::Text,
                help_buttons: false,
                command_buttons: &[],
                reference_idx,
            },
        )
        .await
    }

    /// Send one passive private message with its persisted application sequence.
    /// A valid platform ACK is required; there is no Markdown fallback, retry,
    /// redirect or automatic splitting on this reliable delivery path.
    pub async fn send_private_text(
        &self,
        user_openid: &str,
        passive_msg_id: &str,
        msg_seq: u32,
        text: &str,
    ) -> std::result::Result<GroupMessageAck, GroupSendError> {
        self.send_passive_message(
            super::message_content::MessageScope::Private,
            user_openid,
            passive_msg_id,
            msg_seq,
            super::message_content::PassiveMessage {
                text,
                style: super::message_content::OutboundStyle::Text,
                help_buttons: false,
                command_buttons: &[],
                reference_idx: None,
            },
        )
        .await
    }

    pub async fn send_passive_message(
        &self,
        scope: super::message_content::MessageScope,
        target_openid: &str,
        passive_msg_id: &str,
        msg_seq: u32,
        content: super::message_content::PassiveMessage<'_>,
    ) -> std::result::Result<GroupMessageAck, GroupSendError> {
        let super::message_content::PassiveMessage {
            text,
            style,
            help_buttons,
            command_buttons,
            reference_idx,
        } = content;
        if target_openid.is_empty()
            || matches!(target_openid, "." | "..")
            || passive_msg_id.is_empty()
            || msg_seq == 0
            || text.trim().is_empty()
            || text.chars().count() > 4500
            || reference_idx.is_some_and(str::is_empty)
            || (reference_idx.is_some() && style == super::message_content::OutboundStyle::Markdown)
        {
            return Err(GroupSendError::NotSent {
                reason: GroupNotSentReason::InvalidRequest,
            });
        }
        let url = self
            .passive_endpoint(scope, target_openid, "messages")
            .map_err(|reason| GroupSendError::NotSent { reason })?;
        let mut body = match style {
            super::message_content::OutboundStyle::Text => serde_json::to_value(SendTextBody {
                content: text,
                msg_type: 0,
                msg_id: Some(passive_msg_id),
                msg_seq,
                message_reference: reference_idx.map(|message_id| MessageReference { message_id }),
            })
            .map_err(|_| GroupSendError::NotSent {
                reason: GroupNotSentReason::InvalidRequest,
            })?,
            super::message_content::OutboundStyle::Markdown => {
                super::message_content::markdown_body(
                    text,
                    passive_msg_id,
                    msg_seq,
                    scope,
                    help_buttons,
                )
                .map_err(|_| GroupSendError::NotSent {
                    reason: GroupNotSentReason::InvalidRequest,
                })?
            }
        };
        if style == super::message_content::OutboundStyle::Markdown && !command_buttons.is_empty() {
            body["keyboard"] =
                super::message_content::command_keyboard(command_buttons).map_err(|_| {
                    GroupSendError::NotSent {
                        reason: GroupNotSentReason::InvalidRequest,
                    }
                })?;
        }
        if style == super::message_content::OutboundStyle::Markdown
            && let Some(message_id) = reference_idx
        {
            body["message_reference"] = serde_json::json!({"message_id":message_id});
        }
        self.post_passive_body(url, &body, None).await
    }

    /// Upload one already-validated PNG/JPEG byte slice without sending a chat
    /// message. There is one request, no redirect, no retry, and no file I/O.
    pub async fn upload_passive_image(
        &self,
        scope: super::message_content::MessageScope,
        target_openid: &str,
        bytes: &[u8],
    ) -> std::result::Result<UploadedImage, ImageUploadError> {
        if target_openid.trim().is_empty()
            || target_openid.contains('\0')
            || matches!(target_openid, "." | "..")
            || bytes.len() > MAX_PASSIVE_IMAGE_BYTES
        {
            return Err(ImageUploadError::InvalidRequest);
        }
        let file_name = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            "output.png"
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            "output.jpg"
        } else {
            return Err(ImageUploadError::InvalidRequest);
        };
        self.upload_passive_bytes(scope, target_openid, bytes, file_name, 1)
            .await
    }

    /// Ordinary QQ attachment. Kept separate from the group file-directory API,
    /// which is not exposed by the official QQBot platform.
    pub async fn upload_passive_file(
        &self,
        scope: super::message_content::MessageScope,
        target_openid: &str,
        bytes: &[u8],
        file_name: &str,
    ) -> std::result::Result<UploadedImage, ImageUploadError> {
        if bytes.is_empty()
            || bytes.len() > MAX_PASSIVE_IMAGE_BYTES
            || file_name.is_empty()
            || file_name.chars().count() > 128
            || file_name.chars().any(char::is_control)
            || file_name.contains(['/', '\\'])
            || matches!(file_name, "." | "..")
        {
            return Err(ImageUploadError::InvalidRequest);
        }
        self.upload_passive_bytes(scope, target_openid, bytes, file_name, 4)
            .await
    }

    async fn upload_passive_bytes(
        &self,
        scope: super::message_content::MessageScope,
        target_openid: &str,
        bytes: &[u8],
        file_name: &str,
        file_type: u8,
    ) -> std::result::Result<UploadedImage, ImageUploadError> {
        let url = self
            .passive_endpoint(scope, target_openid, "files")
            .map_err(|_| ImageUploadError::InvalidEndpoint)?;
        let token = self
            .get_access_token()
            .await
            .map_err(|_| ImageUploadError::Authentication)?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let body = UploadFileBody {
            file_type,
            file_data: &encoded,
            file_name,
            srv_send_msg: false,
        };
        // Count from before the request, conservatively including time spent
        // waiting for the upload response rather than extending its lifetime.
        let started = Instant::now();
        let response = self
            .group_client
            .post(url)
            .header("Authorization", format!("QQBot {token}"))
            .header("X-Union-Appid", &self.config.app_id)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    ImageUploadError::Timeout
                } else {
                    ImageUploadError::Transport
                }
            })?;
        let status = response.status();
        let raw = response.bytes().await;
        if status.is_client_error() {
            let code = raw
                .ok()
                .and_then(|raw| serde_json::from_slice::<QqErrorBody>(&raw).ok())
                .and_then(|body| body.code);
            return Err(ImageUploadError::Rejected {
                status: status.as_u16(),
                code,
            });
        }
        if !status.is_success() {
            return Err(ImageUploadError::UpstreamStatus {
                status: status.as_u16(),
            });
        }
        let raw = raw.map_err(|_| ImageUploadError::ResponseBody)?;
        let uploaded: PassiveImageUploadResponse =
            serde_json::from_slice(&raw).map_err(|_| ImageUploadError::InvalidResponse)?;
        if uploaded.file_info.trim().is_empty()
            || uploaded.file_info.contains('\0')
            || uploaded.file_info.len() > 64 * 1024
        {
            return Err(ImageUploadError::InvalidResponse);
        }
        let expires_at = if uploaded.ttl == 0 {
            None
        } else {
            Some(
                started
                    .checked_add(Duration::from_secs(uploaded.ttl))
                    .ok_or(ImageUploadError::InvalidResponse)?,
            )
        };
        Ok(UploadedImage {
            file_info: uploaded.file_info,
            ttl: uploaded.ttl,
            expires_at,
            scope,
            target_openid: target_openid.to_owned(),
        })
    }

    /// Send exactly one media bubble with its persisted passive identity. The
    /// coordinator must check cancellation/revision/anchor again after upload.
    pub async fn send_passive_image(
        &self,
        scope: super::message_content::MessageScope,
        target_openid: &str,
        passive_msg_id: &str,
        msg_seq: u32,
        image: &UploadedImage,
    ) -> std::result::Result<GroupMessageAck, GroupSendError> {
        self.send_passive_image_referencing(
            scope,
            target_openid,
            passive_msg_id,
            msg_seq,
            image,
            None,
        )
        .await
    }

    pub async fn send_passive_image_referencing(
        &self,
        scope: super::message_content::MessageScope,
        target_openid: &str,
        passive_msg_id: &str,
        msg_seq: u32,
        image: &UploadedImage,
        reference_idx: Option<&str>,
    ) -> std::result::Result<GroupMessageAck, GroupSendError> {
        if target_openid != image.target_openid
            || scope != image.scope
            || passive_msg_id.trim().is_empty()
            || passive_msg_id.contains('\0')
            || msg_seq == 0
            || reference_idx.is_some_and(str::is_empty)
            || image
                .expires_at
                .is_some_and(|expiry| expiry <= Instant::now())
        {
            return Err(GroupSendError::NotSent {
                reason: GroupNotSentReason::InvalidRequest,
            });
        }
        let url = self
            .passive_endpoint(scope, target_openid, "messages")
            .map_err(|reason| GroupSendError::NotSent { reason })?;
        let mut body = serde_json::json!({
            "msg_type": 7,
            "msg_id": passive_msg_id,
            "msg_seq": msg_seq,
            "media": { "file_info": image.file_info },
        });
        if let Some(message_id) = reference_idx {
            body["message_reference"] = serde_json::json!({"message_id":message_id});
        }
        self.post_passive_body(url, &body, image.expires_at).await
    }

    fn passive_endpoint(
        &self,
        scope: super::message_content::MessageScope,
        target_openid: &str,
        resource: &str,
    ) -> std::result::Result<Url, GroupNotSentReason> {
        let mut url = Url::parse(&self.config.api_base_url)
            .map_err(|_| GroupNotSentReason::InvalidEndpoint)?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(GroupNotSentReason::InvalidEndpoint);
        }
        let scope_name = match scope {
            super::message_content::MessageScope::Group => "groups",
            super::message_content::MessageScope::Private => "users",
        };
        url.set_query(None);
        url.set_fragment(None);
        url.path_segments_mut()
            .map_err(|_| GroupNotSentReason::InvalidEndpoint)?
            .pop_if_empty()
            .extend(["v2", scope_name, target_openid, resource]);
        Ok(url)
    }

    /// Shared single-POST transport for text, Markdown and uploaded images.
    /// Keep all status/ACK/Unknown rules identical across the three formats.
    async fn post_passive_body(
        &self,
        url: Url,
        body: &Value,
        valid_until: Option<Instant>,
    ) -> std::result::Result<GroupMessageAck, GroupSendError> {
        let token = self
            .get_access_token()
            .await
            .map_err(|_| GroupSendError::NotSent {
                reason: GroupNotSentReason::Authentication,
            })?;
        // Token refresh may await the network; do not send a handle that expired
        // while authenticating, even if the initial local TTL check passed.
        if valid_until.is_some_and(|expiry| expiry <= Instant::now()) {
            return Err(GroupSendError::NotSent {
                reason: GroupNotSentReason::InvalidRequest,
            });
        }
        let response = self
            .group_client
            .post(url)
            .header("Authorization", format!("QQBot {token}"))
            .header("X-Union-Appid", &self.config.app_id)
            .json(&body)
            .send()
            .await
            .map_err(|err| GroupSendError::Unknown {
                reason: if err.is_timeout() {
                    GroupSendUnknownReason::Timeout
                } else {
                    GroupSendUnknownReason::Transport
                },
            })?;
        let status = response.status();
        let raw = response.bytes().await;
        if status.is_client_error() {
            // Rejection is already known from the status; an unreadable error
            // body only removes the optional platform code.
            let code = raw
                .ok()
                .and_then(|raw| serde_json::from_slice::<QqErrorBody>(&raw).ok())
                .and_then(|body| body.code);
            return Err(GroupSendError::Rejected {
                status: status.as_u16(),
                code,
            });
        }
        if !status.is_success() {
            return Err(GroupSendError::Unknown {
                reason: GroupSendUnknownReason::UpstreamStatus {
                    status: status.as_u16(),
                },
            });
        }
        let raw = raw.map_err(|_| GroupSendError::Unknown {
            reason: GroupSendUnknownReason::ResponseBody,
        })?;
        let ack: GroupMessageAck =
            serde_json::from_slice(&raw).map_err(|_| GroupSendError::Unknown {
                reason: GroupSendUnknownReason::InvalidAcknowledgement,
            })?;
        if ack.id.trim().is_empty() {
            return Err(GroupSendError::Unknown {
                reason: GroupSendUnknownReason::InvalidAcknowledgement,
            });
        }
        Ok(ack)
    }

    pub(crate) async fn send_text(
        &self,
        openid: &str,
        message_id: &str,
        text: &str,
        reference_message_id: Option<&str>,
    ) -> Result<()> {
        self.send_text_inner(openid, Some(message_id), text, reference_message_id)
            .await
    }

    pub(crate) async fn send_markdown(
        &self,
        openid: &str,
        message_id: &str,
        markdown: &str,
        reference_message_id: Option<&str>,
    ) -> Result<()> {
        self.send_text_inner(openid, Some(message_id), markdown, reference_message_id)
            .await
    }

    pub(crate) async fn send_markdown_proactive(&self, openid: &str, markdown: &str) -> Result<()> {
        self.send_text_inner(openid, None, markdown, None).await
    }

    async fn send_text_inner(
        &self,
        openid: &str,
        message_id: Option<&str>,
        text: &str,
        reference_message_id: Option<&str>,
    ) -> Result<()> {
        info!(
            openid = %openid,
            reply_to = message_id.unwrap_or(""),
            reference = reference_message_id.unwrap_or(""),
            text_len = text.len(),
            "sending qq text message"
        );
        for chunk in split_text(text, 4500) {
            let msg_seq = self.next_msg_seq(message_id.unwrap_or(openid)).await;
            let message_reference =
                reference_message_id.map(|value| MessageReference { message_id: value });
            let markdown_body = SendMarkdownBody {
                msg_type: 2,
                msg_id: message_id,
                msg_seq,
                markdown: MarkdownPayload { content: &chunk },
                message_reference,
            };
            let url = self.user_endpoint(openid, "messages");
            match self
                .post_json::<serde_json::Value, _>(url.clone(), &markdown_body)
                .await
            {
                Ok(_) => {}
                Err(err) => {
                    warn!(
                        error = %err,
                        "qq markdown message rejected; falling back to plain text"
                    );
                    let text_body = SendTextBody {
                        content: &chunk,
                        msg_type: 0,
                        msg_id: message_id,
                        msg_seq,
                        message_reference,
                    };
                    let _: serde_json::Value = self.post_json(url, &text_body).await?;
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn upload_file(
        &self,
        openid: &str,
        path: &Path,
        file_type: u8,
        file_name_override: Option<&str>,
    ) -> Result<String> {
        info!(
            openid = %openid,
            path = %path.display(),
            file_type,
            "uploading qq media"
        );
        let file_name = normalized_upload_file_name(path, file_name_override)?;
        let file_size = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("failed to stat {}", path.display()))?
            .len();
        ensure_upload_size_allowed(file_type, file_size)?;

        if should_use_chunked_upload(file_size) {
            return self
                .upload_file_chunked(openid, path, file_type, &file_name, file_size)
                .await;
        }

        self.upload_file_direct(path, openid, file_type, &file_name)
            .await
    }

    pub(crate) async fn send_media(
        &self,
        openid: &str,
        message_id: &str,
        file_info: &str,
    ) -> Result<()> {
        info!(
            openid = %openid,
            reply_to = %message_id,
            "sending qq media message"
        );
        let body = SendMediaBody {
            msg_type: 7,
            msg_id: message_id,
            msg_seq: self.next_msg_seq(message_id).await,
            media: MediaFileInfo { file_info },
        };
        let _: serde_json::Value = self
            .post_json(self.user_endpoint(openid, "messages"), &body)
            .await?;
        Ok(())
    }

    pub(crate) async fn get_gateway_url(&self) -> Result<String> {
        let response: GatewayInfo = self
            .request_json(
                Method::GET,
                format!("{}/gateway", self.config.api_base_url),
                Option::<&serde_json::Value>::None,
            )
            .await?;
        Ok(response.url)
    }

    pub(crate) async fn download_attachment(
        &self,
        source_url: &str,
        destination: &Path,
    ) -> Result<()> {
        let normalized_url = if source_url.starts_with("//") {
            format!("https:{source_url}")
        } else {
            source_url.to_string()
        };
        let response = self
            .client
            .get(&normalized_url)
            .send()
            .await
            .with_context(|| format!("failed to download {normalized_url}"))?;
        let status = response.status();
        let bytes = response.bytes().await?;
        anyhow::ensure!(status.is_success(), "download failed with status {status}");
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(destination, bytes).await?;
        info!(
            source_url = %normalized_url,
            destination = %destination.display(),
            "downloaded incoming attachment"
        );
        Ok(())
    }

    pub(crate) async fn get_access_token(&self) -> Result<String> {
        let mut cache = self.token_cache.lock().await;
        if let Some(current) = cache.as_ref()
            && current.expires_at > std::time::Instant::now() + Duration::from_secs(60)
        {
            return Ok(current.value.clone());
        }
        let response = self
            .client
            .post(&self.config.token_url)
            .json(&serde_json::json!({
                "appId": self.config.app_id,
                "clientSecret": self.config.app_secret,
            }))
            .send()
            .await
            .context("failed to request QQ access token")?;
        let status = response.status();
        let body = response.text().await?;
        anyhow::ensure!(
            status.is_success(),
            "QQ access token request failed with status {status}: {body}"
        );
        info!("retrieved qq access token successfully");
        let parsed: AccessTokenResponse = serde_json::from_str(&body)
            .with_context(|| format!("invalid QQ access token response: {body}"))?;
        *cache = Some(CachedToken {
            value: parsed.access_token.clone(),
            expires_at: std::time::Instant::now() + Duration::from_secs(parsed.expires_in),
        });
        Ok(parsed.access_token)
    }

    pub(crate) async fn invalidate_access_token(&self) {
        let mut cache = self.token_cache.lock().await;
        *cache = None;
    }

    async fn upload_file_direct(
        &self,
        path: &Path,
        openid: &str,
        file_type: u8,
        file_name: &str,
    ) -> Result<String> {
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let body = UploadFileBody {
            file_type,
            file_data: &encoded,
            file_name,
            srv_send_msg: false,
        };
        let response: UploadFileResponse = self
            .post_json(self.user_endpoint(openid, "files"), &body)
            .await?;
        Ok(response.file_info)
    }

    async fn upload_file_chunked(
        &self,
        openid: &str,
        path: &Path,
        file_type: u8,
        file_name: &str,
        file_size: u64,
    ) -> Result<String> {
        let hashes = self.compute_file_hashes(path, file_size).await?;
        let prepare = self
            .prepare_chunked_upload(openid, file_type, file_name, file_size, &hashes)
            .await?;
        anyhow::ensure!(
            !prepare.parts.is_empty(),
            "QQ upload_prepare returned no upload parts"
        );

        let block_size = prepare.block_size;
        let retry_timeout = prepare
            .retry_timeout
            .map(Duration::from_secs)
            .map(|timeout| timeout.min(MAX_PART_FINISH_RETRY_TIMEOUT));
        let concurrency = prepare
            .concurrency
            .map(|value| value as usize)
            .unwrap_or(DEFAULT_CHUNK_UPLOAD_CONCURRENCY)
            .clamp(1, MAX_CHUNK_UPLOAD_CONCURRENCY);
        let upload_id = prepare.upload_id.clone();

        stream::iter(prepare.parts.into_iter().map(|part| {
            let upload_id = upload_id.clone();
            async move {
                self.upload_single_part(
                    openid,
                    path,
                    file_size,
                    block_size,
                    &upload_id,
                    &part,
                    retry_timeout,
                )
                .await
            }
        }))
        .buffer_unordered(concurrency)
        .try_collect::<Vec<_>>()
        .await?;

        let response = self.complete_chunked_upload(openid, &upload_id).await?;
        Ok(response.file_info)
    }

    async fn prepare_chunked_upload(
        &self,
        openid: &str,
        file_type: u8,
        file_name: &str,
        file_size: u64,
        hashes: &FileHashes,
    ) -> Result<UploadPrepareResponse> {
        let body = UploadPrepareBody {
            file_type,
            file_name,
            file_size,
            md5: &hashes.md5,
            sha1: &hashes.sha1,
            md5_10m: &hashes.md5_10m,
        };
        self.post_json(self.user_endpoint(openid, "upload_prepare"), &body)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn upload_single_part(
        &self,
        openid: &str,
        path: &Path,
        file_size: u64,
        block_size: u64,
        upload_id: &str,
        part: &UploadPart,
        retry_timeout: Option<Duration>,
    ) -> Result<()> {
        let offset = part.index.saturating_sub(1).saturating_mul(block_size);
        let length = block_size.min(file_size.saturating_sub(offset));
        let bytes = read_file_chunk(path, offset, length).await?;
        let part_md5 = format!("{:x}", md5::compute(&bytes));
        self.put_presigned_part(&part.presigned_url, bytes).await?;
        self.finish_chunked_part(
            openid,
            upload_id,
            part.index,
            length,
            &part_md5,
            retry_timeout,
        )
        .await
    }

    async fn finish_chunked_part(
        &self,
        openid: &str,
        upload_id: &str,
        part_index: u64,
        block_size: u64,
        md5: &str,
        retry_timeout: Option<Duration>,
    ) -> Result<()> {
        let body = UploadPartFinishBody {
            upload_id,
            part_index,
            block_size,
            md5,
        };
        let url = self.user_endpoint(openid, "upload_part_finish");
        let (url, body) = (&url, &body);
        retry_with_backoff(
            PART_FINISH_MAX_RETRIES,
            1,
            "failed to finish chunk upload part",
            move || async move {
                match self
                    .post_json::<serde_json::Value, _>(url.clone(), body)
                    .await
                {
                    Ok(_) => RetryStep::Done(Ok(())),
                    Err(err) if qq_api_error_code(&err) == Some(PART_FINISH_RETRYABLE_CODE) => {
                        RetryStep::Done(
                            self.persistent_retry_part_finish(url.clone(), body, retry_timeout)
                                .await,
                        )
                    }
                    Err(err) => RetryStep::Retry(err),
                }
            },
        )
        .await
    }

    async fn persistent_retry_part_finish(
        &self,
        url: String,
        body: &UploadPartFinishBody<'_>,
        retry_timeout: Option<Duration>,
    ) -> Result<()> {
        let timeout = retry_timeout.unwrap_or(DEFAULT_PART_FINISH_RETRY_TIMEOUT);
        let deadline = Instant::now() + timeout;
        let mut attempts = 0u32;

        loop {
            match self
                .post_json::<serde_json::Value, _>(url.clone(), body)
                .await
            {
                Ok(_) => return Ok(()),
                Err(err) => {
                    if qq_api_error_code(&err) != Some(PART_FINISH_RETRYABLE_CODE) {
                        return Err(err);
                    }
                    attempts = attempts.saturating_add(1);
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(anyhow!(
                            "upload_part_finish 持续重试超时（{} 秒，{} 次重试）",
                            timeout.as_secs(),
                            attempts
                        ));
                    }
                    tokio::time::sleep(PART_FINISH_RETRYABLE_INTERVAL.min(deadline - now)).await;
                }
            }
        }
    }

    async fn complete_chunked_upload(
        &self,
        openid: &str,
        upload_id: &str,
    ) -> Result<UploadFileResponse> {
        let body = UploadCompleteBody { upload_id };
        let url = self.user_endpoint(openid, "files");
        let (url, body) = (&url, &body);
        retry_with_backoff(
            COMPLETE_UPLOAD_MAX_RETRIES,
            2,
            "failed to complete chunked upload",
            move || async move {
                match self
                    .post_json::<UploadFileResponse, _>(url.clone(), body)
                    .await
                {
                    Ok(response) => RetryStep::Done(Ok(response)),
                    Err(err) => RetryStep::Retry(err),
                }
            },
        )
        .await
    }

    async fn compute_file_hashes(&self, path: &Path, file_size: u64) -> Result<FileHashes> {
        let mut file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("failed to open {}", path.display()))?;
        let mut md5_ctx = md5::Context::new();
        let mut md5_10m_ctx = md5::Context::new();
        let mut sha1_ctx = Sha1::new();
        let mut remaining_10m = MD5_10M_SIZE;
        let mut buffer = vec![0u8; 64 * 1024];

        loop {
            let read = file
                .read(&mut buffer)
                .await
                .with_context(|| format!("failed to read {}", path.display()))?;
            if read == 0 {
                break;
            }
            let chunk = &buffer[..read];
            md5_ctx.consume(chunk);
            sha1_ctx.update(chunk);
            if remaining_10m > 0 {
                let take = remaining_10m.min(read as u64) as usize;
                md5_10m_ctx.consume(&chunk[..take]);
                remaining_10m = remaining_10m.saturating_sub(take as u64);
            }
        }

        let md5 = format!("{:x}", md5_ctx.compute());
        let md5_10m = if file_size <= MD5_10M_SIZE {
            md5.clone()
        } else {
            format!("{:x}", md5_10m_ctx.compute())
        };
        let sha1 = format!("{:x}", sha1_ctx.finalize());

        Ok(FileHashes { md5, sha1, md5_10m })
    }

    async fn put_presigned_part(&self, presigned_url: &str, bytes: Vec<u8>) -> Result<()> {
        let bytes = &bytes;
        retry_with_backoff(
            PART_UPLOAD_MAX_RETRIES,
            1,
            "failed to upload chunk to presigned url",
            move || async move {
                let response = self
                    .client
                    .put(presigned_url)
                    .timeout(PART_UPLOAD_TIMEOUT)
                    .header("Content-Length", bytes.len())
                    .body(bytes.clone())
                    .send()
                    .await;
                match response {
                    Ok(response) if response.status().is_success() => RetryStep::Done(Ok(())),
                    Ok(response) => {
                        let status = response.status();
                        let raw = response.text().await.unwrap_or_default();
                        RetryStep::Retry(anyhow!(
                            "QQ presigned upload failed with status {status}: {raw}"
                        ))
                    }
                    Err(err) => RetryStep::Retry(err.into()),
                }
            },
        )
        .await
    }

    /// Build a `/v2/users/{openid}/{path}` endpoint URL on the configured API base.
    fn user_endpoint(&self, openid: &str, path: &str) -> String {
        format!("{}/v2/users/{openid}/{path}", self.config.api_base_url)
    }

    async fn post_json<T, B>(&self, url: String, body: &B) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
        B: Serialize + ?Sized,
    {
        self.request_json(Method::POST, url, Some(body)).await
    }

    async fn request_json<T, B>(&self, method: Method, url: String, body: Option<&B>) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
        B: Serialize + ?Sized,
    {
        let token = self.get_access_token().await?;
        let mut request = self
            .client
            .request(method, url)
            .header("Authorization", format!("QQBot {token}"))
            .header("X-Union-Appid", &self.config.app_id);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?;
        let status = response.status();
        let raw = response.text().await?;
        if !status.is_success() {
            warn!(status = status.as_u16(), "QQ API request failed");
            return Err(QqApiError::from_response(status, raw).into());
        }
        if raw.is_empty() && StatusCode::NO_CONTENT == status {
            return serde_json::from_str("null").context("failed to parse empty response");
        }
        serde_json::from_str(&raw).with_context(|| format!("invalid QQ API response: {raw}"))
    }

    async fn next_msg_seq(&self, msg_id: &str) -> u32 {
        self.msg_seq.lock().await.next(msg_id)
    }
}

/// Outcome of one attempt inside [`retry_with_backoff`].
enum RetryStep<T> {
    /// Terminal: return this result immediately (success, or an error path
    /// that must not be retried by the backoff loop).
    Done(Result<T>),
    /// Record the error and retry after the backoff sleep (if attempts remain).
    Retry(anyhow::Error),
}

/// Run `op` up to `max_retries + 1` times, sleeping
/// `backoff_base_secs * (1 << attempt)` seconds between attempts.
async fn retry_with_backoff<T, F, Fut>(
    max_retries: u32,
    backoff_base_secs: u64,
    exhausted_message: &str,
    mut op: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = RetryStep<T>>,
{
    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 0..=max_retries {
        match op().await {
            RetryStep::Done(result) => return result,
            RetryStep::Retry(err) => {
                last_error = Some(err);
                if attempt < max_retries {
                    tokio::time::sleep(Duration::from_secs(backoff_base_secs * (1 << attempt)))
                        .await;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("{exhausted_message}")))
}

fn should_use_chunked_upload(file_size: u64) -> bool {
    file_size >= CHUNKED_UPLOAD_THRESHOLD_BYTES
}

fn normalized_upload_file_name(path: &Path, override_name: Option<&str>) -> Result<String> {
    let raw = override_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| path.file_name().and_then(|name| name.to_str()))
        .ok_or_else(|| anyhow!("invalid file name for {}", path.display()))?;
    let name = Path::new(raw)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    let sanitized = name
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '/' | '\\') {
                '_'
            } else {
                ch
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim();
    anyhow::ensure!(
        !trimmed.is_empty(),
        "invalid file name for {}",
        path.display()
    );
    Ok(trimmed.to_string())
}

fn ensure_upload_size_allowed(file_type: u8, file_size: u64) -> Result<()> {
    let limit = match file_type {
        1 => 30 * 1024 * 1024,
        2 => 100 * 1024 * 1024,
        3 => 20 * 1024 * 1024,
        4 => 100 * 1024 * 1024,
        _ => 100 * 1024 * 1024,
    };
    anyhow::ensure!(
        file_size <= limit,
        "QQ 文件过大：{file_size} bytes exceeds limit {limit} bytes for file_type {file_type}"
    );
    Ok(())
}

async fn read_file_chunk(path: &Path, offset: u64, length: u64) -> Result<Vec<u8>> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .with_context(|| format!("failed to seek {}", path.display()))?;
    let mut buffer = vec![0u8; length as usize];
    let mut read = 0usize;
    while read < buffer.len() {
        let n = file
            .read(&mut buffer[read..])
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        if n == 0 {
            break;
        }
        read += n;
    }
    buffer.truncate(read);
    Ok(buffer)
}

fn qq_api_error_code(err: &anyhow::Error) -> Option<i64> {
    err.downcast_ref::<QqApiError>()
        .and_then(|value| value.code)
}

fn default_expires_in() -> u64 {
    7200
}

fn deserialize_u64_value<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    parse_u64_value::<D::Error>(value)
}

fn deserialize_optional_u64_value<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    value.map(parse_u64_value::<D::Error>).transpose()
}

fn parse_u64_value<E>(value: Value) -> std::result::Result<u64, E>
where
    E: serde::de::Error,
{
    match value {
        Value::Number(number) => number
            .as_u64()
            .ok_or_else(|| E::custom("numeric value is not u64")),
        Value::String(text) => text
            .parse::<u64>()
            .map_err(|err| E::custom(format!("invalid numeric string: {err}"))),
        other => Err(E::custom(format!("invalid numeric value: {other}"))),
    }
}

fn split_text(text: &str, limit: usize) -> Vec<String> {
    if limit == 0 || text.chars().count() <= limit {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for line in text.lines() {
        // A single line can itself exceed the limit (long code line, URL, or an
        // unbroken paragraph). Hard-split it on char boundaries so no emitted
        // chunk is ever over the limit and rejected by QQ.
        for piece in split_long_line(line, limit) {
            let piece_len = piece.chars().count();
            let sep = usize::from(!current.is_empty());
            if !current.is_empty() && current_len + sep + piece_len > limit {
                chunks.push(std::mem::take(&mut current));
                current_len = 0;
            }
            if !current.is_empty() {
                current.push('\n');
                current_len += 1;
            }
            current.push_str(&piece);
            current_len += piece_len;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Split a single line into pieces of at most `limit` chars, cutting on char
/// boundaries. Returns the line unchanged when it already fits.
fn split_long_line(line: &str, limit: usize) -> Vec<String> {
    if limit == 0 || line.chars().count() <= limit {
        return vec![line.to_string()];
    }
    let mut pieces = Vec::new();
    let mut buf = String::new();
    let mut count = 0usize;
    for ch in line.chars() {
        buf.push(ch);
        count += 1;
        if count == limit {
            pieces.push(std::mem::take(&mut buf));
            count = 0;
        }
    }
    if !buf.is_empty() {
        pieces.push(buf);
    }
    pieces
}

pub(crate) fn estimate_text_chunk_count(text: &str) -> usize {
    split_text(text, 4500).len()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, method, path},
    };

    use super::{
        CHUNKED_UPLOAD_THRESHOLD_BYTES, MsgSeqCache, QqApiClient, QqConfig,
        estimate_text_chunk_count, normalized_upload_file_name, should_use_chunked_upload,
        split_text,
    };

    /// Client pointed at the mock server, with placeholder credentials.
    fn test_client(server: &MockServer) -> QqApiClient {
        QqApiClient::new(QqConfig {
            app_id: "app".into(),
            app_secret: "secret".into(),
            api_base_url: server.uri(),
            token_url: format!("{}/token", server.uri()),
        })
        .unwrap()
    }

    async fn mock_token(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "token-1", "expires_in": 7200
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    fn image_handle(
        scope: crate::qq::message_content::MessageScope,
        target: &str,
    ) -> super::UploadedImage {
        super::UploadedImage {
            file_info: "opaque-image-info".into(),
            ttl: 300,
            expires_at: Some(std::time::Instant::now() + std::time::Duration::from_secs(300)),
            scope,
            target_openid: target.into(),
        }
    }

    fn upload_error(
        result: std::result::Result<super::UploadedImage, super::ImageUploadError>,
    ) -> super::ImageUploadError {
        match result {
            Ok(_) => panic!("expected upload failure"),
            Err(error) => error,
        }
    }

    #[tokio::test]
    async fn command_menus_send_once_in_both_scopes_and_keep_delivery_identity() {
        use crate::{
            model::social::CommandButton,
            qq::message_content::{MessageScope, OutboundStyle, PassiveMessage},
        };
        let server = MockServer::start().await;
        mock_token(&server).await;
        let client = test_client(&server);
        let buttons = vec![CommandButton {
            label: "积极参与".into(),
            command: "/mode 3".into(),
        }];
        for (scope, resource) in [
            (MessageScope::Group, "groups"),
            (MessageScope::Private, "users"),
        ] {
            Mock::given(method("POST"))
                .and(path(format!("/v2/{resource}/menu-target/messages")))
                .and(wiremock::matchers::body_json(serde_json::json!({
                    "msg_type":2,"msg_id":"menu-anchor","msg_seq":3,
                    "markdown":{"content":"## 参与模式"},
                    "keyboard":{"content":{"rows":[{"buttons":[{
                        "id":"menu-0", "render_data":{"label":"积极参与","style":1},
                        "action":{"type":2,"permission":{"type":2},"data":"/mode 3","enter":false}
                    }]}]}}
                })))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"id":"menu-ack"})),
                )
                .expect(1)
                .mount(&server)
                .await;
            client
                .send_passive_message(
                    scope,
                    "menu-target",
                    "menu-anchor",
                    3,
                    PassiveMessage {
                        text: "## 参与模式",
                        style: OutboundStyle::Markdown,
                        help_buttons: false,
                        command_buttons: &buttons,
                        reference_idx: None,
                    },
                )
                .await
                .unwrap();
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn combined_markdown_and_native_quote_is_rejected_before_any_post() {
        use crate::qq::message_content::{MessageScope, OutboundStyle, PassiveMessage};
        use crate::qq::{GroupNotSentReason, GroupSendError};
        let server = MockServer::start().await;
        let client = test_client(&server);
        for scope in [MessageScope::Group, MessageScope::Private] {
            let content = PassiveMessage {
                text: "**正文**",
                style: OutboundStyle::Markdown,
                help_buttons: false,
                command_buttons: &[],
                reference_idx: Some("quote"),
            };
            for result in [
                client
                    .send_passive_message(scope, "target", "anchor", 1, content)
                    .await,
                client
                    .send_proactive_message(scope, "target", 1, content)
                    .await,
            ] {
                assert!(matches!(
                    result,
                    Err(GroupSendError::NotSent {
                        reason: GroupNotSentReason::InvalidRequest
                    })
                ));
            }
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejected_command_keyboard_does_not_retry_as_plain_text() {
        use crate::{
            model::social::CommandButton,
            qq::message_content::{MessageScope, OutboundStyle, PassiveMessage},
        };
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("POST"))
            .and(path("/v2/groups/menu-target/messages"))
            .respond_with(
                ResponseTemplate::new(400).set_body_json(
                    serde_json::json!({"code":304057,"message":"not allowed keyboard"}),
                ),
            )
            .expect(1)
            .mount(&server)
            .await;
        let buttons = vec![CommandButton {
            label: "帮助".into(),
            command: "/help".into(),
        }];
        let result = test_client(&server)
            .send_passive_message(
                MessageScope::Group,
                "menu-target",
                "anchor",
                1,
                PassiveMessage {
                    text: "帮助",
                    style: OutboundStyle::Markdown,
                    help_buttons: false,
                    command_buttons: &buttons,
                    reference_idx: None,
                },
            )
            .await;
        assert!(result.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn passive_image_upload_uses_scoped_endpoints_fixed_names_and_never_sends() {
        use crate::qq::message_content::MessageScope;
        use base64::Engine;
        let server = MockServer::start().await;
        mock_token(&server).await;
        let client = test_client(&server);
        for (scope, resource) in [
            (MessageScope::Group, "groups"),
            (MessageScope::Private, "users"),
        ] {
            for (bytes, filename) in [
                (&b"\x89PNG\r\n\x1a\nfixture"[..], "output.png"),
                (&b"\xff\xd8\xfffixture"[..], "output.jpg"),
            ] {
                let expected = serde_json::json!({
                    "file_type":1, "file_data":base64::engine::general_purpose::STANDARD.encode(bytes),
                    "file_name":filename, "srv_send_msg":false,
                });
                Mock::given(method("POST"))
                    .and(path(format!("/v2/{resource}/target-1/files")))
                    .and(wiremock::matchers::body_json(expected.clone()))
                    .and(wiremock::matchers::header("Authorization", "QQBot token-1"))
                    .and(wiremock::matchers::header("X-Union-Appid", "app"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(
                        serde_json::json!({"file_info":"opaque-image-info","ttl":300}),
                    ))
                    .expect(1)
                    .mount(&server)
                    .await;
                let uploaded = client
                    .upload_passive_image(scope, "target-1", bytes)
                    .await
                    .unwrap();
                assert_eq!(uploaded.ttl, 300);
                assert!(uploaded.expires_at.is_some());
            }
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 5);
        assert!(
            requests
                .iter()
                .all(|request| !request.url.path().ends_with("/messages"))
        );
    }

    #[tokio::test]
    async fn ordinary_file_upload_and_delivery_preserve_scope_name_anchor_and_ack() {
        use crate::qq::message_content::MessageScope;
        use base64::Engine;
        let server = MockServer::start().await;
        mock_token(&server).await;
        let client = test_client(&server);
        let bytes = b"# shared plan\n";
        Mock::given(method("POST")).and(path("/v2/groups/group-one/files"))
            .and(wiremock::matchers::body_json(serde_json::json!({"file_type":4,"file_data":base64::engine::general_purpose::STANDARD.encode(bytes),"file_name":"计划.md","srv_send_msg":false})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"file_info":"opaque-file","ttl":300})))
            .expect(1).mount(&server).await;
        let file = client
            .upload_passive_file(MessageScope::Group, "group-one", bytes, "计划.md")
            .await
            .unwrap();
        assert!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|r| !r.url.path().ends_with("/messages"))
        );
        assert!(
            client
                .send_passive_image_referencing(
                    MessageScope::Group,
                    "other-group",
                    "message",
                    1,
                    &file,
                    None
                )
                .await
                .is_err()
        );
        Mock::given(method("POST")).and(path("/v2/groups/group-one/messages"))
            .and(wiremock::matchers::body_json(serde_json::json!({"msg_type":7,"msg_id":"message","msg_seq":2,"media":{"file_info":"opaque-file"}})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id":"file-ack"})))
            .expect(1).mount(&server).await;
        assert!(
            client
                .send_passive_image_referencing(
                    MessageScope::Group,
                    "group-one",
                    "message",
                    2,
                    &file,
                    None
                )
                .await
                .is_ok()
        );
        assert!(
            client
                .upload_passive_file(MessageScope::Group, "group-one", bytes, "../private.txt")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn image_reference_is_separate_from_the_passive_anchor_in_both_scopes() {
        use crate::qq::message_content::MessageScope;
        let server = MockServer::start().await;
        mock_token(&server).await;
        let client = test_client(&server);
        for (scope, resource) in [
            (MessageScope::Group, "groups"),
            (MessageScope::Private, "users"),
        ] {
            Mock::given(method("POST")).and(path(format!("/v2/{resource}/target-1/messages")))
                .and(wiremock::matchers::body_json(serde_json::json!({"msg_type":7,"msg_id":"delivery-anchor","msg_seq":2,"media":{"file_info":"opaque-image-info"},"message_reference":{"message_id":"original-reference"}})))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id":"quoted-image-ack"})))
                .expect(1).mount(&server).await;
            client
                .send_passive_image_referencing(
                    scope,
                    "target-1",
                    "delivery-anchor",
                    2,
                    &image_handle(scope, "target-1"),
                    Some("original-reference"),
                )
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn passive_image_send_preserves_identity_and_requires_one_ack_for_both_scopes() {
        use crate::qq::message_content::MessageScope;
        let server = MockServer::start().await;
        mock_token(&server).await;
        let client = test_client(&server);
        for (scope, resource) in [
            (MessageScope::Group, "groups"),
            (MessageScope::Private, "users"),
        ] {
            Mock::given(method("POST"))
                .and(path(format!("/v2/{resource}/target-1/messages")))
                .and(wiremock::matchers::body_json(serde_json::json!({
                    "msg_type":7,"msg_id":"passive-image-message","msg_seq":17,
                    "media":{"file_info":"opaque-image-info"},
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"id":"media-ack","ext_info":{"ref_idx":"media-reference"}}),
                ))
                .expect(1)
                .mount(&server)
                .await;
            let ack = client
                .send_passive_image(
                    scope,
                    "target-1",
                    "passive-image-message",
                    17,
                    &image_handle(scope, "target-1"),
                )
                .await
                .unwrap();
            assert_eq!(ack.id, "media-ack");
            assert_eq!(ack.ref_idx(), Some("media-reference"));
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn passive_image_invalid_bytes_targets_and_size_never_contact_qq() {
        use crate::qq::message_content::MessageScope;
        let server = MockServer::start().await;
        let client = test_client(&server);
        let mut excessive = vec![0; super::MAX_PASSIVE_IMAGE_BYTES + 1];
        excessive[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        for bytes in [
            &b""[..],
            &b"GIF89a"[..],
            &b"not an image"[..],
            excessive.as_slice(),
        ] {
            assert_eq!(
                upload_error(
                    client
                        .upload_passive_image(MessageScope::Group, "target-1", bytes)
                        .await
                ),
                super::ImageUploadError::InvalidRequest
            );
        }
        for target in ["", " ", ".", "..", "id\0suffix"] {
            assert_eq!(
                upload_error(
                    client
                        .upload_passive_image(MessageScope::Private, target, b"\xff\xd8\xffdata")
                        .await
                ),
                super::ImageUploadError::InvalidRequest
            );
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn passive_image_exact_byte_limit_and_zero_ttl_are_supported() {
        use crate::qq::message_content::MessageScope;
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("POST"))
            .and(path("/v2/users/target-1/files"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"file_info":"opaque-image-info","ttl":0})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut bytes = vec![0; super::MAX_PASSIVE_IMAGE_BYTES];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let uploaded = test_client(&server)
            .upload_passive_image(MessageScope::Private, "target-1", &bytes)
            .await
            .unwrap();
        assert_eq!(uploaded.ttl, 0);
        assert!(uploaded.expires_at.is_none());
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn passive_image_upload_rejections_invalid_ttl_and_redirects_are_not_retried() {
        use super::ImageUploadError as E;
        use crate::qq::message_content::MessageScope;
        for (status, body, expected) in [
            (
                400,
                r#"{"code":850019,"message":"private-marker"}"#,
                E::Rejected {
                    status: 400,
                    code: Some(850019),
                },
            ),
            (503, "private-marker", E::UpstreamStatus { status: 503 }),
            (307, "private-marker", E::UpstreamStatus { status: 307 }),
            (200, "{}", E::InvalidResponse),
            (200, r#"{"file_info":" ","ttl":300}"#, E::InvalidResponse),
            (200, r#"{"file_info":"private-marker"}"#, E::InvalidResponse),
            (
                200,
                r#"{"file_info":"private-marker","ttl":-1}"#,
                E::InvalidResponse,
            ),
            (
                200,
                r#"{"file_info":"private-marker","ttl":"300"}"#,
                E::InvalidResponse,
            ),
            (
                200,
                r#"{"file_info":"private-marker","ttl":18446744073709551615}"#,
                E::InvalidResponse,
            ),
        ] {
            let server = MockServer::start().await;
            mock_token(&server).await;
            Mock::given(method("POST"))
                .and(path("/v2/groups/target-1/files"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("Location", "/must-not-follow")
                        .set_body_string(body),
                )
                .expect(1)
                .mount(&server)
                .await;
            let error = upload_error(
                test_client(&server)
                    .upload_passive_image(MessageScope::Group, "target-1", b"\xff\xd8\xffdata")
                    .await,
            );
            assert_eq!(error, expected);
            assert!(!format!("{error:?} {error}").contains("private-marker"));
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn passive_image_send_rejections_missing_acks_and_unknowns_are_not_retried() {
        use super::{GroupSendError as E, GroupSendUnknownReason as U};
        use crate::qq::message_content::MessageScope;
        for scope in [MessageScope::Group, MessageScope::Private] {
            for (status, body, expected) in [
                (
                    400,
                    r#"{"code":304080,"message":"private-marker"}"#,
                    E::Rejected {
                        status: 400,
                        code: Some(304080),
                    },
                ),
                (
                    200,
                    "{}",
                    E::Unknown {
                        reason: U::InvalidAcknowledgement,
                    },
                ),
                (
                    200,
                    r#"{"id":" "}"#,
                    E::Unknown {
                        reason: U::InvalidAcknowledgement,
                    },
                ),
                (
                    200,
                    r#"{"id":12}"#,
                    E::Unknown {
                        reason: U::InvalidAcknowledgement,
                    },
                ),
                (
                    503,
                    "private-marker",
                    E::Unknown {
                        reason: U::UpstreamStatus { status: 503 },
                    },
                ),
                (
                    307,
                    "private-marker",
                    E::Unknown {
                        reason: U::UpstreamStatus { status: 307 },
                    },
                ),
            ] {
                let server = MockServer::start().await;
                mock_token(&server).await;
                let resource = if scope == MessageScope::Group {
                    "groups"
                } else {
                    "users"
                };
                Mock::given(method("POST"))
                    .and(path(format!("/v2/{resource}/target-1/messages")))
                    .respond_with(
                        ResponseTemplate::new(status)
                            .insert_header("Location", "/must-not-follow")
                            .set_body_string(body),
                    )
                    .expect(1)
                    .mount(&server)
                    .await;
                let error = test_client(&server)
                    .send_passive_image(
                        scope,
                        "target-1",
                        "source",
                        3,
                        &image_handle(scope, "target-1"),
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error, expected);
                assert!(!format!("{error:?} {error}").contains("private-marker"));
                assert_eq!(server.received_requests().await.unwrap().len(), 2);
            }
        }
    }

    #[tokio::test]
    async fn passive_image_timeout_keeps_upload_separate_from_unknown_delivery() {
        use crate::qq::message_content::MessageScope;
        for upload in [true, false] {
            let server = MockServer::start().await;
            mock_token(&server).await;
            Mock::given(method("POST"))
                .and(path(if upload {
                    "/v2/groups/target-1/files"
                } else {
                    "/v2/groups/target-1/messages"
                }))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_delay(std::time::Duration::from_millis(250))
                        .set_body_json(
                            serde_json::json!({"id":"ack","file_info":"opaque","ttl":300}),
                        ),
                )
                .expect(1)
                .mount(&server)
                .await;
            let mut client = test_client(&server);
            client.group_client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_millis(75))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            if upload {
                assert_eq!(
                    upload_error(
                        client
                            .upload_passive_image(
                                MessageScope::Group,
                                "target-1",
                                b"\xff\xd8\xffdata"
                            )
                            .await
                    ),
                    super::ImageUploadError::Timeout
                );
            } else {
                assert_eq!(
                    client
                        .send_passive_image(
                            MessageScope::Group,
                            "target-1",
                            "source",
                            1,
                            &image_handle(MessageScope::Group, "target-1")
                        )
                        .await
                        .unwrap_err(),
                    super::GroupSendError::Unknown {
                        reason: super::GroupSendUnknownReason::Timeout
                    }
                );
            }
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn passive_image_expired_or_mismatched_capabilities_are_not_sent() {
        use crate::qq::message_content::MessageScope;
        let server = MockServer::start().await;
        let client = test_client(&server);
        let valid = image_handle(MessageScope::Group, "target-1");
        let mut expired = valid.clone();
        expired.expires_at = Some(std::time::Instant::now());
        for (scope, target, message, seq, image) in [
            (MessageScope::Private, "target-1", "source", 1, &valid),
            (MessageScope::Group, "other-target", "source", 1, &valid),
            (MessageScope::Group, "target-1", "", 1, &valid),
            (MessageScope::Group, "target-1", "source", 0, &valid),
            (MessageScope::Group, "target-1", "source", 1, &expired),
        ] {
            assert_eq!(
                client
                    .send_passive_image(scope, target, message, seq, image)
                    .await
                    .unwrap_err(),
                super::GroupSendError::NotSent {
                    reason: super::GroupNotSentReason::InvalidRequest
                }
            );
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn passive_image_ttl_is_rechecked_after_awaiting_token_refresh() {
        use crate::qq::message_content::MessageScope;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(100))
                    .set_body_json(serde_json::json!({"access_token":"token-1","expires_in":7200})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut image = image_handle(MessageScope::Group, "target-1");
        image.expires_at = Some(std::time::Instant::now() + std::time::Duration::from_millis(40));
        let error = test_client(&server)
            .send_passive_image(MessageScope::Group, "target-1", "source", 1, &image)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            super::GroupSendError::NotSent {
                reason: super::GroupNotSentReason::InvalidRequest
            }
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn proactive_text_is_one_post_without_passive_identifiers_or_fallback() {
        use crate::qq::message_content::MessageScope;
        for (scope, route) in [
            (MessageScope::Group, "groups"),
            (MessageScope::Private, "users"),
        ] {
            for status in [200, 403, 503] {
                let server = MockServer::start().await;
                mock_token(&server).await;
                Mock::given(method("POST"))
                    .and(path(format!("/v2/{route}/target/messages")))
                    .and(body_partial_json(
                        serde_json::json!({"msg_type":0,"msg_seq":7,"content":"scheduled result"}),
                    ))
                    .respond_with(
                        ResponseTemplate::new(status)
                            .set_body_json(serde_json::json!({"id":"active-ack","code":40034005})),
                    )
                    .expect(1)
                    .mount(&server)
                    .await;
                let result = test_client(&server)
                    .send_proactive_text(scope, "target", 7, "scheduled result")
                    .await;
                match status {
                    200 => assert_eq!(result.unwrap().id, "active-ack"),
                    403 => assert!(matches!(
                        result,
                        Err(super::GroupSendError::Rejected { .. })
                    )),
                    _ => assert!(matches!(result, Err(super::GroupSendError::Unknown { .. }))),
                }
                let requests = server.received_requests().await.unwrap();
                assert_eq!(requests.len(), 2);
                let body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
                assert!(body.get("msg_id").is_none());
                assert!(body.get("event_id").is_none());
                assert!(body.get("markdown").is_none());
            }
        }
    }

    #[tokio::test]
    async fn proactive_markdown_keeps_links_keyboard_without_passive_anchor_or_retry() {
        use crate::qq::message_content::{MessageScope, OutboundStyle, PassiveMessage};
        for (scope, route) in [
            (MessageScope::Group, "groups"),
            (MessageScope::Private, "users"),
        ] {
            for status in [200, 403, 503] {
                let server = MockServer::start().await;
                mock_token(&server).await;
                Mock::given(method("POST"))
                    .and(path(format!("/v2/{route}/target/messages")))
                    .respond_with(
                        ResponseTemplate::new(status)
                            .set_body_json(serde_json::json!({"id":"active-ack","code":40034124})),
                    )
                    .expect(1)
                    .mount(&server)
                    .await;
                let buttons = [crate::model::social::CommandButton {
                    label: "查看任务进度".into(),
                    command: "/status 20261025-143205".into(),
                }];
                let text = "**已核对** [官方说明](https://example.invalid/)";
                let result = test_client(&server)
                    .send_proactive_message(
                        scope,
                        "target",
                        7,
                        PassiveMessage {
                            text,
                            style: OutboundStyle::Markdown,
                            help_buttons: false,
                            command_buttons: &buttons,
                            reference_idx: None,
                        },
                    )
                    .await;
                match status {
                    200 => assert_eq!(result.unwrap().id, "active-ack"),
                    403 => assert!(matches!(
                        result,
                        Err(super::GroupSendError::Rejected { .. })
                    )),
                    _ => assert!(matches!(result, Err(super::GroupSendError::Unknown { .. }))),
                }
                let requests = server.received_requests().await.unwrap();
                assert_eq!(requests.len(), 2);
                let body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
                assert_eq!(body["msg_type"], 2);
                assert_eq!(body["markdown"]["content"], text);
                assert!(body.get("message_reference").is_none());
                assert_eq!(
                    body["keyboard"]["content"]["rows"][0]["buttons"][0]["action"]["data"],
                    buttons[0].command
                );
                assert!(body.get("msg_id").is_none());
                assert!(body.get("event_id").is_none());
                assert!(body.get("content").is_none());
            }
        }
    }

    #[tokio::test]
    async fn group_native_mentions_reach_qq_unchanged_in_both_styles_and_delivery_routes() {
        use crate::qq::message_content::{MessageScope, OutboundStyle, PassiveMessage};
        for proactive in [false, true] {
            for style in [OutboundStyle::Text, OutboundStyle::Markdown] {
                let server = MockServer::start().await;
                mock_token(&server).await;
                Mock::given(method("POST"))
                    .and(path("/v2/groups/group-target/messages"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_json(serde_json::json!({"id":"mention-ack"})),
                    )
                    .expect(1)
                    .mount(&server)
                    .await;
                let text = "<qqbot-at-user id=\"member-target\" />\n有人找你～";
                let reference = (style == OutboundStyle::Text).then_some("native-quote-index");
                let content = PassiveMessage {
                    text,
                    style,
                    help_buttons: false,
                    command_buttons: &[],
                    reference_idx: reference,
                };
                let client = test_client(&server);
                let result = if proactive {
                    client
                        .send_proactive_message(MessageScope::Group, "group-target", 1, content)
                        .await
                } else {
                    client
                        .send_passive_message(
                            MessageScope::Group,
                            "group-target",
                            "anchor",
                            1,
                            content,
                        )
                        .await
                };
                assert_eq!(result.unwrap().id, "mention-ack");
                let requests = server.received_requests().await.unwrap();
                assert_eq!(requests.len(), 2);
                let body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
                assert_eq!(
                    if style == OutboundStyle::Text {
                        &body["content"]
                    } else {
                        &body["markdown"]["content"]
                    },
                    text
                );
                assert_eq!(body.get("msg_id").is_none(), proactive);
                assert_eq!(body["message_reference"]["message_id"].as_str(), reference);
            }
        }
    }

    #[tokio::test]
    async fn proactive_uploaded_media_uses_no_passive_anchor_in_either_scope() {
        use crate::qq::message_content::MessageScope;
        for (scope, route) in [
            (MessageScope::Group, "groups"),
            (MessageScope::Private, "users"),
        ] {
            let server = MockServer::start().await;
            mock_token(&server).await;
            let image = image_handle(scope, "target");
            Mock::given(method("POST"))
                .and(path(format!("/v2/{route}/target/messages")))
                .and(wiremock::matchers::body_json(serde_json::json!({
                    "msg_type":7,"msg_seq":37,"media":{"file_info":image.file_info}
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id":"media-ack","ext_info":{"ref_idx":"media-reference"}
                })))
                .expect(1)
                .mount(&server)
                .await;
            let ack = test_client(&server)
                .send_proactive_image(scope, "target", 37, &image)
                .await
                .unwrap();
            assert_eq!(ack.id, "media-ack");
            assert_eq!(ack.ref_idx(), Some("media-reference"));
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 2);
            let body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
            for absent in [
                "msg_id",
                "event_id",
                "message_reference",
                "markdown",
                "content",
            ] {
                assert!(body.get(absent).is_none());
            }
        }
    }

    #[tokio::test]
    async fn proactive_file_delivery_accepts_the_scoped_handle_from_file_upload() {
        use crate::qq::message_content::MessageScope;
        for (scope, route) in [
            (MessageScope::Group, "groups"),
            (MessageScope::Private, "users"),
        ] {
            let server = MockServer::start().await;
            mock_token(&server).await;
            Mock::given(method("POST"))
                .and(path(format!("/v2/{route}/target/files")))
                .and(body_partial_json(
                    serde_json::json!({"file_type":4,"file_name":"plan.md","srv_send_msg":false}),
                ))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(
                        serde_json::json!({"file_info":"opaque-file-handle","ttl":300}),
                    ),
                )
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path(format!("/v2/{route}/target/messages")))
                .and(wiremock::matchers::body_json(serde_json::json!({"msg_type":7,"msg_seq":41,"media":{"file_info":"opaque-file-handle"}})))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id":"file-ack"})))
                .expect(1).mount(&server).await;
            let client = test_client(&server);
            let file = client
                .upload_passive_file(scope, "target", b"# Plan\n", "plan.md")
                .await
                .unwrap();
            let ack = client
                .send_proactive_image(scope, "target", 41, &file)
                .await
                .unwrap();
            assert_eq!(ack.id, "file-ack");
            assert_eq!(server.received_requests().await.unwrap().len(), 3);
        }
    }

    #[tokio::test]
    async fn proactive_uploaded_media_rejects_wrong_scope_target_sequence_or_expiry_without_io() {
        use crate::qq::message_content::MessageScope;
        let server = MockServer::start().await;
        let client = test_client(&server);
        let image = image_handle(MessageScope::Group, "target");
        let mut expired = image.clone();
        expired.expires_at = Some(std::time::Instant::now());
        for (scope, target, seq, handle) in [
            (MessageScope::Private, "target", 1, &image),
            (MessageScope::Group, "other-target", 1, &image),
            (MessageScope::Group, "", 1, &image),
            (MessageScope::Group, "target", 0, &image),
            (MessageScope::Group, "target", 1, &expired),
        ] {
            assert_eq!(
                client
                    .send_proactive_image(scope, target, seq, handle)
                    .await
                    .unwrap_err(),
                super::GroupSendError::NotSent {
                    reason: super::GroupNotSentReason::InvalidRequest
                }
            );
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn proactive_uploaded_media_unknown_and_rejection_results_never_trigger_resend() {
        use super::{GroupSendError as E, GroupSendUnknownReason as U};
        use crate::qq::message_content::MessageScope;
        for (scope, route) in [
            (MessageScope::Group, "groups"),
            (MessageScope::Private, "users"),
        ] {
            for (status, body, expected) in [
                (
                    403,
                    r#"{"code":40034005,"message":"private-upstream-text"}"#,
                    E::Rejected {
                        status: 403,
                        code: Some(40034005),
                    },
                ),
                (
                    200,
                    "{}",
                    E::Unknown {
                        reason: U::InvalidAcknowledgement,
                    },
                ),
                (
                    503,
                    "private-upstream-text",
                    E::Unknown {
                        reason: U::UpstreamStatus { status: 503 },
                    },
                ),
            ] {
                let server = MockServer::start().await;
                mock_token(&server).await;
                Mock::given(method("POST"))
                    .and(path(format!("/v2/{route}/target/messages")))
                    .respond_with(ResponseTemplate::new(status).set_body_string(body))
                    .expect(1)
                    .mount(&server)
                    .await;
                let error = test_client(&server)
                    .send_proactive_image(scope, "target", 42, &image_handle(scope, "target"))
                    .await
                    .unwrap_err();
                assert_eq!(error, expected);
                assert!(!format!("{error:?} {error}").contains("private-upstream-text"));
                assert_eq!(server.received_requests().await.unwrap().len(), 2);
            }
        }
    }

    #[tokio::test]
    async fn proactive_uploaded_media_ttl_is_rechecked_after_token_refresh() {
        use crate::qq::message_content::MessageScope;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(100))
                    .set_body_json(serde_json::json!({"access_token":"token-1","expires_in":7200})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut image = image_handle(MessageScope::Private, "target");
        image.expires_at = Some(std::time::Instant::now() + std::time::Duration::from_millis(40));
        assert_eq!(
            test_client(&server)
                .send_proactive_image(MessageScope::Private, "target", 43, &image)
                .await
                .unwrap_err(),
            super::GroupSendError::NotSent {
                reason: super::GroupNotSentReason::InvalidRequest
            }
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn group_plain_text_preserves_explicit_sequences_and_returns_each_ack() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        for seq in [1, 2] {
            Mock::given(method("POST"))
                .and(path("/v2/groups/group-1/messages"))
                .and(body_partial_json(serde_json::json!({
                    "msg_type": 0, "msg_id": "message-1", "msg_seq": seq,
                    "content": format!("bubble {seq}"),
                    "message_reference": {"message_id": "ref-original"}
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": format!("sent-{seq}"), "timestamp": "2026-10-22T12:00:00+08:00",
                    "ext_info": {"ref_idx": format!("ref-{seq}")}
                })))
                .expect(1)
                .mount(&server)
                .await;
        }
        let client = test_client(&server);
        for seq in [1, 2] {
            let ack = client
                .send_group_text(
                    "group-1",
                    "message-1",
                    seq,
                    &format!("bubble {seq}"),
                    Some("ref-original"),
                )
                .await
                .unwrap();
            assert_eq!(ack.id, format!("sent-{seq}"));
            assert_eq!(ack.ref_idx(), Some(format!("ref-{seq}").as_str()));
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        for request in requests
            .iter()
            .filter(|request| request.url.path() != "/token")
        {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert!(body.get("markdown").is_none());
        }
    }

    #[tokio::test]
    async fn group_target_is_one_encoded_path_segment() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "sent-1"})),
            )
            .with_priority(10)
            .expect(1)
            .mount(&server)
            .await;
        test_client(&server)
            .send_group_text("group/../other?x=1#fragment", "message-1", 4, "hi", None)
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let sent = requests
            .iter()
            .find(|request| request.url.path() != "/token")
            .unwrap();
        assert_eq!(
            sent.url.path(),
            "/v2/groups/group%2F..%2Fother%3Fx=1%23fragment/messages"
        );
        assert!(sent.url.query().is_none());
        assert!(sent.url.fragment().is_none());
    }

    #[tokio::test]
    async fn group_rejection_does_not_fallback_or_expose_response_content() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("POST"))
            .and(path("/v2/groups/group-1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "code": 40034005, "message": "private-marker"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let error = test_client(&server)
            .send_group_text("group-1", "message-1", 1, "hi", None)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            super::GroupSendError::Rejected {
                status: 400,
                code: Some(40034005)
            }
        );
        assert!(!error.to_string().contains("private-marker"));
        assert!(!format!("{error:?}").contains("private-marker"));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn group_invalid_ack_and_server_failure_are_unknown_and_not_retried() {
        for (status, body, reason) in [
            (
                200,
                "private-marker",
                super::GroupSendUnknownReason::InvalidAcknowledgement,
            ),
            (
                200,
                "{}",
                super::GroupSendUnknownReason::InvalidAcknowledgement,
            ),
            (
                200,
                r#"{"id":""}"#,
                super::GroupSendUnknownReason::InvalidAcknowledgement,
            ),
            (
                503,
                "private-marker",
                super::GroupSendUnknownReason::UpstreamStatus { status: 503 },
            ),
        ] {
            let server = MockServer::start().await;
            mock_token(&server).await;
            Mock::given(method("POST"))
                .and(path("/v2/groups/group-1/messages"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .expect(1)
                .mount(&server)
                .await;
            let error = test_client(&server)
                .send_group_text("group-1", "message-1", 1, "hi", None)
                .await
                .unwrap_err();
            assert_eq!(error, super::GroupSendError::Unknown { reason });
            assert!(!format!("{error:?}").contains("private-marker"));
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn group_timeout_is_unknown_without_retry() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("POST"))
            .and(path("/v2/groups/group-1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(250))
                    .set_body_json(serde_json::json!({"id": "sent-1"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut client = test_client(&server);
        client.group_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(75))
            .build()
            .unwrap();
        let error = client
            .send_group_text("group-1", "message-1", 1, "hi", None)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            super::GroupSendError::Unknown {
                reason: super::GroupSendUnknownReason::Timeout
            }
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn group_redirect_is_not_followed_or_retried() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("POST"))
            .and(path("/v2/groups/group-1/messages"))
            .respond_with(ResponseTemplate::new(307).insert_header("Location", "/unexpected"))
            .expect(1)
            .mount(&server)
            .await;
        let error = test_client(&server)
            .send_group_text("group-1", "message-1", 1, "hi", None)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            super::GroupSendError::Unknown {
                reason: super::GroupSendUnknownReason::UpstreamStatus { status: 307 }
            }
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn group_invalid_local_request_never_contacts_qq() {
        let server = MockServer::start().await;
        let client = test_client(&server);
        for group in ["", ".", ".."] {
            assert!(matches!(
                client
                    .send_group_text(group, "message-1", 1, "hi", None)
                    .await,
                Err(super::GroupSendError::NotSent {
                    reason: super::GroupNotSentReason::InvalidRequest
                })
            ));
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reliable_private_text_uses_one_plain_payload_and_explicit_sequence() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        let text = "私聊回复".repeat(1000);
        Mock::given(method("POST"))
            .and(path("/v2/users/user-1/messages"))
            .and(body_partial_json(serde_json::json!({
                "msg_type": 0, "msg_id": "private-1", "msg_seq": 4, "content": text
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "sent-private-1", "ext_info": { "ref_idx": "private-ref" }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let ack = test_client(&server)
            .send_private_text("user-1", "private-1", 4, &text)
            .await
            .unwrap();
        assert_eq!(ack.id, "sent-private-1");
        assert_eq!(ack.ref_idx(), Some("private-ref"));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let sent = requests
            .iter()
            .find(|request| request.url.path() != "/token")
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
        assert!(body.get("markdown").is_none());
        assert!(body.get("message_reference").is_none());
        assert_eq!(body.as_object().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn reliable_private_target_is_one_encoded_path_segment() {
        let server = MockServer::start().await;
        mock_token(&server).await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "sent-1"})),
            )
            .with_priority(10)
            .expect(1)
            .mount(&server)
            .await;
        test_client(&server)
            .send_private_text("user/../other?x=1#fragment", "private-1", 1, "hello")
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let sent = requests
            .iter()
            .find(|request| request.url.path() != "/token")
            .unwrap();
        assert_eq!(
            sent.url.path(),
            "/v2/users/user%2F..%2Fother%3Fx=1%23fragment/messages"
        );
        assert!(sent.url.query().is_none());
    }

    #[tokio::test]
    async fn reliable_private_rejection_unknown_ack_and_server_error_are_not_retried() {
        for (status, body, expected) in [
            (
                400,
                r#"{"code":40034005,"message":"private-marker"}"#,
                super::GroupSendError::Rejected {
                    status: 400,
                    code: Some(40034005),
                },
            ),
            (
                200,
                "{}",
                super::GroupSendError::Unknown {
                    reason: super::GroupSendUnknownReason::InvalidAcknowledgement,
                },
            ),
            (
                200,
                r#"{"id":" "}"#,
                super::GroupSendError::Unknown {
                    reason: super::GroupSendUnknownReason::InvalidAcknowledgement,
                },
            ),
            (
                503,
                "private-marker",
                super::GroupSendError::Unknown {
                    reason: super::GroupSendUnknownReason::UpstreamStatus { status: 503 },
                },
            ),
        ] {
            let server = MockServer::start().await;
            mock_token(&server).await;
            Mock::given(method("POST"))
                .and(path("/v2/users/user-1/messages"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .expect(1)
                .mount(&server)
                .await;
            let error = test_client(&server)
                .send_private_text("user-1", "private-1", 1, "hello")
                .await
                .unwrap_err();
            assert_eq!(error, expected);
            assert!(!format!("{error:?}").contains("private-marker"));
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn reliable_private_timeout_and_redirect_never_send_another_request() {
        for redirect in [false, true] {
            let server = MockServer::start().await;
            mock_token(&server).await;
            let (response, reason) = if redirect {
                (
                    ResponseTemplate::new(307).insert_header("Location", "/unexpected"),
                    super::GroupSendUnknownReason::UpstreamStatus { status: 307 },
                )
            } else {
                (
                    ResponseTemplate::new(200)
                        .set_delay(std::time::Duration::from_millis(250))
                        .set_body_json(serde_json::json!({"id":"sent-1"})),
                    super::GroupSendUnknownReason::Timeout,
                )
            };
            Mock::given(method("POST"))
                .and(path("/v2/users/user-1/messages"))
                .respond_with(response)
                .expect(1)
                .mount(&server)
                .await;
            let mut client = test_client(&server);
            client.group_client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_millis(75))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            assert_eq!(
                client
                    .send_private_text("user-1", "private-1", 1, "hello")
                    .await
                    .unwrap_err(),
                super::GroupSendError::Unknown { reason }
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn reliable_private_invalid_or_overlong_input_is_not_split_or_sent() {
        let server = MockServer::start().await;
        let client = test_client(&server);
        let long = "x".repeat(4501);
        for (user, message, seq, text) in [
            ("", "private-1", 1, "hello"),
            ("..", "private-1", 1, "hello"),
            ("user-1", "", 1, "hello"),
            ("user-1", "private-1", 0, "hello"),
            ("user-1", "private-1", 1, long.as_str()),
        ] {
            assert_eq!(
                client
                    .send_private_text(user, message, seq, text)
                    .await
                    .unwrap_err(),
                super::GroupSendError::NotSent {
                    reason: super::GroupNotSentReason::InvalidRequest
                }
            );
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn chunked_upload_threshold_matches_large_files() {
        assert!(!should_use_chunked_upload(
            CHUNKED_UPLOAD_THRESHOLD_BYTES - 1
        ));
        assert!(should_use_chunked_upload(CHUNKED_UPLOAD_THRESHOLD_BYTES));
    }

    #[test]
    fn normalizes_override_file_name() {
        let name =
            normalized_upload_file_name(std::path::Path::new("/tmp/report.bin"), Some("../a.txt"))
                .unwrap();
        assert_eq!(name, "a.txt");
    }

    #[test]
    fn estimates_text_chunks() {
        let text = format!("{}\n{}", "a".repeat(3000), "b".repeat(3000));
        assert_eq!(estimate_text_chunk_count(&text), 2);
    }

    #[test]
    fn split_text_hard_splits_a_single_over_limit_line() {
        // A single line longer than the limit must still be broken up so no
        // emitted chunk exceeds the limit (QQ rejects over-limit content).
        let text = "x".repeat(10_000);
        let chunks = split_text(&text, 4500);
        assert!(chunks.len() >= 3);
        assert!(chunks.iter().all(|c| c.chars().count() <= 4500));
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn split_text_bounds_mixed_long_and_short_lines() {
        let text = format!("{}\nshort tail", "y".repeat(9000));
        let chunks = split_text(&text, 4500);
        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|c| c.chars().count() <= 4500));
        // Content preservation is only modulo line separators: `split_text`
        // rebuilds chunks from `text.lines()`, so a separator that lands on a
        // chunk boundary is dropped, and a hard split inside an over-limit
        // line introduces a boundary that was never a separator. Every
        // non-separator char must survive, in order.
        assert_eq!(
            chunks.concat().replace('\n', ""),
            text.replace('\n', ""),
            "split_text dropped or reordered content"
        );
        assert_eq!(chunks.last().map(String::as_str), Some("short tail"));
    }

    #[test]
    fn msg_seq_cache_is_monotonic_and_bounded() {
        let mut cache = MsgSeqCache::default();
        assert_eq!(cache.next("a"), 1);
        assert_eq!(cache.next("a"), 2);
        assert_eq!(cache.next("b"), 1);
        for i in 0..(MsgSeqCache::CAP * 2) {
            cache.next(&format!("k{i}"));
        }
        assert!(cache.counters.len() <= MsgSeqCache::CAP);
        assert_eq!(cache.order.len(), cache.counters.len());
    }

    #[tokio::test]
    async fn proactive_markdown_sends_markdown_payload() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "token-1",
                "expires_in": 7200
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/users/u1/messages"))
            .and(body_partial_json(serde_json::json!({
                "msg_type": 2,
                "markdown": {
                    "content": "# AI 新闻早餐\n- item"
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;

        let client = test_client(&server);

        client
            .send_markdown_proactive("u1", "# AI 新闻早餐\n- item")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn uploads_large_file_with_chunked_flow() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "token-1",
                "expires_in": 7200
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/users/u1/upload_prepare"))
            .and(body_partial_json(serde_json::json!({
                "file_type": 4,
                "file_name": "report.bin"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "upload_id": "upload-1",
                "block_size": 3145728,
                "parts": [
                    {"index": 1, "presigned_url": format!("{}/upload/1", server.uri())},
                    {"index": 2, "presigned_url": format!("{}/upload/2", server.uri())}
                ],
                "concurrency": 2,
                "retry_timeout": 60
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/upload/1"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/upload/2"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/users/u1/upload_part_finish"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/users/u1/files"))
            .and(body_partial_json(serde_json::json!({
                "upload_id": "upload-1"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "file_info": "file-info"
            })))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let path = dir.path().join("report.bin");
        fs::write(
            &path,
            vec![b'x'; (CHUNKED_UPLOAD_THRESHOLD_BYTES + 128) as usize],
        )
        .unwrap();

        let client = test_client(&server);

        let file_info = client
            .upload_file("u1", &path, 4, Some("report.bin"))
            .await
            .unwrap();
        assert_eq!(file_info, "file-info");
    }
}
