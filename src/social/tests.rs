use std::{collections::BTreeMap, time::Duration};

use serde_json::{Value, json};

use super::{
    SocialConfig, jev, runtime,
    state::{CollectionWindow, rule_decision, validate_bubbles},
    store::{Store, acquire_lock, private_directory},
};
use crate::{
    model::social::*,
    qq::{GroupMessageAck, GroupSendError, GroupSendUnknownReason},
};

fn add(state: &mut GroupState, author: &str, text: &str, time: i64, at_bot: bool) -> u64 {
    state
        .ingest(
            ChatMessage {
                id: format!("m{}", state.next_seq),
                author_id: author.into(),
                text: text.into(),
                timestamp_ms: time,
                at_bot,
                ..Default::default()
            },
            500,
        )
        .unwrap()
        .unwrap()
}
fn decide(state: &mut GroupState, seq: u64, action: Action, request_id: Option<u64>) {
    state.begin_decision(seq).unwrap();
    let decision = Decision {
        action,
        request_id,
        source: "jev".into(),
        style: "answer".into(),
        audience_id: Some(state.record(seq).unwrap().message.author_id.clone()),
        request_revisions: super::state::request_revisions(state),
        ..Default::default()
    };
    state.apply_decision(seq, decision, 16).unwrap();
}
fn one_request() -> GroupState {
    let mut state = GroupState::default();
    let seq = add(&mut state, "human-A", "帮我选晚饭", 1000, true);
    decide(&mut state, seq, Action::NewRequest, None);
    state
}

