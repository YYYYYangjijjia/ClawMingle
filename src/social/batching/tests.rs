use super::*;
use crate::{
    model::social::{Action, ChatMessage, Decision, DeliveryStatus, ParticipationMode},
    social::{history::History, state::rule_decision},
};
use serde_json::json;

pub(super) fn fixture() -> (tempfile::TempDir, SocialConfig, History, GroupState) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = SocialConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    };
    let history = History::new(dir.path(), "fixture-group").unwrap();
    let mut state = GroupState::default();
    for (id, author) in [(1, "member-one"), (2, "member-two")] {
        let seq = state
            .ingest(
                ChatMessage {
                    id: format!("message-{id}"),
                    author_id: author.into(),
                    text: format!("question-{id}"),
                    timestamp_ms: 1000,
                    at_bot: true,
                    ..Default::default()
                },
                100,
            )
            .unwrap()
            .unwrap();
        state.begin_decision(seq).unwrap();
        let decision = rule_decision(
            ParticipationMode::Mentions,
            &state.record(seq).unwrap().message,
        )
        .unwrap();
        state.apply_decision(seq, decision, 16).unwrap();
    }
    (dir, config, history, state)
}
fn members() -> Vec<Member> {
    vec![Member { id: 1, revision: 1 }, Member { id: 2, revision: 1 }]
}
fn text_result(id: u64, text: &str) -> serde_json::Value {
    json!({"request_id":id,"revision":1,"outcome":"answer","bubbles":[text],"quote_seq":id,"mentions":[]})
}
fn short_answers() -> Vec<review::Verdict> {
    vec![
        review::Verdict {
            id: 1,
            short: true,
            obsolete: false,
        },
        review::Verdict {
            id: 2,
            short: true,
            obsolete: false,
        },
    ]
}

#[test]
fn separate_authors_share_one_batch_but_keep_outboxes_and_quotes() {
    let (_dir, config, history, mut state) = fixture();
    let batch = Batch::prepare(&state, members(), &config, &history, false)
        .unwrap()
        .unwrap();
    batch.begin(&mut state).unwrap();
    batch
        .complete(
            &mut state,
            &json!({"results":[text_result(1,"first answer"),text_result(2,"second answer")]})
                .to_string(),
            false,
        )
        .unwrap();
    for id in [1, 2] {
        let r = &state.requests[&id];
        assert_eq!(r.status, RequestStatus::Ready);
        assert_eq!(r.quote_source_seq, Some(id));
        assert_eq!(r.bubbles[0].passive_message_id, format!("message-{id}"));
        assert_eq!(r.bubbles[0].status, DeliveryStatus::Pending);
        assert_eq!(r.generated_revision, 1);
        assert_eq!(r.delivered_revision, 0);
        assert!(r.codex_thread_id.is_none());
        assert_eq!(r.source_seqs, vec![id]);
    }
}

#[test]
fn one_member_changes_without_discarding_other_answer() {
    for cancel in [false, true] {
        let (_dir, config, history, mut state) = fixture();
        let batch = Batch::prepare(&state, members(), &config, &history, false)
            .unwrap()
            .unwrap();
        batch.begin(&mut state).unwrap();
        let seq = state
            .ingest(
                ChatMessage {
                    id: "amendment".into(),
                    author_id: "member-one".into(),
                    text: "change it".into(),
                    ..Default::default()
                },
                100,
            )
            .unwrap()
            .unwrap();
        state.begin_decision(seq).unwrap();
        state
            .apply_decision(
                seq,
                Decision {
                    action: if cancel {
                        Action::Cancel
                    } else {
                        Action::Revise
                    },
                    request_id: Some(1),
                    ..Default::default()
                },
                16,
            )
            .unwrap();
        assert!(batch.current(&state));
        batch
            .complete(
                &mut state,
                &json!({"results":[text_result(1,"stale"),text_result(2,"valid")]}).to_string(),
                false,
            )
            .unwrap();
        assert!(state.requests[&1].bubbles.is_empty());
        assert_eq!(state.requests[&2].bubbles[0].text, "valid");
    }
}

