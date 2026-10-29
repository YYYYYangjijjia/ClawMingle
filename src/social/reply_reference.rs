//! Resolve an incoming message to quote before either delivery route.
//!
//! Only conversation-local sequence numbers enter this policy. QQ's opaque
//! reference comes from the retained incoming record, never a passive message
//! ID, a model-supplied string, or this bubble's own delivery acknowledgement.

use anyhow::{Result, ensure};

use crate::model::social::{DeliveryStatus, GroupState, RequestStatus};

const MAX_EXCERPT_CHARS: usize = 48;

/// Tencent's SDK does not combine Markdown with message_reference. Prepare
/// separate durable messages before sending, never retry a sent rich message
/// as plain text. No ACKed/unknown bubble or existing index is rewritten.
pub(super) fn prepare_rendering(state: &mut GroupState, id: u64, index: usize) {
    let request = &state.requests[&id];
    let bubble = &request.bubbles[index];
    if bubble.status != DeliveryStatus::Pending
        || bubble.image.is_some()
        || bubble.file.is_some()
        || !bubble.markdown
        || bubble.reply_reference_idx.is_none()
    {
        return;
    }
    if !needs_markdown(&bubble.text) && !bubble.help_buttons && bubble.command_buttons.is_empty() {
        state
            .requests
            .get_mut(&id)
            .expect("request retained")
            .bubbles[index]
            .markdown = false;
        return;
    }
    // Append instead of inserting: persisted history uses bubble indices as
    // stable identities. next_bubble sends the matching body immediately after
    // its reference prelude via the explicit continuation link.
    let mut body = bubble.clone();
    body.reply_reference_idx = None;
    body.proactive = false;
    body.msg_seq = state
        .requests
        .values()
        .flat_map(|r| &r.bubbles)
        .filter(|b| !b.proactive && b.passive_message_id == body.passive_message_id)
        .map(|b| b.msg_seq)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .filter(|seq| *seq <= 5)
        .unwrap_or(0);
    let (prelude, remainder) = split_prelude(&bubble.text, bubble.origin.is_progress());
    body.text = remainder;
    let request = state.requests.get_mut(&id).expect("request retained");
    let body_index = request.bubbles.len();
    let prelude_bubble = &mut request.bubbles[index];
    prelude_bubble.text = prelude;
    prelude_bubble.markdown = false;
    prelude_bubble.help_buttons = false;
    prelude_bubble.command_buttons.clear();
    // The rich body owns the notification. Quoting must not @ the member twice.
    prelude_bubble.mentioned_user_ids.clear();
    prelude_bubble.render_continuation = Some(body_index);
    request.bubbles.push(body);
}

fn needs_markdown(text: &str) -> bool {
    text.contains('*')
        || text.contains('_')
        || text.contains("~~")
        || text.contains('`')
        || text.contains("](")
        || text.contains("][")
        || text.contains("<http")
        || text.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with(['#', '>', '|'])
                || line.starts_with("- ")
                || line.starts_with("+ ")
                || line.split_once(". ").is_some_and(|(prefix, _)| {
                    !prefix.is_empty() && prefix.chars().all(|ch| ch.is_ascii_digit())
                })
        })
}

fn split_prelude(text: &str, progress: bool) -> (String, String) {
    if let Some((lead, rest)) = text.split_once('\n')
        && !lead.trim().is_empty()
        && !rest.trim().is_empty()
        && lead.chars().count() <= 120
        && !needs_markdown(lead)
    {
        return (lead.trim().into(), rest.trim().into());
    }
    (
        if progress {
            "这条消息的进展："
        } else {
            "这条消息的回复："
        }
        .into(),
        text.into(),
    )
}