#[test]
fn fresh_request_omits_only_completed_sources_and_keeps_silent_and_visual_context() {
    let mut state = one_request();
    let old = state.requests.get_mut(&1).unwrap();
    old.status = RequestStatus::Completed;
    old.generated_revision = 1;
    old.delivered_revision = 1;
    let second = add(&mut state, "human-B", "silent text", 2000, false);
    decide(&mut state, second, Action::Silent, None);
    let third = add(&mut state, "human-A", "silent screenshot", 3000, false);
    state
        .messages
        .last_mut()
        .unwrap()
        .message
        .images
        .push(ImageAttachment {
            url: "https://example.com/screenshot.png".into(),
            content_type: "image/png".into(),
        });
    decide(&mut state, third, Action::Silent, None);
    let fourth = add(&mut state, "human-A", "look at that screenshot", 4000, true);
    decide(&mut state, fourth, Action::NewRequest, None);
    let config = SocialConfig::default();
    let selected = |s: &GroupState, c: &SocialConfig| {
        super::message_context::request_window(s, 2, c)
            .iter()
            .map(|r| r.seq)
            .collect::<Vec<_>>()
    };
    assert_eq!(selected(&state, &config), vec![2, 3, 4]);
    let prompt = runtime::codex_prompt(&state, 2, &config).unwrap();
    let input: Value = serde_json::from_str(prompt.rsplit_once("\n\n").unwrap().1).unwrap();
    assert_eq!(
        input["group_context"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["seq"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(input["context_selection"]["completed_sources_omitted"], 1);
    assert_eq!(
        super::image_inputs::identity(&super::image_inputs::collect(&state, 2, &config).unwrap())
            [0]["seq"],
        3
    );
    for status in [
        RequestStatus::Failed,
        RequestStatus::Sending,
        RequestStatus::UnknownDelivery,
        RequestStatus::AwaitingDelivery,
    ] {
        state.requests.get_mut(&1).unwrap().status = status;
        assert_eq!(selected(&state, &config), vec![1, 2, 3, 4]);
    }
    state.requests.get_mut(&1).unwrap().status = RequestStatus::Completed;
    state.requests.get_mut(&1).unwrap().failure_code = Some("model_rate_limited".into());
    assert_eq!(selected(&state, &config), vec![1, 2, 3, 4]);
    state.requests.get_mut(&1).unwrap().failure_code = None;
    state.messages[0].applied_revision = None;
    assert_eq!(selected(&state, &config), vec![1, 2, 3, 4]);
    state.messages[0].applied_revision = Some(1);
    let mut disabled = config.clone();
    disabled
        .enabled_tools
        .retain(|name| name != "message_lookup");
    assert_eq!(selected(&state, &disabled), vec![1, 2, 3, 4]);
    state.messages[0].message.images = state.messages[2].message.images.clone();
    assert_eq!(selected(&state, &config), vec![1, 2, 3, 4]);
    let mut bounded = config.clone();
    bounded.context_windows.codex_messages = Some(2);
    assert_eq!(selected(&state, &bounded), vec![3, 4]);
}

#[test]
fn mentioned_fragments_can_merge_but_invalid_targets_never_mutate_other_work() {
    let mut state = one_request();
    let seq = add(&mut state, "human-A", "also include breakfast", 2000, true);
    assert!(
        super::state::routing_rule(
            &state,
            ParticipationMode::Social,
            &state.record(seq).unwrap().message
        )
        .is_none()
    );
    decide(&mut state, seq, Action::Append, Some(1));
    assert_eq!(state.requests.len(), 1);
    assert_eq!(state.requests[&1].source_seqs, vec![1, 2]);
    let seq = add(
        &mut state,
        "human-B",
        "change their requirements",
        3000,
        true,
    );
    decide(&mut state, seq, Action::Revise, Some(1));
    assert_eq!(state.requests[&1].revision, 2);
    assert_eq!(state.record(seq).unwrap().applied_request, Some(2));
    let seq = add(&mut state, "human-A", "later addition", 4000, true);
    state.begin_decision(seq).unwrap();
    state
        .apply_decision(
            seq,
            Decision {
                action: Action::Append,
                request_id: Some(1),
                source: "jev".into(),
                request_revisions: BTreeMap::from([(1, 1)]),
                ..Default::default()
            },
            16,
        )
        .unwrap();
    assert_eq!(state.requests[&1].revision, 2);
    assert_eq!(state.record(seq).unwrap().applied_request, Some(3));
}
fn unknown() -> GroupSendError {
    GroupSendError::Unknown {
        reason: GroupSendUnknownReason::Timeout,
    }
}
fn ack() -> GroupMessageAck {
    GroupMessageAck {
        id: "reply-1".into(),
        timestamp: None,
        ext_info: None,
    }
}

fn asr_reference(text: &str) -> VoiceInput {
    VoiceInput {
        source: VoiceSource::QqAsr,
        status: VoiceStatus::Available,
        transcript: Some(text.into()),
    }
}

#[test]
fn voice_references_reach_jev_current_history_and_active_request_without_urls() {
    let mut state = one_request();
    state.messages[0].message.voices = vec![asr_reference("之前的语音问题")];
    let seq = add(&mut state, "human-A", "", 2000, false);
    state.messages.last_mut().unwrap().message.voices = vec![asr_reference("补充语音里的要求")];
    let config = SocialConfig {
        mode: ParticipationMode::Helpful,
        ..SocialConfig::default()
    };
    let prepared = jev::prepare(&state, seq, &config, "fixture-jev").unwrap();
    assert_eq!(
        prepared.input.state["current_message"]["content"]["text"],
        ""
    );
    assert_eq!(
        prepared.input.state["current_message"]["voice_inputs"][0]["source"],
        "qq_asr"
    );
    assert_eq!(
        prepared.input.state["current_message"]["voice_inputs"][0]["transcript"]["text"],
        "补充语音里的要求"
    );
    assert_eq!(
        prepared.input.state["recent_messages"][0]["voice_inputs"][0]["transcript"]["text"],
        "之前的语音问题"
    );
    assert_eq!(
        prepared.input.state["active_requests"][0]["messages"][0]["voice_inputs"][0]["transcript"]
            ["text"],
        "之前的语音问题"
    );
    assert!(
        prepared.input.state["policy"]["voice_input_instructions"]
            .as_str()
            .unwrap()
            .contains("Never derive")
    );
    decide(&mut state, seq, Action::NewRequest, None);
    let prompt = runtime::codex_prompt(&state, 2, &SocialConfig::default()).unwrap();
    let data: Value = serde_json::from_str(prompt.rsplit_once("\n\n").unwrap().1).unwrap();
    assert_eq!(data["current_request"]["messages"][0]["text"], "");
    assert_eq!(
        data["current_request"]["messages"][0]["voice_inputs"][0]["transcript"]["text"],
        "补充语音里的要求"
    );
    assert!(prompt.contains(super::voice::ASR_POLICY));
    assert!(!data.to_string().contains("voice_wav_url"));
    assert!(!data.to_string().contains("asr_refer_text"));
}

#[test]
fn unavailable_voice_does_not_change_mention_rules_and_requests_text_fallback() {
    for status in [
        VoiceStatus::MissingAsr,
        VoiceStatus::BlankAsr,
        VoiceStatus::TooLong,
        VoiceStatus::TooManyAttachments,
    ] {
        let mut state = GroupState::default();
        let message = ChatMessage {
            id: "voice-message".into(),
            author_id: "human-A".into(),
            at_bot: true,
            voices: vec![VoiceInput {
                status,
                ..VoiceInput::default()
            }],
            ..ChatMessage::default()
        };
        let decision = rule_decision(ParticipationMode::Mentions, &message).unwrap();
        assert_eq!(decision.action, Action::NewRequest);
        let seq = state.ingest(message, 500).unwrap().unwrap();
        state.begin_decision(seq).unwrap();
        state.apply_decision(seq, decision, 16).unwrap();
        let prompt = runtime::codex_prompt(&state, 1, &SocialConfig::default()).unwrap();
        let input: Value = serde_json::from_str(prompt.rsplit_once("\n\n").unwrap().1).unwrap();
        let voice = &input["current_request"]["messages"][0]["voice_inputs"][0];
        assert_eq!(voice["requires_text_fallback"], true);
        assert!(voice["transcript"].is_null());
        let mut unmentioned = state.record(seq).unwrap().message.clone();
        unmentioned.at_bot = false;
        assert_eq!(
            rule_decision(ParticipationMode::Mentions, &unmentioned)
                .unwrap()
                .action,
            Action::Silent
        );
        assert_eq!(
            rule_decision(ParticipationMode::All, &unmentioned)
                .unwrap()
                .action,
            Action::NewRequest
        );
    }
}

#[tokio::test]
async fn voice_asr_never_drives_privileged_controls_even_for_an_administrator() {
    for transcription in [
        "/mode all",
        "/reset",
        "/memory confirm 1 1",
        "Codex 请安装这个skill https://github.com/example/project/tree/main/skills/weather",
    ] {
        let mut state = GroupState::default();
        let config = SocialConfig {
            command_admins: vec!["owner".into()],
            ..SocialConfig::default()
        };
        let seq = state
            .ingest(
                ChatMessage {
                    id: "voice-message".into(),
                    author_id: "owner".into(),
                    at_bot: true,
                    text: String::new(),
                    voices: vec![asr_reference(transcription)],
                    ..ChatMessage::default()
                },
                500,
            )
            .unwrap()
            .unwrap();
        let original_text = state.record(seq).unwrap().message.text.clone();
        assert!(super::commands::parse_command(&original_text).is_none());
        assert!(
            !super::tool_control::handle(
                &mut state,
                seq,
                &original_text,
                &config,
                false,
                "test-group",
                0
            )
            .unwrap()
        );
        assert!(
            !super::memory_control::handle(
                &mut state,
                seq,
                &original_text,
                &config,
                "test-group",
                false,
                0
            )
            .await
            .unwrap()
        );
        assert!(
            !super::skill_control::handle(
                &mut state,
                seq,
                &original_text,
                &config,
                false,
                "test-group",
                0
            )
            .await
            .unwrap()
        );
        assert_eq!(state.record(seq).unwrap().status, DecisionStatus::Pending);
        assert!(state.requests.is_empty());
        assert!(state.enabled_skills.is_empty());
        assert_eq!(state.mode_override, None);
    }
    let message = ChatMessage {
        text: "/mode 1".into(),
        voices: vec![asr_reference("/mode all")],
        ..ChatMessage::default()
    };
    assert!(
        matches!(super::commands::parse_command(&message.text),Some(super::commands::ChatCommand::Mode(Some(mode))) if mode=="mode-1")
    );
}

#[test]
fn rules_skip_jev_for_real_mentions_and_extreme_modes_but_not_quoted_bot() {
    for mode in [
        ParticipationMode::Mentions,
        ParticipationMode::Helpful,
        ParticipationMode::Social,
        ParticipationMode::All,
    ] {
        let mut message = ChatMessage {
            at_bot: true,
            ..Default::default()
        };
        assert_eq!(
            rule_decision(mode, &message).unwrap().action,
            Action::NewRequest
        );
        message.at_bot = false;
        message.quotes.push(Quote {
            from_bot: true,
            text: "bot text".into(),
            ..Default::default()
        });
        match mode {
            ParticipationMode::Mentions => assert_eq!(
                rule_decision(mode, &message).unwrap().action,
                Action::Silent
            ),
            ParticipationMode::All => assert_eq!(
                rule_decision(mode, &message).unwrap().action,
                Action::NewRequest
            ),
            _ => assert!(rule_decision(mode, &message).is_none()),
        }
    }
}

#[test]
fn dinner_case_keeps_three_sources_and_human_chatter_as_context_only() {
    let mut state = GroupState::default();
    for (author, text, time) in [
        ("A", "你吃饭了么", 0),
        ("B", "没有呢不知道吃啥", 20_000),
        ("A", "我也是，Codex赶快帮帮我", 22_000),
        ("A", "帮我选选吃烤肉还是吃烤鱼还是吃烧烤", 30_000),
        ("A", "算了还是在烤肉和烤鱼里选吧", 32_000),
        ("A", "你今晚上还得写代码么？要不要等下打回游戏？", 34_000),
    ] {
        add(&mut state, author, text, time, false);
    }
    for (seq, action, id) in [
        (1, Action::Silent, None),
        (2, Action::Silent, None),
        (3, Action::NewRequest, None),
        (4, Action::Append, Some(1)),
        (5, Action::Revise, Some(1)),
        (6, Action::Silent, None),
    ] {
        decide(&mut state, seq, action, id);
    }
    assert_eq!(state.requests[&1].source_seqs, vec![3, 4, 5]);
    assert_eq!(state.requests[&1].revision, 3);
    assert_eq!(state.decided_through_seq, 6);
    let prompt = runtime::codex_prompt(&state, 1, &SocialConfig::default()).unwrap();
    let data: Value = serde_json::from_str(prompt.rsplit_once("\n\n").unwrap().1).unwrap();
    assert_eq!(
        data["current_request"]["messages"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert!(
        data["group_context"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["seq"] == 6)
    );
    assert!(
        !data["current_request"]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["seq"] == 6)
    );
}

#[test]
fn collection_window_is_bounded_and_does_not_merge_eight_second_gap() {
    let mut window = CollectionWindow::default();
    window.observe(0);
    assert!(!window.due(1199, 1200, 4000));
    assert!(window.due(1200, 1200, 4000));
    window.clear();
    window.observe(8000);
    assert!(!window.due(8500, 1200, 4000));
    window.observe(9000);
    window.observe(10000);
    window.observe(11000);
    assert!(window.due(12000, 1200, 4000));
}

#[test]
fn update_invalidates_inflight_generation_and_preserves_latest_audience() {
    let mut state = one_request();
    let old = state.begin_generation(1).unwrap();
    let seq = add(&mut state, "human-B", "也帮我选一下", 2000, false);
    state.begin_decision(seq).unwrap();
    state
        .apply_decision(
            seq,
            Decision {
                action: Action::Append,
                request_id: Some(1),
                audience_id: Some("human-B".into()),
                style: "clarify".into(),
                source: "jev".into(),
                ..Default::default()
            },
            16,
        )
        .unwrap();
    assert!(
        !state
            .finish_generation(1, old, vec!["old answer".into()])
            .unwrap()
    );
    assert_eq!(state.requests[&1].audience_id.as_deref(), Some("human-B"));
    assert_eq!(state.requests[&1].style, "clarify");
    assert_eq!(
        state.requests[&1].contributor_ids,
        vec!["human-A", "human-B"]
    );
}

#[test]
fn another_member_can_supplement_but_cannot_revise_or_cancel_owner_request() {
    let mut state = one_request();
    for action in [Action::Revise, Action::Cancel] {
        let seq = add(&mut state, "human-B", "取消他的请求", 2000, false);
        decide(&mut state, seq, action, Some(1));
        assert_eq!(
            state.record(seq).unwrap().application.as_deref(),
            Some("owner_required")
        );
    }
    assert_eq!(state.requests[&1].revision, 1);
    assert_eq!(state.requests[&1].owner_id, "human-A");
}

#[test]
fn failed_decision_blocks_cursor_but_not_later_direct_request() {
    let mut state = GroupState::default();
    let first = add(&mut state, "A", "普通发言", 1, false);
    state.begin_decision(first).unwrap();
    state.fail_decision(first);
    let second = add(&mut state, "A", "明确@请求", 2, true);
    decide(&mut state, second, Action::NewRequest, None);
    assert_eq!(state.decided_through_seq, 0);
    assert_eq!(state.requests.len(), 1);
    assert!(!state.has_pending());
}

#[test]
fn replay_is_deduplicated_and_late_at_reopens_silent_decision() {
    let mut state = GroupState::default();
    let seq = add(&mut state, "A", "Codex", 1, false);
    decide(&mut state, seq, Action::Silent, None);
    let mut replay = state.record(seq).unwrap().message.clone();
    assert!(state.ingest(replay.clone(), 500).unwrap().is_none());
    replay.at_bot = true;
    assert_eq!(state.ingest(replay, 500).unwrap(), Some(seq));
    assert_eq!(state.decided_through_seq, 0);
    state.begin_decision(seq).unwrap();
    let decision = rule_decision(
        ParticipationMode::Mentions,
        &state.record(seq).unwrap().message,
    )
    .unwrap();
    state.apply_decision(seq, decision, 16).unwrap();
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.requests.len(), 1);
    assert_eq!(state.decided_through_seq, 1);
}

#[test]
fn old_revision_unknown_resolves_to_latest_queue_or_cancel_without_stalling() {
    for action in [Action::Append, Action::Cancel] {
        for was_sent in [true, false] {
            let mut state = one_request();
            let revision = state.begin_generation(1).unwrap();
            state
                .finish_generation(1, revision, vec!["first".into(), "second".into()])
                .unwrap();
            state.requests.get_mut(&1).unwrap().bubbles[0].status = DeliveryStatus::Sending;
            state.requests.get_mut(&1).unwrap().status = RequestStatus::Sending;
            let seq = add(&mut state, "human-A", "新要求", 2000, false);
            decide(&mut state, seq, action, Some(1));
            runtime::finish_send(&mut state, 1, 0, Err(unknown()));
            assert_eq!(state.requests[&1].status, RequestStatus::UnknownDelivery);
            state.resolve(1, 0, was_sent).unwrap();
            assert_eq!(
                state.requests[&1].status,
                if action == Action::Cancel {
                    RequestStatus::Cancelled
                } else {
                    RequestStatus::Queued
                }
            );
            assert_eq!(
                state.requests[&1].bubbles[0].status,
                if was_sent {
                    DeliveryStatus::Sent
                } else {
                    DeliveryStatus::Cancelled
                }
            );
            assert_eq!(
                state.requests[&1].bubbles[1].status,
                DeliveryStatus::Cancelled
            );
        }
    }
}

#[test]
fn partial_delivery_and_restart_do_not_resend_sent_prefix_or_unknown_tail() {
    let mut state = one_request();
    let revision = state.begin_generation(1).unwrap();
    state
        .finish_generation(
            1,
            revision,
            vec!["first".into(), "second".into(), "third".into()],
        )
        .unwrap();
    runtime::finish_send(&mut state, 1, 0, Ok(ack()));
    state.requests.get_mut(&1).unwrap().bubbles[1].status = DeliveryStatus::Sending;
    state.requests.get_mut(&1).unwrap().status = RequestStatus::Sending;
    let bytes = serde_json::to_vec(&state).unwrap();
    let mut recovered: GroupState = serde_json::from_slice(&bytes).unwrap();
    recovered.recover();
    assert_eq!(
        recovered.requests[&1].bubbles[0].status,
        DeliveryStatus::Sent
    );
    assert_eq!(
        recovered.requests[&1].bubbles[1].status,
        DeliveryStatus::Unknown
    );
    assert_eq!(
        recovered.requests[&1].status,
        RequestStatus::UnknownDelivery
    );
    assert!(runtime::next_bubble(&mut recovered).is_none());
    recovered.resolve(1, 1, true).unwrap();
    assert_eq!(runtime::next_bubble(&mut recovered), Some((1, 2)));
    assert_eq!(recovered.requests[&1].delivered_revision, 0);
    runtime::finish_send(&mut recovered, 1, 2, Ok(ack()));
    assert_eq!(recovered.requests[&1].delivered_revision, 1);
}

#[test]
fn expired_passive_anchor_is_never_eligible_even_after_restart() {
    let mut state = one_request();
    let revision = state.begin_generation(1).unwrap();
    state
        .finish_generation(1, revision, vec!["reply".into()])
        .unwrap();
    assert!(runtime::passive_anchor_valid(&state, 1, 0, 2000, false));
    assert!(!runtime::passive_anchor_valid(&state, 1, 0, 301000, false));
}

#[test]
fn bubble_limits_reject_empty_long_and_excessive_output() {
    assert!(validate_bubbles(&[], 3, 100).is_err());
    assert!(validate_bubbles(&[" ".into()], 3, 100).is_err());
    assert!(validate_bubbles(&["字".repeat(101)], 3, 100).is_err());
    assert!(validate_bubbles(&vec!["a".into(); 4], 3, 100).is_err());
}

#[test]
fn jev_input_is_causal_uses_human_mentions_and_prioritizes_active_requests() {
    let mut state = one_request();
    for id in 2..=18 {
        state.requests.insert(
            id,
            RequestRecord {
                id,
                revision: 1,
                status: RequestStatus::Completed,
                updated_at_ms: 2000,
                source_seqs: vec![1],
                ..Default::default()
            },
        );
    }
    let seq = add(&mut state, "A", "@另一位朋友 你觉得呢", 3000, false);
    state
        .messages
        .last_mut()
        .unwrap()
        .message
        .mentioned_user_ids
        .push("has-not-spoken".into());
    add(&mut state, "A", "future message", 4000, false);
    let config = SocialConfig {
        mode: ParticipationMode::Helpful,
        ..Default::default()
    };
    let prepared = jev::prepare(&state, seq, &config, "jev-1.14.0").unwrap();
    assert!(
        prepared.input.questions["task_target"]
            .criteria
            .contains_key("request_1")
    );
    assert!(
        prepared
            .audiences
            .values()
            .any(|id| id.as_deref() == Some("has-not-spoken"))
    );
    assert_eq!(
        prepared.input.state["current_message"]["mentions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(!prepared.input.state.to_string().contains("future message"));
    assert!(!prepared.input.state.to_string().contains("has-not-spoken"));
}

#[test]
fn semantic_image_fragments_share_one_reply_and_keep_unrelated_messages_separate() {
    let mut state = GroupState::default();
    for time in [1000, 2000, 3000, 4000] {
        let seq = add(&mut state, "A", "", time, false);
        state
            .messages
            .last_mut()
            .unwrap()
            .message
            .images
            .push(ImageAttachment {
                url: format!("https://example.com/image-{seq}.png"),
                content_type: "image/png".into(),
            });
    }
    decide(&mut state, 1, Action::NewRequest, None);
    let old = state.begin_generation(1).unwrap();
    decide(&mut state, 2, Action::Append, Some(1));
    decide(&mut state, 3, Action::Append, Some(1));
    decide(&mut state, 4, Action::Silent, None);
    assert_eq!(state.requests.len(), 1);
    assert_eq!(state.requests[&1].source_seqs, vec![1, 2, 3]);
    assert!(state.record(4).unwrap().applied_request.is_none());
    assert!(
        !state
            .finish_generation(1, old, vec!["obsolete".into()])
            .unwrap()
    );
    let new = state.begin_generation(1).unwrap();
    assert!(
        state
            .finish_generation(1, new, vec!["combined reply".into()])
            .unwrap()
    );
    assert_eq!(state.requests[&1].bubbles.len(), 1);
    assert_eq!(state.messages.len(), 4);
    let config = SocialConfig {
        mode: ParticipationMode::Social,
        ..Default::default()
    };
    let prepared = jev::prepare(&state, 4, &config, "fixture-jev").unwrap();
    assert_eq!(prepared.input.state["recent_messages"][0]["image_count"], 1);
    assert_eq!(
        prepared.input.state["active_requests"][0]["messages"][0]["image_count"],
        1
    );
    assert!(
        !prepared
            .input
            .state
            .to_string()
            .contains("https://example.com")
    );
}

#[test]
fn complete_jev_distribution_is_preserved_and_invalid_choices_rejected() {
    let mut state = GroupState::default();
    let seq = add(&mut state, "A", "help", 1, false);
    let config = SocialConfig {
        mode: ParticipationMode::Helpful,
        ..Default::default()
    };
    let prepared = jev::prepare(&state, seq, &config, "jev-1.14.0").unwrap();
    let mut answers = BTreeMap::new();
    for (name, q) in &prepared.input.questions {
        let choice = match name.as_str() {
            "relation" => "new_request",
            "participation" => "engage",
            "you_referent" => "absent",
            "task_target" => "none",
            "reply_boundary" => "allow",
            "audience" => "speaker",
            "exchange" => "bot",
            _ => "answer",
        };
        let probabilities = q
            .criteria
            .keys()
            .map(|key| (key.clone(), if key == choice { 1.0 } else { 0.0 }))
            .collect::<BTreeMap<_, _>>();
        answers.insert(
            name,
            json!({"type":"choice","choice":choice,"confidence":1.0,"probabilities":probabilities}),
        );
    }
    let mut response = json!({"model":"jev-1.14.0","answers":answers});
    let decision = jev::decode(&serde_json::to_vec(&response).unwrap(), &prepared).unwrap();
    assert_eq!(decision.action, Action::NewRequest);
    assert_eq!(decision.judgments.len(), 8);
    response["answers"]["relation"]["choice"] = "invented".into();
    assert!(jev::decode(&serde_json::to_vec(&response).unwrap(), &prepared).is_err());
}

#[test]
fn separate_group_files_and_private_atomic_storage_survive_large_snapshots() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    private_directory(root.path()).unwrap();
    let _lock = acquire_lock(root.path()).unwrap();
    assert!(acquire_lock(root.path()).is_err());
    let first = Store::new(root.path(), "group-one").unwrap();
    let second = Store::new(root.path(), "group-two").unwrap();
    let mut state = GroupState::default();
    for _ in 0..60 {
        add(&mut state, "A", &"大".repeat(600), 1000, false);
    }
    first.save(&state).unwrap();
    assert_eq!(first.load().unwrap().messages.len(), 60);
    assert!(second.load().unwrap().messages.is_empty());
    assert_eq!(
        std::fs::metadata(&first.path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn default_model_and_reasoning_match_user_choice_and_unknown_fields_fail() {
    let config = SocialConfig::default();
    assert_eq!(config.codex.model, "gpt-6.1-sol");
    assert_eq!(config.codex.reasoning_effort, "high");
    assert!(toml::from_str::<SocialConfig>("made_up_setting = true").is_err());
}

#[test]
fn operator_retry_requeues_failures_without_touching_unknown_or_sent_bubbles() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut state = one_request();
    let revision = state.begin_generation(1).unwrap();
    state
        .finish_generation(1, revision, vec!["first".into(), "unknown".into()])
        .unwrap();
    runtime::finish_send(&mut state, 1, 0, Ok(ack()));
    runtime::finish_send(&mut state, 1, 1, Err(unknown()));
    let seq = add(&mut state, "A", "new request", 2000, true);
    decide(&mut state, seq, Action::NewRequest, None);
    state.requests.get_mut(&2).unwrap().status = RequestStatus::Failed;
    let seq = add(&mut state, "B", "failed decision", 3000, false);
    state.begin_decision(seq).unwrap();
    state.fail_decision(seq);
    let store = Store::new(root.path(), "test-group").unwrap();
    store.save(&state).unwrap();
    let config = SocialConfig {
        data_dir: root.path().to_owned(),
        allowed_groups: vec!["test-group".into()],
        ..Default::default()
    };
    super::store::retry_failed(&config, 0).unwrap();
    let recovered = store.load().unwrap();
    assert_eq!(
        recovered.requests[&1].status,
        RequestStatus::UnknownDelivery
    );
    assert_eq!(
        recovered.requests[&1].bubbles[0].status,
        DeliveryStatus::Sent
    );
    assert_eq!(
        recovered.requests[&1].bubbles[1].status,
        DeliveryStatus::Unknown
    );
    assert_eq!(recovered.requests[&2].status, RequestStatus::Queued);
    assert_eq!(
        recovered.record(seq).unwrap().status,
        DecisionStatus::Pending
    );
}

#[tokio::test]
async fn deadline_cancels_and_waits_for_backend_terminal_event() {
    let (external_tx, external_rx) = tokio::sync::oneshot::channel();
    let (backend_tx, backend_rx) = tokio::sync::oneshot::channel();
    let execution = async {
        backend_rx.await.unwrap();
        Ok::<_, anyhow::Error>("stopped")
    };
    let result = runtime::bounded_execution(
        execution,
        external_rx,
        backend_tx,
        Duration::from_millis(5),
        Duration::from_millis(100),
    )
    .await
    .unwrap();
    assert_eq!(result, "stopped");
    drop(external_tx);
}

#[tokio::test]
async fn unresponsive_interrupt_is_fatal_instead_of_reusing_active_thread() {
    let (_external_tx, external_rx) = tokio::sync::oneshot::channel();
    let (backend_tx, _backend_rx) = tokio::sync::oneshot::channel();
    let result = runtime::bounded_execution(
        std::future::pending::<anyhow::Result<()>>(),
        external_rx,
        backend_tx,
        Duration::from_millis(5),
        Duration::from_millis(5),
    )
    .await;
    assert!(result.is_err());
}

fn followup_context(author: &str, followup: &str) -> (GroupState, u64) {
    let mut state = GroupState::default();
    let first = add(&mut state, "human-A", "你能帮我定闹钟么", 1000, false);
    decide(&mut state, first, Action::NewRequest, None);
    let request = state.requests.get_mut(&1).unwrap();
    request.status = RequestStatus::Completed;
    request.bubbles.push(Bubble {
        text:
            "可以帮你设群内定时消息提醒，但不能直接设置手机响铃闹钟。你想明天几点提醒，提醒什么呀？"
                .into(),
        status: DeliveryStatus::Sent,
        sent_at_ms: Some(2000),
        reply_to_seq: Some(first),
        platform_message_id: Some("bot-alarm-answer".into()),
        revision: 1,
        ..Default::default()
    });
    let next = add(&mut state, author, followup, 36_000, false);
    (state, next)
}

#[test]
fn jev_compact_metadata_keeps_identity_outcomes_and_reports_budgeted_window() {
    let mut state = GroupState::default();
    for index in 0..20 {
        let seq = add(
            &mut state,
            "human-A",
            "这个计划再核对一下",
            1000 + index * 1000,
            false,
        );
        decide(&mut state, seq, Action::NewRequest, None);
        let request = state.requests.values_mut().last().unwrap();
        request.status = RequestStatus::Completed;
        request.generated_revision = request.revision;
        request.delivered_revision = request.revision;
        request.bubbles.push(Bubble {
            text: "已核对这一步。".into(),
            status: DeliveryStatus::Sent,
            sent_at_ms: Some(1500 + index * 1000),
            revision: request.revision,
            ..Default::default()
        });
    }
    let seq = add(&mut state, "human-A", "我把后续资料接给你了", 25000, false);
    let mut config = SocialConfig {
        mode: ParticipationMode::Social,
        ..Default::default()
    };
    config.context_windows.jev_messages = Some(20);
    let prepared = jev::prepare(&state, seq, &config, "fixture").unwrap();
    let history = prepared.input.state["recent_messages"].as_array().unwrap();
    let omitted = prepared.input.state["recent_messages_omitted_for_budget"]
        .as_u64()
        .unwrap();
    assert_eq!(history.len() as u64 + omitted, 20);
    assert!(!history.is_empty());
    assert_eq!(
        history[0]["metadata"]["message_ref"],
        format!("M{}", omitted + 1)
    );
    assert_eq!(history.last().unwrap()["seq"], 20);
    assert!(history[0]["metadata"]["message_id"].is_string());
    assert_eq!(history[0]["timestamp_ms"], (omitted + 1) * 1000);
    assert_eq!(
        history[0]["metadata"]["processing"]["request"]["state"],
        "completed"
    );
    assert_eq!(
        history[0]["metadata"]["processing"]["request"]["source_reply_complete"],
        true
    );
    assert!(history[0]["metadata"]["processing"]["request"]["current_delivery_counts"].is_null());
    let full = super::message_context::describe(
        &super::history::Item::Incoming(Box::new(state.messages[0].clone())),
        &state,
        None,
    )
    .unwrap();
    assert!(full["processing"]["request"]["current_delivery_counts"].is_object());
    assert!(serde_json::to_vec(&prepared.input.state).unwrap().len() <= 24000);
}

#[tokio::test]
#[ignore = "requires CLAWMINGLE_JEV_EVAL_CONFIG and explicitly authorized real Jev requests"]
async fn live_jev_direct_progress_and_elliptical_followup() {
    let path = std::env::var("CLAWMINGLE_JEV_EVAL_CONFIG").unwrap();
    let mut config = SocialConfig::load(std::path::Path::new(&path)).unwrap();
    let client = jev::JevClient::new(&config).unwrap();
    let mut failures = Vec::new();
    for (name, mode, current, earlier, human_target, expected) in [
        (
            "direct_progress_social",
            ParticipationMode::Social,
            "我在给你接入在线文档管理功能",
            "",
            false,
            true,
        ),
        (
            "direct_progress_helpful",
            ParticipationMode::Helpful,
            "我在给你接入在线文档管理功能",
            "",
            false,
            true,
        ),
        (
            "ellipsis_after_silent",
            ParticipationMode::Social,
            "还有网页查询工具",
            "我在给你接入在线文档管理功能",
            false,
            true,
        ),
        (
            "human_work_coordination",
            ParticipationMode::Social,
            "小B，我在给你接入在线文档管理功能",
            "小A，帮我给公司的应用装上文档功能",
            true,
            false,
        ),
        (
            "about_bot_to_human",
            ParticipationMode::Helpful,
            "小B，我给这机器人接了网页工具，咱们明天一起试试",
            "",
            true,
            false,
        ),
    ] {
        config.mode = mode;
        let (mut state, _) = followup_context("human-A", current);
        state.messages[0].message.text = "之后会给你接入一些工具".into();
        state.requests.get_mut(&1).unwrap().bubbles[0].text =
            "接好后叫我一声，我们实际试试。".into();
        state.messages.pop();
        if !earlier.is_empty() {
            let seq = add(
                &mut state,
                if human_target { "human-B" } else { "human-A" },
                earlier,
                1300000,
                false,
            );
            decide(&mut state, seq, Action::Silent, None);
        }
        let seq = add(&mut state, "human-A", current, 1312000, false);
        if human_target {
            state
                .messages
                .last_mut()
                .unwrap()
                .message
                .mentioned_user_ids
                .push("human-B".into());
        }
        for record in &mut state.messages {
            record.message.author_name = Some(
                if record.message.author_id == "human-A" {
                    "小A"
                } else {
                    "小B"
                }
                .into(),
            );
        }
        let decision = client.judge(&state, seq, &config).await.unwrap();
        let replied = matches!(
            decision.action,
            Action::NewRequest | Action::Append | Action::Revise
        );
        println!(
            "{name}: {:?} {:?}; exchange {:?}",
            decision.action,
            decision.judgments["relation"].probabilities,
            decision.judgments["exchange"].choice
        );
        if replied != expected {
            failures.push(name);
        }
    }
    assert!(
        failures.is_empty(),
        "unexpected direct-address decisions: {failures:?}"
    );
}

#[test]
fn jev_followup_context_carries_actual_bot_time_addressee_and_custom_policy() {
    let (mut state, seq) = followup_context("human-B", "人家需要你学习一下怎么设置闹铃钟");
    let mut older = state.requests[&1].clone();
    older.id = 99; // Larger request id does not mean a more recent reply.
    older.bubbles[0].sent_at_ms = Some(1500);
    older.bubbles[0].text = "earlier reply".into();
    state.requests.insert(99, older);
    let mut config = SocialConfig {
        mode: ParticipationMode::Helpful,
        ..Default::default()
    };
    config.context_windows.jev_bot_messages = 1;
    config
        .behavior
        .modes
        .get_mut("mode-2")
        .unwrap()
        .participation_instructions = "custom operator policy".into();
    let prepared = jev::prepare(&state, seq, &config, "fixture-jev").unwrap();
    let input = prepared.input.state;
    assert_eq!(input["policy"]["instructions"], "custom operator policy");
    assert_eq!(input["recent_bot_replies"].as_array().unwrap().len(), 1);
    let reply = &input["recent_bot_replies"][0];
    assert_eq!(reply["request_id"], 1);
    assert_eq!(reply["timestamp_ms"], 2000);
    assert_eq!(reply["speaker"], "bot");
    assert_eq!(
        reply["request_owner"],
        input["recent_messages"][0]["speaker"]
    );
    assert_eq!(reply["audience"], input["recent_messages"][0]["speaker"]);
    assert_ne!(reply["audience"], input["current_message"]["speaker"]);
    assert_eq!(reply["reply_to_seq"], 1);
    for mode in [ParticipationMode::Mentions, ParticipationMode::All] {
        config.mode = mode;
        assert!(jev::prepare(&state, seq, &config, "fixture-jev").is_err());
    }
}

#[test]
fn separate_context_windows_do_not_omit_current_request_or_custom_response_policy() {
    let (mut state, _) = followup_context("human-A", "那你学习一下");
    for time in 40..44 {
        add(
            &mut state,
            "human-B",
            "human conversation",
            time * 1000,
            false,
        );
    }
    let mut config = SocialConfig::default();
    config.context_windows.codex_messages = Some(2);
    config.context_windows.jev_messages = Some(1);
    config
        .behavior
        .categories
        .get_mut("explanation")
        .unwrap()
        .instructions = "explanation policy fixture".into();
    let prompt = runtime::codex_prompt(&state, 1, &config).unwrap();
    assert!(prompt.contains("explanation policy fixture"));
    assert!(!prompt.contains("bubbles (short text strings)"));
    let context: Value = serde_json::from_str(prompt.rsplit("\n\n").next().unwrap()).unwrap();
    assert_eq!(context["group_context"].as_array().unwrap().len(), 2);
    assert_eq!(
        context["current_request"]["messages"][0]["text"],
        "你能帮我定闹钟么"
    );
    config.mode = ParticipationMode::Helpful;
    let input = jev::prepare(&state, state.next_seq - 1, &config, "fixture-jev")
        .unwrap()
        .input
        .state;
    assert_eq!(input["recent_messages"].as_array().unwrap().len(), 1);
}

/// Explicit opt-in: synthetic messages only, no QQ sends and no live history changes.
#[tokio::test]
#[ignore = "requires stopped bot, independent Codex login and CLAWMINGLE_STYLE_EVAL_CONFIG; manually review samples"]
async fn live_codex_conversation_style_samples() {
    use crate::codex::{AppServerHandle, ClientInfo, IsolatedAppServerConfig};
    let path = std::env::var("CLAWMINGLE_STYLE_EVAL_CONFIG")
        .expect("explicit private config path required");
    let config = SocialConfig::load(std::path::Path::new(&path)).unwrap();
    assert!(config.codex.credentials_file.is_none() && config.codex.provider.is_none());
    let _lock = acquire_lock(&config.data_dir).expect("stop bot before style evaluation");
    let handle = AppServerHandle::start_isolated(
        IsolatedAppServerConfig {
            bubblewrap_binary: config.codex.bubblewrap_binary.clone(),
            codex_binary: config.codex.binary.clone(),
            runtime_root: config.data_dir.join("runtime"),
            provider_environment: BTreeMap::new(),
            selected_provider: None,
            image_worker: false,
            codex_app_worker: false,
        },
        ClientInfo {
            name: "clawmingle-style-evaluation".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: None,
        },
    )
    .await
    .unwrap();
    // Use the real prompt/schema in fresh threads, with no QQ delivery or application tools.
    // This checks transport and output shape; naturalness is reviewed by a human, not an assertion.
    let outcome: anyhow::Result<()> = async {
        for (name, text, style) in [
            (
                "food_sharing",
                "今天第一次挑战特辣火锅，嘴已经不是自己的了",
                "chat",
            ),
            (
                "shared_joke",
                "公司说周末爬山算团建，这不就是换个山头继续加班",
                "chat",
            ),
            (
                "explanation",
                "能解释一下递归为什么需要终止条件吗？给我一个小例子。",
                "answer",
            ),
        ] {
            let mut state = GroupState::default();
            let seq = add(
                &mut state,
                "human-A",
                text,
                chrono::Utc::now().timestamp_millis(),
                false,
            );
            decide(&mut state, seq, Action::NewRequest, None);
            state.requests.get_mut(&1).unwrap().style = style.into();
            let prompt = runtime::codex_prompt(&state, 1, &config)?;
            let request = runtime::execution_request(&state, prompt, &config)?;
            let result = tokio::time::timeout(
                Duration::from_secs(120),
                handle.execute_social_with_input(
                    request,
                    super::reply::bubble_schema(&config),
                    Default::default(),
                    None,
                    None,
                ),
            )
            .await??;
            let reply = super::reply::parse_reply(&result.text, &config, 0)?;
            assert!(!reply.bubbles.is_empty());
            println!("{name}: {}", serde_json::to_string(&reply.bubbles)?);
        }
        Ok(())
    }
    .await;
    handle.shutdown().await;
    outcome.unwrap();
}

/// Explicit opt-in: synthetic messages only, no QQ sends and no live history changes.
#[tokio::test]
#[ignore = "requires CLAWMINGLE_JEV_EVAL_CONFIG and explicitly authorized real Jev requests"]
async fn live_jev_proactive_participation_regression() {
    let path =
        std::env::var("CLAWMINGLE_JEV_EVAL_CONFIG").expect("explicit private config path required");
    let mut config = SocialConfig::load(std::path::Path::new(&path)).unwrap();
    let client = jev::JevClient::new(&config).unwrap();
    let mut failures = Vec::new();
    for (name, mode, previous, text, should_reply) in [
        (
            "open_sharing",
            ParticipationMode::Social,
            "",
            "今天第一次挑战特辣火锅，嘴已经不是自己的了",
            true,
        ),
        (
            "sleepy_sharing",
            ParticipationMode::Social,
            "",
            "午睡两小时，醒来晒晒太阳",
            true,
        ),
        (
            "shared_banter",
            ParticipationMode::Social,
            "周末团建居然安排爬山",
            "这哪是团建，换个山头继续加班",
            true,
        ),
        (
            "helpful_mode_sharing",
            ParticipationMode::Helpful,
            "",
            "午睡两小时，醒来晒晒太阳",
            false,
        ),
        (
            "helpful_mode_banter",
            ParticipationMode::Helpful,
            "周末团建居然安排爬山",
            "这哪是团建，换个山头继续加班",
            false,
        ),
        (
            "focused_exchange",
            ParticipationMode::Social,
            "小A，合同上次修改的金额是多少？我现在就填进去",
            "小B，最后那版是三千，你先核对文件再填",
            false,
        ),
        (
            "explicit_opt_out",
            ParticipationMode::Social,
            "",
            "小B，刚才的话只和你说，机器人先别接话",
            false,
        ),
        (
            "closing",
            ParticipationMode::Social,
            "",
            "就聊到这吧，我先去忙，不用回啦",
            false,
        ),
    ] {
        config.mode = mode;
        let mut state = GroupState::default();
        if !previous.is_empty() {
            add(&mut state, "human-B", previous, 1000, false);
        }
        let seq = add(&mut state, "human-A", text, 3000, false);
        for record in &mut state.messages {
            record.message.author_name = Some(
                if record.message.author_id == "human-A" {
                    "小A"
                } else {
                    "小B"
                }
                .into(),
            );
        }
        let decision = client.judge(&state, seq, &config).await.unwrap();
        let responded = matches!(
            decision.action,
            Action::NewRequest | Action::Append | Action::Revise
        );
        println!(
            "{name}: {:?}; {:?}; exchange {:?}",
            decision.action,
            decision.judgments["relation"].probabilities,
            decision.judgments["exchange"].choice
        );
        if responded != should_reply {
            failures.push(name);
        }
    }
    assert!(
        failures.is_empty(),
        "unexpected social decisions: {failures:?}"
    );
}

/// Explicit opt-in: synthetic messages only, no QQ sends and no live history changes.
#[tokio::test]
#[ignore = "requires explicit Jev config and a local reaction-image fixture; no QQ sends"]
async fn live_jev_emotional_burst_regression() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let config_path =
        std::env::var("CLAWMINGLE_JEV_EVAL_CONFIG").expect("explicit config required");
    let image_path =
        std::env::var("CLAWMINGLE_JEV_EVAL_IMAGE").expect("explicit image fixture required");
    let image = format!(
        "data:image/png;base64,{}",
        STANDARD.encode(std::fs::read(image_path).unwrap())
    );
    let mut config = SocialConfig::load(std::path::Path::new(&config_path)).unwrap();
    config.mode = ParticipationMode::Helpful;
    let client = jev::JevClient::new(&config).unwrap();
    for (name, text, unfinished, expected) in [
        ("emotional_reply", "", false, Action::NewRequest),
        ("emotional_append", "", true, Action::Append),
        (
            "human_addressee",
            "小B，我是对你发表情，机器人不用回",
            true,
            Action::Silent,
        ),
        (
            "closing",
            "好了，谢谢你，不用再回复了",
            false,
            Action::Silent,
        ),
    ] {
        let mut state = GroupState::default();
        let first = add(
            &mut state,
            "human-A",
            "Codex，你刚才的玩笑让我有点委屈",
            1000,
            false,
        );
        state
            .messages
            .last_mut()
            .unwrap()
            .message
            .images
            .push(ImageAttachment::default());
        decide(&mut state, first, Action::NewRequest, None);
        if !unfinished {
            let revision = state.begin_generation(1).unwrap();
            state
                .finish_generation(
                    1,
                    revision,
                    vec!["抱歉，刚才那句话过头了。你愿意跟我说说吗？".into()],
                )
                .unwrap();
            let request = state.requests.get_mut(&1).unwrap();
            request.status = RequestStatus::Completed;
            request.updated_at_ms = 2000;
            request.bubbles[0].status = DeliveryStatus::Sent;
            request.bubbles[0].sent_at_ms = Some(2000);
        }
        let seq = add(&mut state, "human-A", text, 3000, false);
        state
            .messages
            .last_mut()
            .unwrap()
            .message
            .images
            .push(ImageAttachment::default());
        let mut recent = jev::RecentImages::default();
        recent.remember(first, vec![image.clone()]);
        let decision = client
            .judge_with_images(
                &state,
                seq,
                &config,
                None,
                std::slice::from_ref(&image),
                &recent,
            )
            .await
            .unwrap();
        println!(
            "{name}: {:?}; {:?}",
            decision.action, decision.judgments["relation"].probabilities
        );
        if expected == Action::NewRequest {
            assert!(
                matches!(decision.action, Action::NewRequest | Action::Append),
                "{name}"
            );
        } else {
            assert_eq!(decision.action, expected, "{name}");
        }
    }
}

/// Explicit opt-in: synthetic messages only, no QQ sends and no live history changes.
#[tokio::test]
#[ignore = "requires CLAWMINGLE_JEV_EVAL_CONFIG and explicitly authorized real Jev requests"]
async fn live_jev_followup_policy_regression() {
    let path =
        std::env::var("CLAWMINGLE_JEV_EVAL_CONFIG").expect("explicit private config path required");
    let mut config = SocialConfig::load(std::path::Path::new(&path)).unwrap();
    config.mode = ParticipationMode::Helpful;
    let client = jev::JevClient::new(&config).unwrap();
    for (name, author, text, addressed_to_human, should_reply) in [
        (
            "same_speaker_followup",
            "human-A",
            "那你学习一下",
            false,
            true,
        ),
        (
            "other_member_clarifies",
            "human-B",
            "人家需要你学习一下怎么设置闹铃钟",
            false,
            true,
        ),
        (
            "human_to_human",
            "human-A",
            "小B，那你学习一下怎么设闹钟吧，机器人不用回答",
            true,
            false,
        ),
        (
            "human_commentary",
            "human-A",
            "小B，这机器人说话挺逗哈哈，咱俩还是去打游戏吧",
            true,
            false,
        ),
    ] {
        let (mut state, seq) = followup_context(author, text);
        if addressed_to_human {
            state
                .messages
                .last_mut()
                .unwrap()
                .message
                .mentioned_user_ids
                .push("human-B".into());
        }
        let decision = client.judge(&state, seq, &config).await.unwrap();
        let responded = matches!(
            decision.action,
            Action::NewRequest | Action::Append | Action::Revise
        );
        println!(
            "{name}: {:?}; {:?}",
            decision.action,
            decision.judgments.get("relation").map(|j| &j.probabilities)
        );
        assert_eq!(responded, should_reply, "scenario {name}");
    }
}

#[test]
fn large_codex_context_is_trimmed_but_current_request_is_preserved() {
    let mut state = one_request();
    for time in 1..50 {
        add(
            &mut state,
            "human-B",
            &"历史内容".repeat(1800),
            2000 + time,
            false,
        );
    }
    let mut config = SocialConfig::default();
    config.context_windows.codex_messages = Some(80);
    let prompt = runtime::codex_prompt(&state, 1, &config).unwrap();
    let raw = prompt.rsplit("\n\n").next().unwrap();
    assert!(raw.len() <= 128 * 1024);
    let input: Value = serde_json::from_str(raw).unwrap();
    assert!(
        input["context_messages_omitted_for_budget"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        input["current_request"]["messages"][0]["text"],
        "帮我选晚饭"
    );
}

/// Synthetic incident replay; never sends QQ messages or changes live history.
#[tokio::test]
#[ignore = "requires CLAWMINGLE_JEV_EVAL_CONFIG and explicit real Jev evaluation"]
async fn live_jev_branching_and_pronoun_incident() {
    let path = std::env::var("CLAWMINGLE_JEV_EVAL_CONFIG").expect("explicit config");
    let mut config = SocialConfig::load(std::path::Path::new(&path)).unwrap();
    config.mode = ParticipationMode::Social;
    let client = jev::JevClient::new(&config).unwrap();
    for (name, text, human, expected) in [
        (
            "challenge_bot_evidence",
            "你怎么知道这两张图对应的是不同手机端，iPhone/Android？",
            false,
            true,
        ),
        (
            "human_you",
            "小B，你怎么知道这两张图对应不同手机？",
            true,
            false,
        ),
        (
            "explicit_silence",
            "你先不用回复，等我把图发完。",
            false,
            false,
        ),
    ] {
        let (mut state, seq) = followup_context("human-A", text);
        state.messages[0].message.text = "为什么同一条消息显示不一样？".into();
        state.requests.get_mut(&1).unwrap().bubbles[0].text =
            "两张截图看起来分别是 Android 和 iPhone 的界面。".into();
        if human {
            state.messages.insert(
                0,
                MessageRecord {
                    seq: 0,
                    message: ChatMessage {
                        id: "known-human".into(),
                        author_id: "human-B".into(),
                        author_name: Some("小B".into()),
                        text: "我也在群里。".into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            );
            state
                .messages
                .last_mut()
                .unwrap()
                .message
                .mentioned_user_ids
                .push("human-B".into());
        }
        let decision = client.judge(&state, seq, &config).await.unwrap();
        println!(
            "{name}: {:?}; branches={}",
            decision.action,
            serde_json::to_string(&decision.judgments).unwrap()
        );
        assert_eq!(decision.action != Action::Silent, expected, "{name}");
    }
    let mut state = GroupState::default();
    let seq = add(
        &mut state,
        "human-A",
        "你好，Codex，你能帮我看看吗？",
        1000,
        true,
    );
    let decision = client.judge(&state, seq, &config).await.unwrap();
    assert_eq!(decision.action, Action::NewRequest);
    println!("no_prior_task: {:?}", decision.action);
}

#[test]
fn private_reply_and_steer_configuration_use_all_mode_preferences() {
    let mut state = one_request();
    state.mode_override = Some(ParticipationMode::Helpful);
    let config = SocialConfig::default();
    let effective =
        super::control::effective_config_for_scope(&config, &state, Some(1), true).unwrap();
    assert_eq!(effective.mode, ParticipationMode::All);
    assert!(
        runtime::codex_prompt(&state, 1, &effective)
            .unwrap()
            .contains(&config.behavior.modes["mode-all"].reply_instructions)
    );
    assert_eq!(
        super::control::effective_config_for_scope(&config, &state, Some(1), false)
            .unwrap()
            .mode,
        ParticipationMode::Helpful
    );
}

#[test]
fn jev_old_bot_audience_does_not_follow_later_request_revision() {
    let (mut state, seq) = followup_context("human-B", "人家需要你学习一下怎么设置闹铃钟");
    decide(&mut state, seq, Action::Append, Some(1));
    let later = add(&mut state, "human-B", "还有呢", 40_000, false);
    let config = SocialConfig {
        mode: ParticipationMode::Helpful,
        ..Default::default()
    };
    let input = jev::prepare(&state, later, &config, "fixture-jev")
        .unwrap()
        .input
        .state;
    let reply = &input["recent_bot_replies"][0];
    assert_eq!(reply["audience"], input["recent_messages"][0]["speaker"]);
    assert_ne!(reply["audience"], input["current_message"]["speaker"]);
    assert_eq!(reply["source_seqs"], json!([1]));
}