#[test]
fn invalid_duplicate_or_missing_member_does_not_lose_valid_member() {
    for bad in [
        json!([]),
        json!([text_result(1, "one"), text_result(1, "duplicate")]),
        json!([{"request_id":1,"revision":99}]),
    ] {
        let (_dir, config, history, mut state) = fixture();
        let batch = Batch::prepare(&state, members(), &config, &history, false)
            .unwrap()
            .unwrap();
        batch.begin(&mut state).unwrap();
        let mut results = bad.as_array().unwrap().clone();
        results.push(text_result(2, "keep this"));
        results.push(text_result(999, "untrusted id"));
        batch
            .complete(&mut state, &json!({"results":results}).to_string(), false)
            .unwrap();
        assert!(state.requests[&1].failure_code.is_some());
        assert_eq!(state.requests[&2].bubbles[0].text, "keep this");
        assert!(!state.requests.contains_key(&999));
    }
}

#[test]
fn defer_goes_to_individual_tools_without_rebatching_or_reusing_shared_thread() {
    let (_dir, config, history, mut state) = fixture();
    let batch = Batch::prepare(&state, members(), &config, &history, false)
        .unwrap()
        .unwrap();
    batch.begin(&mut state).unwrap();
    batch.complete(&mut state,&json!({"results":[{"request_id":1,"revision":1,"outcome":"defer","bubbles":[],"mentions":[],"quote_seq":null},text_result(2,"done")]}).to_string(),false).unwrap();
    assert_eq!(state.requests[&1].status, RequestStatus::Queued);
    assert_eq!(state.requests[&1].batch_individual_revision, Some(1));
    assert!(!fresh(&state.requests[&1]));
    assert!(state.requests.values().all(|r| r.codex_thread_id.is_none()));
    let restored: GroupState =
        serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    assert_eq!(restored.requests[&1].batch_individual_revision, Some(1));
}

#[test]
fn failed_model_execution_is_not_automatically_replayed_and_reports_once() {
    let (_dir, config, history, mut state) = fixture();
    let batch = Batch::prepare(&state, members(), &config, &history, false)
        .unwrap()
        .unwrap();
    batch.begin(&mut state).unwrap();
    batch
        .fail(&mut state, "model_rate_limited", "limited")
        .unwrap();
    assert_eq!(
        state
            .requests
            .values()
            .map(|r| r.bubbles.len())
            .sum::<usize>(),
        1
    );
    assert!(
        state
            .requests
            .values()
            .all(|r| r.failure_code.as_deref() == Some("model_rate_limited") && !fresh(r))
    );
    batch.requeue(&mut state, false);
    assert!(
        state
            .requests
            .values()
            .all(|r| r.status != RequestStatus::Queued)
    );
}

#[test]
fn only_predispatch_refusal_can_reenter_batch_and_restart_remains_conservative() {
    let (_dir, config, history, mut state) = fixture();
    let batch = Batch::prepare(&state, members(), &config, &history, false)
        .unwrap()
        .unwrap();
    batch.begin(&mut state).unwrap();
    batch.requeue(&mut state, false);
    assert!(state.requests.values().all(fresh));
    batch.begin(&mut state).unwrap();
    state.recover();
    assert!(
        state
            .requests
            .values()
            .all(|r| r.status == RequestStatus::Failed)
    );
}

#[test]
fn review_branches_preserve_explicit_requests_and_withdraw_only_current_optional_chat() {
    for later in [false, true] {
        let (_dir, config, _history, mut state) = fixture();
        state.requests.get_mut(&1).unwrap().style = "chat".into();
        state.messages[0].message.at_bot = false;
        let review = Review::new(&state, &[1, 2], 7, 10_000);
        if later {
            state
                .ingest(
                    ChatMessage {
                        id: "later".into(),
                        author_id: "member-two".into(),
                        text: "new context".into(),
                        ..Default::default()
                    },
                    100,
                )
                .unwrap();
        }
        let mut coordinator = Coordinator::default();
        coordinator.reviewed(
            &mut state,
            &review,
            Ok(vec![
                review::Verdict {
                    id: 1,
                    short: true,
                    obsolete: true,
                },
                review::Verdict {
                    id: 2,
                    short: true,
                    obsolete: true,
                },
            ]),
            10_000,
        );
        assert_eq!(
            state.requests[&1].status,
            if later {
                RequestStatus::Queued
            } else {
                RequestStatus::Cancelled
            }
        );
        assert_eq!(state.requests[&2].status, RequestStatus::Queued);
        assert!(state.record(1).is_ok());
        let _ = config;
    }
}