/// Decorate one current pending bubble. The caller freezes and persists it with
/// `Sending` after this succeeds; cancellation and passive-window checks remain
/// the delivery owner's responsibility. Calling again on a prepared bubble is
/// idempotent, including an excerpt fallback after a known-not-sent attempt.
pub fn decorate(state: &mut GroupState, id: u64, index: usize, max_chars: usize) -> Result<()> {
    let request = state
        .requests
        .get(&id)
        .ok_or_else(|| anyhow::anyhow!("reply_reference_request_missing"))?;
    let bubble = request
        .bubbles
        .get(index)
        .ok_or_else(|| anyhow::anyhow!("reply_reference_bubble_missing"))?;
    if !(request.status == RequestStatus::Ready
        || matches!(
            request.status,
            RequestStatus::Generating | RequestStatus::Queued
        ) && bubble.origin.is_progress())
        || request.control
        || request.failure_code.is_some()
        || request.cancelled_revision.is_some()
        || bubble.revision != request.revision
        || bubble.status != DeliveryStatus::Pending
    {
        return Ok(());
    }
    if bubble.reply_to_seq.is_some()
        || request.bubbles.iter().any(|other| {
            other.revision == request.revision
                && other.origin.is_progress() == bubble.origin.is_progress()
                && other.reply_to_seq.is_some()
                && matches!(
                    other.status,
                    DeliveryStatus::Sent | DeliveryStatus::Sending | DeliveryStatus::Unknown
                )
        })
    {
        return Ok(());
    }

    let source_seq = request.quote_source_seq.or_else(|| {
        if request.suppress_auto_quote {
            return None;
        }
        request
            .source_seqs
            .last()
            .copied()
            .filter(|source| state.messages.iter().any(|record| record.seq > *source))
    });
    let Some(source_seq) = source_seq else {
        return Ok(());
    };
    apply_reference(state, id, index, source_seq, max_chars)
}

/// Target is already scoped/validated by the caller. Never accept raw platform IDs.
pub(super) fn apply_reference(
    state: &mut GroupState,
    id: u64,
    index: usize,
    source_seq: u64,
    max_chars: usize,
) -> Result<()> {
    let bubble = &state.requests[&id].bubbles[index];
    let source = state
        .messages
        .iter()
        .find(|record| record.seq == source_seq)
        .ok_or_else(|| anyhow::anyhow!("reply_reference_source_missing"))?;
    ensure!(source_seq != 0, "reply_reference_source_missing");
    let reference = source
        .message
        .reference_idx
        .as_ref()
        .filter(|value| !value.trim().is_empty() && !value.chars().any(char::is_control));
    let fallback = if reference.is_none() {
        // An image payload does not carry this text. Leave it unmarked so a
        // later text bubble may explain the source without inventing a message.
        if bubble.image.is_some() || bubble.text.trim().is_empty() {
            return Ok(());
        }
        let Some(text) = excerpt_reply(
            &source.message.text,
            &bubble.text,
            bubble.markdown,
            max_chars,
        ) else {
            // Quoting is auxiliary: a full reply must not fail or lose content
            // merely because there is no remaining room for an excerpt.
            return Ok(());
        };
        Some(text)
    } else {
        None
    };
    let reference = reference.cloned();
    let bubble = &mut state
        .requests
        .get_mut(&id)
        .expect("request retained")
        .bubbles[index];
    bubble.reply_to_seq = Some(source_seq);
    bubble.reply_reference_idx = reference;
    if let Some(text) = fallback {
        bubble.text = text;
    }
    Ok(())
}

/// Proactive delivery supports the same native quote, independently of msg_id.
/// Already prepared bubbles (including old textual fallbacks) remain frozen.
pub(super) fn decorate_proactive(state: &mut GroupState, id: u64, index: usize) -> Result<()> {
    let request = state
        .requests
        .get(&id)
        .ok_or_else(|| anyhow::anyhow!("reply_reference_request_missing"))?;
    if request
        .bubbles
        .get(index)
        .is_some_and(|b| b.reply_to_seq.is_some())
    {
        return Ok(());
    }
    if (request.suppress_auto_quote && request.quote_source_seq.is_none())
        || request.bubbles.iter().any(|b| {
            !b.origin.is_progress()
                && !request.bubbles[index].origin.is_progress()
                && b.revision == request.revision
                && b.reply_to_seq.is_some()
                && b.status == DeliveryStatus::Sent
        })
    {
        return Ok(());
    }
    let source = request
        .quote_source_seq
        .or_else(|| request.source_seqs.last().copied());
    let Some(source) = source else {
        return Ok(());
    };
    apply_reference(state, id, index, source, 4000)
}

