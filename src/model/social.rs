//! Application-owned group history and request revisions, independent of Codex history.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParticipationMode {
    #[default]
    #[serde(rename = "mode-1")]
    Mentions,
    #[serde(rename = "mode-2")]
    Helpful,
    #[serde(rename = "mode-3")]
    Social,
    #[serde(rename = "mode-all")]
    All,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Quote {
    pub reference_idx: Option<String>,
    pub author_id: Option<String>,
    pub author_name: Option<String>,
    pub text: String,
    pub from_bot: bool,
    pub timestamp_ms: Option<i64>,
    pub time_source: MessageTimeSource,
    pub message_type: Option<u32>,
    pub source: QuoteSource,
    pub attachment_kinds: Vec<String>,
    pub images: Vec<ImageAttachment>,
    pub voices: Vec<VoiceInput>,
    pub parts: Vec<Quote>,
    pub truncated: bool,
    /// Application-owned snapshots recovered using this chat's sent ACK only.
    pub local_images: Vec<OutputImage>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteSource {
    #[default]
    Platform,
    ConversationHistory,
    Unavailable,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageTimeSource {
    #[default]
    Unknown,
    QqMessage,
    ReceivedAt,
    QqSendAck,
    SendAcknowledgedAt,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatMessage {
    pub id: String,
    pub author_id: String,
    pub author_name: Option<String>,
    pub text: String,
    pub timestamp_ms: i64,
    pub time_source: MessageTimeSource,
    pub at_bot: bool,
    pub reference_idx: Option<String>,
    pub quotes: Vec<Quote>,
    pub mentioned_user_ids: Vec<String>,
    pub attachment_kinds: Vec<String>,
    /// Private transport metadata; prompt projections must never include URLs.
    pub images: Vec<ImageAttachment>,
    /// QQ ASR is reference text, never an original-text command or permission.
    #[serde(default)]
    pub voices: Vec<VoiceInput>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceSource {
    #[default]
    QqAsr,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceStatus {
    Available,
    #[default]
    MissingAsr,
    BlankAsr,
    TooLong,
    TooManyAttachments,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceInput {
    pub source: VoiceSource,
    pub status: VoiceStatus,
    pub transcript: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub url: String,
    pub content_type: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    #[default]
    Pending,
    Running,
    Decided,
    Bypassed,
    Failed,
}

impl DecisionStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Decided | Self::Bypassed)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    #[default]
    Silent,
    NewRequest,
    Append,
    Revise,
    Cancel,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChoiceRecord {
    pub choice: String,
    pub confidence: f64,
    pub probabilities: BTreeMap<String, f64>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Decision {
    pub action: Action,
    pub request_id: Option<u64>,
    /// None addresses the group; Some preserves the actual selected participant.
    pub audience_id: Option<String>,
    pub style: String,
    pub source: String,
    pub model: Option<String>,
    pub policy_version: u32,
    pub snapshot_through_seq: u64,
    pub request_revisions: BTreeMap<u64, u64>,
    pub judgments: BTreeMap<String, ChoiceRecord>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MessageRecord {
    pub seq: u64,
    pub message: ChatMessage,
    pub status: DecisionStatus,
    pub decision: Option<Decision>,
    pub applied_request: Option<u64>,
    pub applied_revision: Option<u64>,
    /// Judgment tasks started, not HTTP retry count; None for legacy records.
    pub jev_attempts: Option<u32>,
    /// Fixed application code, never a raw service error.
    pub application: Option<String>,
    pub attempts: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    #[default]
    Queued,
    Generating,
    Ready,
    Sending,
    Completed,
    Cancelled,
    Failed,
    UnknownDelivery,
    WaitingForUser,
    AwaitingDelivery,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    #[default]
    Pending,
    Sending,
    Sent,
    Failed,
    Unknown,
    Cancelled,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Bubble {
    pub origin: BubbleOrigin,
    pub revision: u64,
    pub text: String,
    pub passive_message_id: String,
    /// Host-resolved members of this conversation, rendered only at delivery.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentioned_user_ids: Vec<String>,
    pub msg_seq: u32,
    /// True only for a host-authorized same-conversation proactive send.
    #[serde(default)]
    pub proactive: bool,
    pub status: DeliveryStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_issue: Option<DeliveryIssue>,
    pub platform_message_id: Option<String>,
    pub reference_idx: Option<String>,
    /// The incoming message this bubble replies to, separate from its own ACK.
    #[serde(default)]
    pub reply_to_seq: Option<u64>,
    #[serde(default)]
    pub reply_reference_idx: Option<String>,
    /// Rich body paired with a plain native-reference prelude. Indices remain
    /// stable for history lookup; the delivery queue follows this link first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_continuation: Option<usize>,
    pub sent_at_ms: Option<i64>,
    pub time_source: MessageTimeSource,
    pub markdown: bool,
    pub help_buttons: bool,
    /// Host-built command choices, persisted with the delivery for recovery.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command_buttons: Vec<CommandButton>,
    pub image: Option<OutputImage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<OutputFile>,
}

/// Safe delivery diagnostics: fixed host reason and numeric platform codes only.
#[derive(Clone, Serialize, Deserialize)]
pub struct DeliveryIssue {
    pub reason: DeliveryIssueReason,
    pub at_ms: i64,
    pub http_status: Option<u16>,
    pub qq_code: Option<i64>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryIssueReason {
    PassiveWindowPushUnavailable,
    PassiveQuotaPushUnavailable,
    MediaNeedsFreshMessage,
    MediaPreparationFailed,
    AuthenticationUnavailable,
    InvalidRequest,
    QqRejected,
    Unknown,
}

/// Immutable host snapshot, never a model-selected host path or upload token.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutputFile {
    pub file: String,
    pub name: String,
    pub bytes: u64,
}

/// A command prefill, never a callback or an automatically submitted action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandButton {
    pub label: String,
    pub command: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BubbleOrigin {
    #[default]
    Unknown,
    ModelAnswer,
    ControlReply,
    FailureNotice,
    ScheduledTask,
    TaskAck,
    TaskProgress,
    TaskHeartbeat,
    Clarification,
}

impl BubbleOrigin {
    pub fn is_progress(self) -> bool {
        matches!(
            self,
            Self::TaskAck | Self::TaskProgress | Self::TaskHeartbeat
        )
    }
}

/// An application-created immutable image snapshot, scoped by the owning chat.
/// No model-supplied host path or QQ upload capability is persisted here.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputImage {
    pub file: String,
    pub mime: String,
    pub bytes: u64,
    #[serde(default)]
    pub sticker: Option<StickerOrigin>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StickerOrigin {
    pub id: String,
    pub revision: u64,
    pub name: String,
    pub requested: bool,
    pub attempted_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closing_text: Option<String>,
}

/// Host-observed timing, separate from model completion and QQ delivery.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskFeedback {
    pub revision: u64,
    pub accepted_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub last_activity_at_ms: Option<i64>,
    pub last_progress_text: String,
    pub last_progress_at_ms: Option<i64>,
    pub last_update_attempt_ms: Option<i64>,
    pub stale_notice_after_ms: Option<i64>,
}

/// Candidate fingerprints, verified against recoverable native user input
/// before deduplication. An RPC acceptance alone is not a history receipt.
/// No duplicate message bodies are retained; history remains canonical.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexContextCheckpoint {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_input_fingerprint: Option<String>,
    /// Process-local validation only; never trust a persisted claim of delivery.
    #[serde(skip)]
    pub verified: bool,
    pub messages: BTreeMap<String, String>,
    pub deliveries: BTreeMap<String, String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestRecord {
    pub id: u64,
    /// A read-only batch explicitly handed this revision to individual execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_individual_revision: Option<u64>,
    /// Native tool/history continuity belongs to this task, not every unrelated
    /// conversation turn. Missing legacy bindings start a fresh bounded context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_context: Option<CodexContextCheckpoint>,
    /// Immutable user-facing timestamp code; internal IDs remain wire compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_id: Option<String>,
    pub owner_id: String,
    pub contributor_ids: Vec<String>,
    pub audience_id: Option<String>,
    pub source_seqs: Vec<u64>,
    pub revision: u64,
    pub submitted_revision: u64,
    /// Authoritative turn/start acknowledgement, separate from pipeline start.
    pub codex_started_revision: u64,
    /// Advanced only after a successful Codex turn, not merely thread creation.
    pub accepted_revision: u64,
    pub generated_revision: u64,
    pub delivered_revision: u64,
    pub cancelled_revision: Option<u64>,
    /// Fixed application failure code; a sent program notice is not a Codex answer.
    #[serde(default)]
    pub failure_code: Option<String>,
    #[serde(default)]
    pub quote_source_seq: Option<u64>,
    #[serde(default)]
    pub suppress_auto_quote: bool,
    pub status: RequestStatus,
    pub updated_at_ms: i64,
    pub style: String,
    pub bubbles: Vec<Bubble>,
    pub control: bool,
    pub model_alias: Option<String>,
    pub model_stamped: bool,
    #[serde(default)]
    pub long_task: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<TaskFeedback>,
    #[serde(default)]
    pub awaiting_input: bool,
    #[serde(default)]
    pub scheduled_job_id: Option<u64>,
    #[serde(default)]
    pub scheduled_run_id: Option<u64>,
    #[serde(default)]
    pub scheduled_status: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NextModel {
    pub alias: String,
    pub expires_at_ms: i64,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GroupProfileFields {
    pub purpose: String,
    pub role: String,
    pub style: String,
    pub boundaries: String,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupProfile {
    pub fields: GroupProfileFields,
    pub revision: u64,
    /// Highest observed source at the last edit; reset retains this watermark.
    pub source_seq: u64,
    pub source_message_id: String,
    pub updated_by: String,
    pub updated_at_ms: i64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupState {
    /// Explicit local social settings, independent from memories and shared persona files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_profile: Option<GroupProfile>,
    /// This conversation's bot name override; never changes shared persona files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<ConversationIdentity>,
    pub schema_version: u32,
    pub next_seq: u64,
    pub next_request: u64,
    /// Prevent reuse of an unsuffixed time code after pruning or clock rollback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_time_high_watermark: Option<i64>,
    pub decided_through_seq: u64,
    pub thread_id: Option<String>,
    pub messages: Vec<MessageRecord>,
    pub requests: BTreeMap<u64, RequestRecord>,
    pub mode_override: Option<ParticipationMode>,
    pub model_alias: Option<String>,
    pub next_models: BTreeMap<String, NextModel>,
    pub context_start_seq: u64,
    pub enabled_skills: Vec<String>,
    pub tools_override: Option<Vec<String>>,
    pub toolset_key: String,
    #[serde(default)]
    pub browser_enabled: bool,
    #[serde(default)]
    pub plugin_grants: Vec<String>,
}

impl Default for GroupState {
    fn default() -> Self {
        Self {
            group_profile: None,
            identity: None,
            schema_version: 1,
            next_seq: 1,
            next_request: 1,
            request_time_high_watermark: None,
            decided_through_seq: 0,
            thread_id: None,
            messages: Vec::new(),
            requests: BTreeMap::new(),
            mode_override: None,
            model_alias: None,
            next_models: BTreeMap::new(),
            context_start_seq: 0,
            enabled_skills: Vec::new(),
            tools_override: None,
            toolset_key: String::new(),
            browser_enabled: false,
            plugin_grants: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ConversationIdentity {
    pub display_name: Option<String>,
    pub revision: u64,
    pub source_seq: u64,
    pub source_message_id: String,
    pub updated_by: String,
    pub updated_at_ms: i64,
}