#[test]
fn changed_revision_policy_or_new_429_requires_new_review() {
    let (_dir, config, _history, mut state) = fixture();
    let review = Review::new(&state, &[1, 2], 7, 1000);
    let mut coordinator = Coordinator::default();
    coordinator.reviewed(&mut state, &review, Ok(short_answers()), 1000);
    assert!(matches!(
        coordinator.plan(&state, 1, |_| true, &config, 7, 1001),
        Plan::Batch(_)
    ));
    assert!(matches!(
        coordinator.plan(&state, 1, |_| true, &config, 8, 1001),
        Plan::Review(_)
    ));
    state.requests.get_mut(&1).unwrap().revision += 1;
    assert!(matches!(
        coordinator.plan(&state, 1, |_| true, &config, 7, 1001),
        Plan::Review(_)
    ));
    state.context_start_seq = 9;
    coordinator.reviewed(&mut state, &review, Ok(short_answers()), 1002);
    assert!(matches!(
        coordinator.plan(&state, 1, |_| true, &config, 7, 1003),
        Plan::Review(_)
    ));
}

#[test]
fn review_failure_keeps_requests_and_does_not_loop_each_tick() {
    let (_dir, config, _history, mut state) = fixture();
    let review = Review::new(&state, &[1, 2], 1, 1000);
    let mut coordinator = Coordinator::default();
    coordinator.reviewed(&mut state, &review, Err(anyhow::anyhow!("offline")), 1000);
    assert!(matches!(
        coordinator.plan(&state, 1, |_| true, &config, 1, 1001),
        Plan::Single
    ));
    assert!(
        state
            .requests
            .values()
            .all(|r| r.status == RequestStatus::Queued)
    );
}

#[test]
fn ongoing_chat_does_not_keep_reviewed_questions_waiting() {
    let (_dir, config, _history, mut state) = fixture();
    state.requests.get_mut(&2).unwrap().style = "chat".into();
    state.messages[1].message.at_bot = false;
    let review = Review::new(&state, &[1, 2], 2, 1000);
    let mut coordinator = Coordinator::default();
    // More chat arrives while Jev is working, so only the optional member's
    // relevance becomes stale. The explicit question can still start.
    state
        .ingest(
            ChatMessage {
                id: "later".into(),
                author_id: "member-two".into(),
                text: "more chat".into(),
                ..Default::default()
            },
            100,
        )
        .unwrap();
    coordinator.reviewed(&mut state, &review, Ok(short_answers()), 2000);
    assert!(matches!(
        coordinator.plan(&state, 1, |_| true, &config, 2, 2001),
        Plan::Single
    ));
    // An existing individual task must not wait for companion reviews either.
    state
        .requests
        .get_mut(&1)
        .unwrap()
        .batch_individual_revision = Some(1);
    assert!(matches!(
        coordinator.plan(&state, 1, |_| true, &config, 3, 2002),
        Plan::Single
    ));
}

#[test]
fn aged_optional_with_stale_review_yields_to_ready_explicit_request() {
    let (_dir, config, _history, mut state) = fixture();
    state.requests.get_mut(&1).unwrap().style = "chat".into();
    state.messages[0].message.at_bot = false;
    let review = Review::new(&state, &[1, 2], 2, 61_000);
    let mut coordinator = Coordinator::default();
    state
        .ingest(
            ChatMessage {
                id: "later".into(),
                author_id: "member-two".into(),
                ..Default::default()
            },
            100,
        )
        .unwrap();
    coordinator.reviewed(&mut state, &review, Ok(short_answers()), 62_000);
    let selected = coordinator.select(&state, 1, |_| true, &config, 2, 62_001);
    assert_eq!(selected, 2);
    assert!(matches!(
        coordinator.plan(&state, selected, |_| true, &config, 2, 62_001),
        Plan::Single
    ));
    // A not-yet-ready question cannot bypass its dispatch/pacing fences.
    assert_eq!(
        coordinator.select(&state, 1, |id| id != 2, &config, 2, 62_001),
        1
    );
    assert!(matches!(
        coordinator.plan(&state, 1, |_| true, &config, 2, 62_001),
        Plan::Single
    ));
}

