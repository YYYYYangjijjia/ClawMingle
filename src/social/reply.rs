//! Typed reply intentions. The model selects content; the application prepares
//! media and resolves conversation-local references before anything is sent.
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{config::SocialConfig, state::validate_bubbles};
use crate::model::social::GroupState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reply {
    pub bubbles: Vec<String>,
    #[serde(default)]
    pub mentions: Vec<super::mentions::Selection>,
    #[serde(default)]
    pub images: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub image_request: Option<ImageRequest>,
    #[serde(default)]
    pub quote: Option<QuoteRequest>,
    #[serde(default)]
    pub outcome: Outcome,
    #[serde(default)]
    pub sticker: Option<StickerRequest>,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct StickerRequest {
    pub id: String,
    pub revision: u64,
    pub requested: bool,
    /// Text alternative only for a genuine conversational closing, not unfinished work.
    #[serde(default)]
    pub closing_text: Option<String>,
}

#[derive(Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Outcome {
    #[default]
    Answer,
    NeedsInput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ImageRequest {
    pub prompt: String,
    pub reference_images: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QuoteRequest {
    pub mode: QuoteMode,
    pub seq: Option<u64>,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum QuoteMode {
    Auto,
    None,
    Source,
}

#[cfg(test)]
pub(super) fn parse_reply(
    text: &str,
    config: &SocialConfig,
    generated_images: usize,
) -> Result<Reply> {
    parse_reply_with_queued(text, config, generated_images, false)
}

pub(super) fn parse_reply_with_queued(
    text: &str,
    config: &SocialConfig,
    generated_images: usize,
    has_queued_stickers: bool,
) -> Result<Reply> {
    let mut reply: Reply =
        serde_json::from_str(text).map_err(|_| anyhow::anyhow!("codex_reply_json_invalid"))?;
    if has_queued_stickers {
        // The tool queue is authoritative; the legacy field must not echo an
        // already selected item into a second send.
        reply.sticker = None;
    }
    let images = reply.images.len()
        + generated_images.max(usize::from(reply.image_request.is_some()))
        + usize::from(reply.sticker.is_some());
    ensure!(
        images <= super::media::MAX_OUTPUT_IMAGES,
        "output_image_count_exceeded"
    );
    ensure!(reply.files.len() <= 2, "output_file_count_exceeded");
    ensure!(
        !reply.bubbles.is_empty() || images > 0 || !reply.files.is_empty() || has_queued_stickers,
        "empty_reply"
    );
    ensure!(
        reply.bubbles.len() + images + reply.files.len() <= config.max_bubbles,
        "invalid_bubble_count"
    );
    if !reply.bubbles.is_empty() {
        validate_bubbles(&reply.bubbles, config.max_bubbles, config.max_bubble_chars)?;
    }
    super::mentions::validate(&reply)?;
    if let Some(request) = &reply.image_request {
        ensure!(
            !request.prompt.trim().is_empty() && request.prompt.chars().count() <= 4000,
            "image_prompt_invalid"
        );
        ensure!(
            request.reference_images.len() <= 3,
            "image_reference_count_exceeded"
        );
    }
    if let Some(sticker) = &reply.sticker {
        if let Some(text) = &sticker.closing_text {
            ensure!(
                !text.trim().is_empty()
                    && text.chars().count() <= config.max_bubble_chars
                    && !text.contains('\0')
                    && reply.outcome == Outcome::Answer,
                "sticker_closing_invalid"
            );
        }
        ensure!(
            sticker.id.len() == 40
                && sticker
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                && sticker.revision > 0,
            "sticker_reference_invalid"
        );
        ensure!(
            sticker.requested
                || sticker.closing_text.is_some()
                || !reply.bubbles.is_empty()
                || !reply.images.is_empty()
                || !reply.files.is_empty(),
            "sticker_cannot_replace_answer"
        );
    }
    if let Some(quote) = &reply.quote {
        ensure!(
            (quote.mode == QuoteMode::Source) == quote.seq.is_some(),
            "quote_selection_invalid"
        );
    }
    Ok(reply)
}

pub(super) fn quote_selection(
    state: &GroupState,
    id: u64,
    reply: &Reply,
    config: &SocialConfig,
) -> Result<(Option<u64>, bool)> {
    let request = state
        .requests
        .get(&id)
        .ok_or_else(|| anyhow::anyhow!("request_missing"))?;
    let Some(quote) = &reply.quote else {
        return Ok((None, false));
    };
    if let Some(seq) = quote.seq {
        let visible = request.source_seqs.contains(&seq)
            || state
                .messages
                .iter()
                .filter(|r| {
                    r.seq >= state.context_start_seq && r.application.as_deref() != Some("command")
                })
                .rev()
                .take(config.context_messages)
                .any(|r| r.seq == seq);
        ensure!(
            visible && state.record(seq).is_ok(),
            "quote_source_not_visible"
        );
    }
    Ok(quote_intent(reply))
}

/// Visibility is checked against the generation snapshot. New chat messages
/// must not invalidate that choice when the live context window moves forward.
pub(super) fn quote_intent(reply: &Reply) -> (Option<u64>, bool) {
    reply
        .quote
        .as_ref()
        .map(|q| (q.seq, q.mode == QuoteMode::None))
        .unwrap_or((None, false))
}

pub(super) fn bubble_schema(config: &SocialConfig) -> Value {
    json!({"type":"object","properties":{
        "outcome":{"type":"string","enum":["answer","needs_input"],"description":"answer is a completed result; needs_input asks an essential question and keeps the application request open."},
        "bubbles":{"type":"array","items":{"type":"string"},"maxItems":config.max_bubbles},
        "mentions":super::mentions::schema(),
        "files":{"type":"array","maxItems":2,"items":{"type":"string","maxLength":512},"description":"Send ordinary attachments from the current conversation workspace. Each must be an existing /workspace/ or relative regular file, at most 2 MiB. Use workspace_exec to create Markdown, CSV, JSON, PDF or other requested documents. No remote URLs, host paths or other conversations. Text, images and files share the total bubble limit."},
        "images":{"type":"array","items":{"type":"string","description":"An application image_ref (input:1..3, sent:1..2), or a /workspace/ PNG/JPEG/GIF/WebP path. Existing originals only; animation previews must not replace originals."},"maxItems":super::media::MAX_OUTPUT_IMAGES},
        "image_request":{"type":["object","null"],"properties":{"prompt":{"type":"string","maxLength":4000},"reference_images":{"type":"array","items":{"type":"string"},"maxItems":3}},"required":["prompt","reference_images"],"additionalProperties":false,"description":"Request one native Codex generated/edited PNG. For an edit, include the provided image_ref(s). Null when no image generation/edit is requested."},
        "quote":{"type":["object","null"],"properties":{"mode":{"type":"string","enum":["auto","none","source"]},"seq":{"type":["integer","null"]}},"required":["mode","seq"],"additionalProperties":false,"description":"Normally null (automatic delayed-reply reference). For an explicit quote use source and a visible incoming message seq. Use none only when the user asks not to quote."}
        ,"sticker":{"type":["object","null"],"properties":{"id":{"type":"string","maxLength":40},"revision":{"type":"integer","minimum":1},"requested":{"type":"boolean"},"closing_text":{"type":["string","null"],"maxLength":config.max_bubble_chars,"description":"Normally null. Only for a genuine goodbye/thanks/natural closing with no unanswered request: a short natural text alternative if the sticker is suppressed. Allows a sticker-only closing; never use to abandon a task or essential clarification."}},"required":["id","revision","requested","closing_text"],"additionalProperties":false,"description":"At most one existing catalogue sticker, selected with sticker_lookup. requested is true only when the current user asks for a sticker; otherwise obey automatic/cooldown policy. Choose when it adds a fitting emotional response, especially during a playful sticker exchange; use null when unnecessary or no suitable asset exists. Never replace a substantive answer with a sticker."}
    },"required":["bubbles","mentions","images","files","image_request","quote","sticker","outcome"],"additionalProperties":false})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_files_share_total_reply_budget_and_are_optional_for_old_replies() {
        let config = SocialConfig::default();
        assert!(
            parse_reply(
                &json!({"bubbles":[],"files":["/workspace/plan.md"]}).to_string(),
                &config,
                0
            )
            .is_ok()
        );
        assert!(
            parse_reply(&json!({"bubbles":["old format"]}).to_string(), &config, 0)
                .unwrap()
                .files
                .is_empty()
        );
        assert!(
            parse_reply(
                &json!({"bubbles":["one","two","three"],"files":["/workspace/plan.md"]})
                    .to_string(),
                &config,
                0
            )
            .is_err()
        );
    }
    #[test]
    fn optional_sticker_counts_as_an_image_and_never_replaces_an_ordinary_answer() {
        let config = SocialConfig::default();
        let sticker = json!({"id":"a".repeat(40),"revision":1,"requested":false});
        assert!(
            parse_reply(
                &json!({"bubbles":["answer"],"sticker":sticker}).to_string(),
                &config,
                0
            )
            .is_ok()
        );
        assert!(
            parse_reply(
                &json!({"bubbles":[],"sticker":sticker}).to_string(),
                &config,
                0
            )
            .is_err()
        );
        assert!(
            parse_reply(
                &json!({"bubbles":["answer"],"images":["input:1","input:2"],"sticker":sticker})
                    .to_string(),
                &config,
                0
            )
            .is_err()
        );
        assert!(
            parse_reply(
                &json!({"bubbles":["a","b","c"],"sticker":sticker}).to_string(),
                &config,
                0
            )
            .is_err()
        );
        assert!(parse_reply(&json!({"bubbles":[],"sticker":{"id":"a".repeat(40),"revision":1,"requested":true}}).to_string(),&config,0).is_ok());
        assert!(
            parse_reply(
                &json!({"bubbles":["answer"],"sticker":null}).to_string(),
                &config,
                0
            )
            .is_ok()
        );
    }
    #[test]
    fn sticker_only_closing_requires_a_valid_text_alternative_and_cannot_ask_for_input() {
        let config = SocialConfig::default();
        let plan = json!({"bubbles":[],"sticker":{"id":"a".repeat(40),"revision":1,"requested":false,"closing_text":"晚安，明天见。"}});
        let reply = parse_reply(&plan.to_string(), &config, 0).unwrap();
        assert!(reply.bubbles.is_empty());
        assert_eq!(
            reply.sticker.unwrap().closing_text.as_deref(),
            Some("晚安，明天见。")
        );
        for invalid in [
            json!(""),
            json!("x".repeat(config.max_bubble_chars + 1)),
            json!("\u{0000}"),
        ] {
            let mut value = plan.clone();
            value["sticker"]["closing_text"] = invalid;
            assert!(parse_reply(&value.to_string(), &config, 0).is_err());
        }
        let mut value = plan;
        value["outcome"] = json!("needs_input");
        assert!(parse_reply(&value.to_string(), &config, 0).is_err());
    }
    #[test]
    fn generation_is_a_real_media_bubble_and_invalid_plans_never_start_work() {
        let config = SocialConfig::default();
        let text=json!({"bubbles":[],"images":[],"image_request":{"prompt":"把毛绒玩具改成橙色，保留背景","reference_images":["input:1"]}}).to_string();
        assert!(parse_reply(&text, &config, 0).is_ok());
        assert!(parse_reply(&text, &config, 1).is_ok());
        assert!(parse_reply(&text, &config, 3).is_err());
        for invalid in [
            json!({"bubbles":[],"images":[],"image_request":{"prompt":"","reference_images":[]}}),
            json!({"bubbles":["a","b","c"],"images":[],"image_request":{"prompt":"draw","reference_images":[]}}),
            json!({"bubbles":["a"],"quote":{"mode":"auto","seq":1}}),
        ] {
            assert!(parse_reply(&invalid.to_string(), &config, 0).is_err());
        }
    }

    #[test]
    fn a_model_selects_only_visible_message_sequences_never_platform_indices() {
        use crate::model::social::{ChatMessage, MessageRecord, RequestRecord};
        let mut state = GroupState::default();
        state.requests.insert(
            1,
            RequestRecord {
                source_seqs: vec![1],
                ..Default::default()
            },
        );
        for seq in 1..=4 {
            state.messages.push(MessageRecord {
                seq,
                message: ChatMessage {
                    id: format!("message-{seq}"),
                    reference_idx: Some(format!("opaque-{seq}")),
                    ..Default::default()
                },
                application: (seq == 3).then(|| "command".into()),
                ..Default::default()
            });
        }
        let config = SocialConfig {
            context_messages: 1,
            ..Default::default()
        };
        for seq in [1, 4] {
            let reply = parse_reply(
                &json!({"bubbles":["answer"],"quote":{"mode":"source","seq":seq}}).to_string(),
                &config,
                0,
            )
            .unwrap();
            assert_eq!(
                quote_selection(&state, 1, &reply, &config).unwrap(),
                (Some(seq), false)
            );
        }
        for seq in [2, 3, 999] {
            let reply = parse_reply(
                &json!({"bubbles":["answer"],"quote":{"mode":"source","seq":seq}}).to_string(),
                &config,
                0,
            )
            .unwrap();
            assert!(quote_selection(&state, 1, &reply, &config).is_err());
        }
        let no_quote = parse_reply(
            r#"{"bubbles":["answer"],"quote":{"mode":"none","seq":null}}"#,
            &config,
            0,
        )
        .unwrap();
        assert_eq!(
            quote_selection(&state, 1, &no_quote, &config).unwrap(),
            (None, true)
        );
    }
}