/// Render a visibly textual excerpt, preserving the complete answer. Normalize
/// chat whitespace/delimiters and escape Markdown so the excerpt stays data.
fn excerpt_reply(source: &str, body: &str, markdown: bool, max_chars: usize) -> Option<String> {
    let room = max_chars.checked_sub(body.chars().count())?;
    let prefix = "回复「";
    let suffix = "」：";
    let room = room.checked_sub(prefix.chars().count() + suffix.chars().count())?;
    if room == 0 {
        return None;
    }
    let display = crate::util::text::display_mentions(source);
    let clean: String = display
        .chars()
        .filter(|ch| !ch.is_control() || ch.is_whitespace())
        .map(|ch| match ch {
            '<' => '〈',
            '>' => '〉',
            '「' => '“',
            '」' => '”',
            _ => ch,
        })
        .collect();
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let clean = if clean.is_empty() {
        "原消息"
    } else {
        &clean
    };
    let limit = room.min(MAX_EXCERPT_CHARS);
    let mut excerpt = String::new();
    let mut used = 0;
    let mut chars = clean.chars().peekable();
    while let Some(ch) = chars.next() {
        let escaped = markdown && "\\`*_{}[]()#+-.!|~".contains(ch);
        let cost = 1 + usize::from(escaped);
        let more = chars.peek().is_some();
        if used + cost + usize::from(more) > limit {
            if excerpt.is_empty() {
                return None;
            }
            if used < limit {
                excerpt.push('…');
            }
            break;
        }
        if escaped {
            excerpt.push('\\');
        }
        excerpt.push(ch);
        used += cost;
    }
    if excerpt.is_empty() {
        return None;
    }
    Some(format!("{prefix}{excerpt}{suffix}{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formatting_is_not_lost_when_selecting_plain_chat() {
        for text in [
            "*说明*",
            "_说明_",
            "1. 第一项\n2. 第二项",
            "<https://example.invalid/>",
            "[来源](https://example.invalid/)",
            "`code`",
        ] {
            assert!(needs_markdown(text), "{text}");
        }
        assert!(!needs_markdown("吃饭优先，消息又不会凉～"));
    }
    #[test]
    fn casual_native_quote_stays_one_plain_message() {
        let mut state = state();
        state.requests.get_mut(&ID).unwrap().bubbles[0].markdown = true;
        decorate_proactive(&mut state, ID, 0).unwrap();
        prepare_rendering(&mut state, ID, 0);
        assert_eq!(state.requests[&ID].bubbles.len(), 1);
        assert!(!bubble(&state).markdown);
        assert_eq!(bubble(&state).text, "完整回答");
        assert_eq!(
            bubble(&state).reply_reference_idx.as_deref(),
            Some("opaque-source-index")
        );
    }

    #[test]
    fn known_sent_or_uncertain_messages_are_never_split_and_plain_buttons_are_preserved() {
        for status in [
            DeliveryStatus::Sent,
            DeliveryStatus::Unknown,
            DeliveryStatus::Sending,
        ] {
            let mut state = state();
            decorate_proactive(&mut state, ID, 0).unwrap();
            let b = &mut state.requests.get_mut(&ID).unwrap().bubbles[0];
            b.markdown = true;
            b.status = status;
            let before = serde_json::to_vec(&state).unwrap();
            prepare_rendering(&mut state, ID, 0);
            assert_eq!(serde_json::to_vec(&state).unwrap(), before);
        }
        let mut state = state();
        decorate_proactive(&mut state, ID, 0).unwrap();
        let b = &mut state.requests.get_mut(&ID).unwrap().bubbles[0];
        b.markdown = true;
        b.help_buttons = true;
        prepare_rendering(&mut state, ID, 0);
        assert_eq!(state.requests[&ID].bubbles.len(), 2);
        assert!(state.requests[&ID].bubbles[1].help_buttons);
        assert_eq!(state.requests[&ID].bubbles[1].msg_seq, 2);
    }

    #[test]
    fn rich_quote_is_durable_ordered_and_never_replays_sent_bubbles() {
        let mut state = state();
        let r = state.requests.get_mut(&ID).unwrap();
        r.bubbles[0].markdown = true;
        r.bubbles[0].text = "已核实这一点。\n\n**依据** [来源](https://example.invalid/)".into();
        r.bubbles[0].command_buttons = vec![crate::model::social::CommandButton {
            label: "查看进度".into(),
            command: "/status 7".into(),
        }];
        let mut next = r.bubbles[0].clone();
        next.text = "后续段落".into();
        next.msg_seq = 5;
        r.bubbles.push(next);
        decorate_proactive(&mut state, ID, 0).unwrap();
        prepare_rendering(&mut state, ID, 0);
        prepare_rendering(&mut state, ID, 0);
        let r = &state.requests[&ID];
        assert_eq!(r.bubbles.len(), 3);
        assert_eq!(r.bubbles[0].text, "已核实这一点。");
        assert_eq!(r.bubbles[1].text, "后续段落");
        assert_eq!(r.bubbles[0].render_continuation, Some(2));
        assert!(!r.bubbles[0].markdown);
        assert!(r.bubbles[0].command_buttons.is_empty());
        assert!(r.bubbles[2].markdown);
        assert!(r.bubbles[2].reply_reference_idx.is_none());
        assert_eq!(r.bubbles[2].reply_to_seq, Some(1));
        assert_eq!(r.bubbles[2].command_buttons.len(), 1);
        assert_eq!(r.bubbles[2].msg_seq, 0); // reserved for a distinct proactive send
        let mut recovered: GroupState =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        recovered.requests.get_mut(&ID).unwrap().bubbles[0].status = DeliveryStatus::Sent;
        assert_eq!(
            super::super::runtime::next_bubble(&mut recovered),
            Some((ID, 2))
        );
        decorate_proactive(&mut recovered, ID, 2).unwrap();
        prepare_rendering(&mut recovered, ID, 2);
        assert_eq!(recovered.requests[&ID].bubbles.len(), 3);
        assert!(
            recovered.requests[&ID].bubbles[2]
                .reply_reference_idx
                .is_none()
        );
        recovered.requests.get_mut(&ID).unwrap().bubbles[2].status = DeliveryStatus::Sent;
        assert_eq!(
            super::super::runtime::next_bubble(&mut recovered),
            Some((ID, 1))
        );
    }
    #[test]
    fn readable_fallback_never_shows_opaque_mentions() {
        let original = "<@opaque_bot> 查一下 @Codex";
        let text = excerpt_reply(original, "结果", false, 200).unwrap();
        assert!(!text.contains("opaque_bot"));
        assert!(text.contains("@Codex"));
        assert!(original.contains("opaque_bot"));
    }

    #[test]
    fn proactive_progress_retains_native_quote_markdown_buttons_and_source_identity() {
        let mut state = state();
        let request = state.requests.get_mut(&ID).unwrap();
        request.bubbles[0].markdown = true;
        request.bubbles[0].origin = crate::model::social::BubbleOrigin::TaskProgress;
        request.bubbles[0].text = "**结果** [来源](https://example.invalid/)".into();
        let original = request.bubbles[0].text.clone();
        decorate_proactive(&mut state, ID, 0).unwrap();
        assert_eq!(
            bubble(&state).reply_reference_idx.as_deref(),
            Some("opaque-source-index")
        );
        assert_eq!(bubble(&state).text, original);
        assert_eq!(bubble(&state).passive_message_id, "passive-message-1");
        let mut next = bubble(&state).clone();
        next.reply_to_seq = None;
        next.reply_reference_idx = None;
        state.requests.get_mut(&ID).unwrap().bubbles[0].status = DeliveryStatus::Sent;
        state.requests.get_mut(&ID).unwrap().bubbles.push(next);
        decorate_proactive(&mut state, ID, 1).unwrap();
        assert_eq!(
            state.requests[&ID].bubbles[1]
                .reply_reference_idx
                .as_deref(),
            Some("opaque-source-index")
        );
        decorate_proactive(&mut state, ID, 1).unwrap();
        assert_eq!(state.requests[&ID].bubbles[1].text, original);
    }

    #[test]
    fn proactive_missing_index_uses_one_escaped_excerpt_without_losing_body() {
        let mut state = state();
        state.messages[0].message.reference_idx = None;
        state.messages[0].message.text = "**问题**".into();
        state.requests.get_mut(&ID).unwrap().bubbles[0].markdown = true;
        let body = bubble(&state).text.clone();
        decorate_proactive(&mut state, ID, 0).unwrap();
        assert!(bubble(&state).reply_reference_idx.is_none());
        assert!(
            bubble(&state)
                .text
                .starts_with("回复「\\*\\*问题\\*\\*」：")
        );
        assert!(bubble(&state).text.ends_with(&body));
        let once = bubble(&state).text.clone();
        decorate_proactive(&mut state, ID, 0).unwrap();
        assert_eq!(bubble(&state).text, once);
    }

    use crate::model::social::{
        Bubble, ChatMessage, MessageRecord, OutputImage, Quote, RequestRecord,
    };

    const ID: u64 = 7;

    fn record(seq: u64, reference: Option<&str>) -> MessageRecord {
        MessageRecord {
            seq,
            message: ChatMessage {
                id: format!("passive-message-{seq}"),
                author_id: "test-member".into(),
                text: format!("问题{seq}"),
                reference_idx: reference.map(str::to_owned),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn state() -> GroupState {
        let mut state = GroupState {
            messages: vec![record(1, Some("opaque-source-index"))],
            ..Default::default()
        };
        state.requests.insert(
            ID,
            RequestRecord {
                id: ID,
                source_seqs: vec![1],
                revision: 2,
                status: RequestStatus::Ready,
                bubbles: vec![Bubble {
                    revision: 2,
                    text: "完整回答".into(),
                    passive_message_id: "passive-message-1".into(),
                    msg_seq: 1,
                    reference_idx: Some("own-ack-untouched".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        state
    }

    fn bubble(state: &GroupState) -> &Bubble {
        &state.requests[&ID].bubbles[0]
    }

    fn interleave(state: &mut GroupState) {
        state.messages.push(record(2, Some("intervening-index")));
    }

    #[test]
    fn uninterrupted_reply_has_no_reference() {
        let mut state = state();
        decorate(&mut state, ID, 0, 1000).unwrap();
        assert!(bubble(&state).reply_to_seq.is_none());
        assert!(bubble(&state).reply_reference_idx.is_none());
        assert_eq!(bubble(&state).text, "完整回答");
    }

    #[test]
    fn interleaved_reply_quotes_the_latest_request_source_not_the_new_message() {
        let mut state = state();
        interleave(&mut state);
        state
            .messages
            .push(record(3, Some("latest-source-index==")));
        state
            .messages
            .push(record(4, Some("unrelated-latest-index")));
        state.requests.get_mut(&ID).unwrap().source_seqs.push(3);
        decorate(&mut state, ID, 0, 1000).unwrap();
        assert_eq!(bubble(&state).reply_to_seq, Some(3));
        assert_eq!(
            bubble(&state).reply_reference_idx.as_deref(),
            Some("latest-source-index==")
        );
        assert_eq!(
            bubble(&state).reference_idx.as_deref(),
            Some("own-ack-untouched")
        );
        assert_eq!(bubble(&state).passive_message_id, "passive-message-1");
        assert_eq!(bubble(&state).msg_seq, 1);
        assert_eq!(bubble(&state).text, "完整回答");
    }

    #[test]
    fn explicit_selection_quotes_without_interleaving_and_overrides_auto_suppression() {
        let mut state = state();
        let request = state.requests.get_mut(&ID).unwrap();
        request.quote_source_seq = Some(1);
        request.suppress_auto_quote = true;
        decorate(&mut state, ID, 0, 1000).unwrap();
        assert_eq!(bubble(&state).reply_to_seq, Some(1));
        assert_eq!(
            bubble(&state).reply_reference_idx.as_deref(),
            Some("opaque-source-index")
        );
    }

    #[test]
    fn suppress_auto_quote_preserves_an_interleaved_reply() {
        let mut state = state();
        interleave(&mut state);
        state.requests.get_mut(&ID).unwrap().suppress_auto_quote = true;
        decorate(&mut state, ID, 0, 1000).unwrap();
        assert!(bubble(&state).reply_to_seq.is_none());
        assert_eq!(bubble(&state).text, "完整回答");
    }

    #[test]
    fn only_one_sent_sending_or_unknown_bubble_per_revision_is_quoted() {
        for status in [
            DeliveryStatus::Sent,
            DeliveryStatus::Sending,
            DeliveryStatus::Unknown,
        ] {
            let mut state = state();
            interleave(&mut state);
            let second = bubble(&state).clone();
            state.requests.get_mut(&ID).unwrap().bubbles.push(second);
            decorate(&mut state, ID, 0, 1000).unwrap();
            state.requests.get_mut(&ID).unwrap().bubbles[0].status = status;
            decorate(&mut state, ID, 1, 1000).unwrap();
            let request = &state.requests[&ID];
            assert_eq!(request.bubbles[0].reply_to_seq, Some(1));
            assert!(request.bubbles[1].reply_to_seq.is_none());
            assert!(request.bubbles[1].reply_reference_idx.is_none());
        }
    }

    #[test]
    fn a_previous_revision_does_not_consume_the_current_quote() {
        let mut state = state();
        interleave(&mut state);
        state.requests.get_mut(&ID).unwrap().bubbles.push(Bubble {
            revision: 1,
            status: DeliveryStatus::Sent,
            reply_to_seq: Some(1),
            ..Default::default()
        });
        decorate(&mut state, ID, 0, 1000).unwrap();
        assert_eq!(bubble(&state).reply_to_seq, Some(1));
    }

    #[test]
    fn preparing_the_same_pending_bubble_twice_keeps_its_frozen_reference() {
        let mut state = state();
        interleave(&mut state);
        decorate(&mut state, ID, 0, 1000).unwrap();
        state.messages[0].message.reference_idx = Some("changed-index".into());
        decorate(&mut state, ID, 0, 1000).unwrap();
        assert_eq!(
            bubble(&state).reply_reference_idx.as_deref(),
            Some("opaque-source-index")
        );
        assert_eq!(bubble(&state).text, "完整回答");
    }

    #[test]
    fn references_are_resolved_only_in_the_supplied_group_or_private_state() {
        let mut group = state();
        let mut private = state();
        group.requests.get_mut(&ID).unwrap().quote_source_seq = Some(1);
        private.requests.get_mut(&ID).unwrap().quote_source_seq = Some(1);
        private.messages[0].message.reference_idx = Some("private-index".into());
        decorate(&mut group, ID, 0, 1000).unwrap();
        decorate(&mut private, ID, 0, 1000).unwrap();
        assert_eq!(
            bubble(&group).reply_reference_idx.as_deref(),
            Some("opaque-source-index")
        );
        assert_eq!(
            bubble(&private).reply_reference_idx.as_deref(),
            Some("private-index")
        );
        group.messages.push(record(9, Some("other-state-only")));
        private.requests.get_mut(&ID).unwrap().bubbles[0].reply_to_seq = None;
        private.requests.get_mut(&ID).unwrap().quote_source_seq = Some(9);
        let error = decorate(&mut private, ID, 0, 1000).unwrap_err();
        assert_eq!(error.to_string(), "reply_reference_source_missing");
        assert!(!error.to_string().contains("other-state-only"));
    }

    #[test]
    fn missing_native_index_uses_a_bounded_text_excerpt_without_changing_ids() {
        let mut state = state();
        interleave(&mut state);
        let source = &mut state.messages[0].message;
        source.reference_idx = None;
        source.text = "很长的原始问题不能全部放进短摘录".into();
        source.quotes.push(Quote {
            reference_idx: Some("nested-quote-is-not-source".into()),
            ..Default::default()
        });
        decorate(&mut state, ID, 0, 12).unwrap();
        let reply = bubble(&state);
        assert_eq!(reply.text, "回复「很长…」：完整回答");
        assert!(reply.text.chars().count() <= 12);
        assert_eq!(reply.reply_to_seq, Some(1));
        assert!(reply.reply_reference_idx.is_none());
        assert_eq!(reply.passive_message_id, "passive-message-1");
        decorate(&mut state, ID, 0, 12).unwrap();
        assert_eq!(bubble(&state).text, "回复「很长…」：完整回答");
    }

    #[test]
    fn full_text_stays_intact_and_a_later_bubble_can_supply_the_excerpt() {
        let mut state = state();
        interleave(&mut state);
        state.messages[0].message.reference_idx = None;
        state.requests.get_mut(&ID).unwrap().bubbles.push(Bubble {
            revision: 2,
            text: "答".into(),
            ..Default::default()
        });
        decorate(&mut state, ID, 0, 4).unwrap();
        assert_eq!(bubble(&state).text, "完整回答");
        assert!(bubble(&state).reply_to_seq.is_none());
        state.requests.get_mut(&ID).unwrap().bubbles[0].status = DeliveryStatus::Sent;
        decorate(&mut state, ID, 1, 12).unwrap();
        assert_eq!(state.requests[&ID].bubbles[1].text, "回复「问题1」：答");
        assert_eq!(state.requests[&ID].bubbles[1].reply_to_seq, Some(1));
    }

    #[test]
    fn an_image_without_a_native_index_never_invents_a_quote_or_extra_bubble() {
        let mut state = state();
        interleave(&mut state);
        state.messages[0].message.reference_idx = None;
        let reply = &mut state.requests.get_mut(&ID).unwrap().bubbles[0];
        reply.text.clear();
        reply.image = Some(OutputImage::default());
        decorate(&mut state, ID, 0, 1000).unwrap();
        assert!(bubble(&state).text.is_empty());
        assert!(bubble(&state).reply_to_seq.is_none());
        assert!(bubble(&state).reply_reference_idx.is_none());
        assert_eq!(state.requests[&ID].bubbles.len(), 1);
        state.messages[0].message.reference_idx = Some("image-source-index".into());
        decorate(&mut state, ID, 0, 1000).unwrap();
        assert_eq!(
            bubble(&state).reply_reference_idx.as_deref(),
            Some("image-source-index")
        );
    }

    #[test]
    fn fallback_treats_excerpt_markup_controls_and_blank_indices_as_data() {
        let mut state = state();
        interleave(&mut state);
        state.messages[0].message.reference_idx = Some(" \n".into());
        state.messages[0].message.text = "  **标题**\n<@member>\0「内文」 ".into();
        state.requests.get_mut(&ID).unwrap().bubbles[0].markdown = true;
        decorate(&mut state, ID, 0, 1000).unwrap();
        assert_eq!(
            bubble(&state).text,
            "回复「\\*\\*标题\\*\\* @成员“内文”」：完整回答"
        );
        assert!(bubble(&state).reply_reference_idx.is_none());
    }

    #[test]
    fn excerpt_links_stay_text_and_unicode_and_escape_costs_respect_every_budget() {
        assert_eq!(
            excerpt_reply("[x](https://example.invalid)", "答", true, 1000).unwrap(),
            "回复「\\[x\\]\\(https://example\\.invalid\\)」：答"
        );
        let source = format!("甲乙\\*[]{}", "长".repeat(80));
        for markdown in [false, true] {
            for budget in 0..100 {
                if let Some(text) = excerpt_reply(&source, "完整正文😀", markdown, budget) {
                    assert!(text.chars().count() <= budget);
                    assert!(text.ends_with("完整正文😀"));
                    assert!(!text.starts_with("回复「…」"));
                }
            }
        }
        let text = excerpt_reply(&"长".repeat(80), "答", false, 1000).unwrap();
        assert_eq!(text, format!("回复「{}…」：答", "长".repeat(47)));
    }

    #[test]
    fn generating_task_can_quote_only_nonfinal_progress() {
        use crate::model::social::BubbleOrigin;
        for origin in [BubbleOrigin::TaskAck, BubbleOrigin::TaskProgress] {
            let mut state = state();
            interleave(&mut state);
            let request = state.requests.get_mut(&ID).unwrap();
            request.status = RequestStatus::Generating;
            request.bubbles[0].origin = origin;
            decorate(&mut state, ID, 0, 1000).unwrap();
            assert_eq!(bubble(&state).reply_to_seq, Some(1));
            assert_eq!(
                bubble(&state).reply_reference_idx.as_deref(),
                Some("opaque-source-index")
            );
            assert_eq!(state.requests[&ID].status, RequestStatus::Generating);
            assert_eq!(state.requests[&ID].delivered_revision, 0);
        }
    }

    #[test]
    fn cancelled_stale_control_failed_or_nonpending_bubbles_are_unchanged() {
        for case in 0..8 {
            let mut state = state();
            interleave(&mut state);
            let request = state.requests.get_mut(&ID).unwrap();
            match case {
                0 => request.status = RequestStatus::Cancelled,
                1 => request.cancelled_revision = Some(2),
                2 => request.bubbles[0].revision = 1,
                3 => request.control = true,
                4 => request.failure_code = Some("generation_failed".into()),
                5 => request.bubbles[0].status = DeliveryStatus::Sending,
                6 => request.status = RequestStatus::Generating,
                7 => {
                    request.status = RequestStatus::Generating;
                    request.bubbles[0].origin = crate::model::social::BubbleOrigin::ModelAnswer;
                }
                _ => unreachable!(),
            }
            let before = serde_json::to_value(&state).unwrap();
            decorate(&mut state, ID, 0, 1000).unwrap();
            assert_eq!(serde_json::to_value(&state).unwrap(), before);
        }
    }
}