#[test]
fn new_chat_or_delivered_reply_invalidates_batching_but_not_forward_progress() {
    for delivered in [false, true] {
        let (_dir, config, _history, mut state) = fixture();
        state.requests.get_mut(&1).unwrap().style = "chat".into();
        state.messages[0].message.at_bot = false;
        let review = Review::new(&state, &[1, 2], 2, 1000);
        let mut coordinator = Coordinator::default();
        coordinator.reviewed(&mut state, &review, Ok(short_answers()), 1000);
        if delivered {
            state
                .requests
                .get_mut(&2)
                .unwrap()
                .bubbles
                .push(crate::model::social::Bubble {
                    status: DeliveryStatus::Sent,
                    text: "human-visible reply".into(),
                    ..Default::default()
                });
        } else {
            state
                .ingest(
                    ChatMessage {
                        id: "new-chat".into(),
                        author_id: "member-two".into(),
                        text: "moved on".into(),
                        ..Default::default()
                    },
                    100,
                )
                .unwrap();
        }
        assert!(matches!(
            coordinator.plan(&state, 1, |_| true, &config, 2, 2000),
            Plan::Single
        ));
    }
}

#[test]
fn overflow_survives_pruning_then_reopens_when_capacity_returns() {
    let (_dir, _config, _history, mut state) = fixture();
    let seq = state
        .ingest(
            ChatMessage {
                id: "overflow".into(),
                author_id: "member-one".into(),
                text: "help".into(),
                at_bot: true,
                timestamp_ms: 1000,
                ..Default::default()
            },
            100,
        )
        .unwrap()
        .unwrap();
    state.begin_decision(seq).unwrap();
    state
        .apply_decision(
            seq,
            Decision {
                action: Action::NewRequest,
                ..Default::default()
            },
            2,
        )
        .unwrap();
    assert_eq!(state.admit_overflow(2).unwrap(), None);
    for r in state.requests.values_mut() {
        r.status = RequestStatus::Completed;
    }
    state.prune(100_000, 1, 100);
    assert_eq!(
        state.record(seq).unwrap().application.as_deref(),
        Some("request_queue_full")
    );
    // Continuous incoming messages must not block a previously decided intent.
    state
        .ingest(
            ChatMessage {
                id: "unrelated-pending".into(),
                author_id: "member-two".into(),
                ..Default::default()
            },
            100,
        )
        .unwrap();
    assert!(state.has_pending());
    assert_eq!(state.admit_overflow(2).unwrap(), Some(seq));
    assert_eq!(state.admit_overflow(2).unwrap(), None);
    assert_eq!(state.next_pending(seq), None);
    assert!(state.record(seq).unwrap().applied_request.is_some());
}

#[test]
fn continuous_input_and_full_optional_queue_cannot_starve_explicit_overflow() {
    for review_fails in [false, true] {
        let (_dir, config, _history, mut state) = fixture();
        for record in &mut state.messages {
            record.message.at_bot = false;
        }
        for request in state.requests.values_mut() {
            request.style = "chat".into();
        }
        for id in 3..=16 {
            let seq = state
                .ingest(
                    ChatMessage {
                        id: format!("chat-{id}"),
                        author_id: "member-one".into(),
                        ..Default::default()
                    },
                    500,
                )
                .unwrap()
                .unwrap();
            state.begin_decision(seq).unwrap();
            state
                .apply_decision(
                    seq,
                    Decision {
                        action: Action::NewRequest,
                        style: "chat".into(),
                        ..Default::default()
                    },
                    16,
                )
                .unwrap();
        }
        // Simulate an overflow already persisted by an earlier process. Admission
        // must prefer it even when newer ordinary messages are still pending.
        let explicit = state
            .ingest(
                ChatMessage {
                    id: "explicit-overflow".into(),
                    author_id: "member-two".into(),
                    at_bot: true,
                    ..Default::default()
                },
                500,
            )
            .unwrap()
            .unwrap();
        let r = state.messages.last_mut().unwrap();
        r.status = crate::model::social::DecisionStatus::Bypassed;
        r.decision = Some(Decision {
            action: Action::NewRequest,
            style: "answer".into(),
            ..Default::default()
        });
        r.application = Some("request_queue_full".into());
        let mut coordinator = Coordinator::default();
        let Plan::Review(review) = coordinator.plan(&state, 1, |_| true, &config, 1, 61_000) else {
            panic!("initial review")
        };
        for id in 0..100 {
            let seq = state
                .ingest(
                    ChatMessage {
                        id: format!("silent-{id}"),
                        author_id: "member-two".into(),
                        ..Default::default()
                    },
                    500,
                )
                .unwrap()
                .unwrap();
            state.begin_decision(seq).unwrap();
            state.apply_decision(seq, Decision::default(), 16).unwrap();
        }
        let answers = if review_fails {
            Err(anyhow::anyhow!("fixture offline"))
        } else {
            Ok((1..=16)
                .map(|id| review::Verdict {
                    id,
                    short: true,
                    obsolete: true,
                })
                .collect())
        };
        coordinator.reviewed(&mut state, &review, answers, 62_000);
        assert!(matches!(
            coordinator.plan(&state, 1, |_| true, &config, 1, 62_001),
            Plan::Single
        ));
        assert_eq!(state.admit_overflow(16).unwrap(), Some(explicit));
        let id = state.record(explicit).unwrap().applied_request.unwrap();
        assert_eq!(state.requests[&id].owner_id, "member-two");
        assert_eq!(state.requests[&id].source_seqs, vec![explicit]);
        assert_eq!(
            state.requests[&1].failure_code.as_deref(),
            Some("optional_reply_yielded_to_explicit")
        );
        let Plan::Review(review) = coordinator.plan(&state, id, |_| true, &config, 1, 62_002)
        else {
            panic!("new explicit revision reviewed once")
        };
        state
            .ingest(
                ChatMessage {
                    id: "yet-more-chat".into(),
                    author_id: "member-one".into(),
                    ..Default::default()
                },
                500,
            )
            .unwrap();
        coordinator.reviewed(
            &mut state,
            &review,
            Err(anyhow::anyhow!("fixture offline")),
            62_003,
        );
        assert!(matches!(
            coordinator.plan(&state, id, |_| true, &config, 1, 62_004),
            Plan::Single
        ));
        assert_eq!(state.requests[&id].status, RequestStatus::Queued);
    }
}

#[test]
fn capacity_priority_never_withdraws_started_long_or_directed_requests() {
    for protected in [
        "started",
        "long",
        "thread",
        "delivery",
        "directed",
        "scheduled",
    ] {
        let (_dir, _config, _history, mut state) = fixture();
        state.requests.get_mut(&1).unwrap().style = "chat".into();
        state.messages[0].message.at_bot = false;
        let r = state.requests.get_mut(&1).unwrap();
        match protected {
            "started" => r.submitted_revision = 1,
            "long" => r.long_task = true,
            "thread" => r.codex_thread_id = Some("fixture-thread".into()),
            "delivery" => r.bubbles.push(Default::default()),
            "directed" => {
                state.messages[0]
                    .decision
                    .as_mut()
                    .unwrap()
                    .judgments
                    .insert(
                        "you_referent".into(),
                        crate::model::social::ChoiceRecord {
                            choice: "bot".into(),
                            ..Default::default()
                        },
                    );
            }
            "scheduled" => r.scheduled_run_id = Some(1),
            _ => unreachable!(),
        }
        let seq = state
            .ingest(
                ChatMessage {
                    id: "explicit-new".into(),
                    author_id: "member-two".into(),
                    at_bot: true,
                    ..Default::default()
                },
                500,
            )
            .unwrap()
            .unwrap();
        state.begin_decision(seq).unwrap();
        state
            .apply_decision(
                seq,
                Decision {
                    action: Action::NewRequest,
                    style: "answer".into(),
                    ..Default::default()
                },
                2,
            )
            .unwrap();
        assert_eq!(
            state.record(seq).unwrap().application.as_deref(),
            Some("request_queue_full"),
            "{protected}"
        );
        assert!(state.requests[&1].cancelled_revision.is_none());
        assert!(state.admit_overflow(2).unwrap().is_none());
    }
}
