//! User-visible task updates, clarification and durable result retrieval.
//! Only the conversation worker mutates requests and queues QQ messages.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::config::SocialConfig;
use crate::model::social::{BubbleOrigin, Decision, DeliveryStatus, GroupState, RequestStatus};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InteractionSettings {
    pub enabled: bool,
    pub feedback_mode: FeedbackMode,
    /// Legacy configuration compatibility; never used to create a receipt.
    pub ack_delay_seconds: u64,
    pub heartbeat_interval_seconds: u64,
    pub long_timeout_seconds: u64,
    pub tool_calls: usize,
    pub workspace_timeout_seconds: u64,
    pub long_max_bubbles: usize,
    pub long_bubble_chars: usize,
    pub max_progress_messages: usize,
    pub progress_interval_seconds: u64,
}
impl Default for InteractionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            feedback_mode: FeedbackMode::Standard,
            ack_delay_seconds: 5,
            heartbeat_interval_seconds: 90,
            long_timeout_seconds: 0,
            tool_calls: 48,
            workspace_timeout_seconds: 120,
            long_max_bubbles: 4,
            long_bubble_chars: 2000,
            max_progress_messages: 2,
            progress_interval_seconds: 45,
        }
    }
}
impl InteractionSettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=30).contains(&self.ack_delay_seconds)
                && (self.heartbeat_interval_seconds == 0
                    || (30..=1800).contains(&self.heartbeat_interval_seconds))
                && (self.long_timeout_seconds == 0
                    || (60..=3600).contains(&self.long_timeout_seconds))
                && (1..=128).contains(&self.tool_calls)
                && (1..=300).contains(&self.workspace_timeout_seconds)
                && (1..=8).contains(&self.long_max_bubbles)
                && (100..=2000).contains(&self.long_bubble_chars)
                && (1..=3).contains(&self.max_progress_messages)
                && (10..=300).contains(&self.progress_interval_seconds),
            "invalid_interaction_settings"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackMode {
    Quiet,
    #[default]
    Standard,
    Detailed,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Update {
    pub kind: Kind,
    pub text: String,
    /// Current task source selected by the model, resolved to QQ's index by host.
    #[serde(default)]
    pub quote_seq: Option<u64>,
}
#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Kind {
    Ack,
    Progress,
    /// Internal read-only timing request; not advertised in the model tool schema.
    Observe,
}
pub(super) struct Event {
    pub request_id: u64,
    pub generation_revision: u64,
    pub update: Update,
    pub reply: oneshot::Sender<Value>,
}

pub(super) fn query(
    sender: mpsc::Sender<Event>,
    request_id: u64,
    generation_revision: u64,
) -> crate::codex::app_server::QqQuery {
    std::sync::Arc::new(move |raw| {
        let sender = sender.clone();
        Box::pin(async move {
            let update = serde_json::from_str::<Update>(&raw);
            let Ok(update) = update else {
                return error("invalid_update", "进展信息格式无效。");
            };
            if (update.kind != Kind::Observe && update.text.trim().is_empty())
                || update.text.chars().count() > 400
                || update.text.contains('\0')
            {
                return error("invalid_update", "进展信息应为不超过400字的自然短句。");
            }
            let (reply, rx) = oneshot::channel();
            if sender
                .send(Event {
                    request_id,
                    generation_revision,
                    update,
                    reply,
                })
                .await
                .is_err()
            {
                return error("turn_ended", "任务已结束，未发送进展。");
            }
            rx.await
                .unwrap_or_else(|_| error("turn_ended", "任务已结束，未发送进展。"))
        })
    })
}
fn error(code: &str, message: &str) -> Value {
    json!({"ok":false,"error":{"code":code,"message":message}})
}

pub(super) fn accept(
    state: &mut GroupState,
    id: u64,
    update: Update,
    config: &SocialConfig,
    now: i64,
    private: bool,
) -> Result<Value> {
    ensure!(config.interactions.enabled, "interactions_disabled");
    let request = state
        .requests
        .get(&id)
        .ok_or_else(|| anyhow::anyhow!("request_missing"))?;
    ensure!(
        request.status == RequestStatus::Generating && request.cancelled_revision.is_none(),
        "task_update_stale"
    );
    if update.kind == Kind::Observe {
        return Ok(task_timing(request, config, now));
    }
    if let Some(seq) = update.quote_seq {
        ensure!(
            request.source_seqs.contains(&seq) && state.record(seq).is_ok(),
            "task_quote_source_invalid"
        );
    }
    let origin = if update.kind == Kind::Ack {
        BubbleOrigin::TaskAck
    } else {
        BubbleOrigin::TaskProgress
    };
    let request = state.requests.get_mut(&id).expect("request retained");
    request.long_task = true;
    let feedback = super::task_feedback::ensure(request, now);
    feedback.last_activity_at_ms = Some(now);
    feedback.last_progress_at_ms = Some(now);
    feedback.last_progress_text = update.text.clone();
    let queued = super::task_feedback::queue(state, id, origin, update.text, config, now, private)?;
    if queued && let Some(seq) = update.quote_seq {
        let index = state.requests[&id]
            .bubbles
            .iter()
            .rposition(|b| b.origin == origin && b.status == DeliveryStatus::Pending)
            .expect("queued update");
        super::reply_reference::apply_reference(state, id, index, seq, 4000)?;
    }
    Ok(
        json!({"ok":true,"queued":queued,"delivered":false,"long_task":true,
        "reason":if queued {"queued"} else {"feedback_throttled_or_unavailable"},
        "final_bubble_limit":config.interactions.long_max_bubbles}),
    )
}

/// Timing is based on confirmed QQ delivery, not a model's proposed progress.
fn task_timing(
    request: &crate::model::social::RequestRecord,
    config: &SocialConfig,
    now: i64,
) -> Value {
    let feedback = request
        .feedback
        .as_ref()
        .filter(|f| f.revision == request.revision);
    let started = feedback.map_or(now, |f| f.accepted_at_ms);
    let visible = request
        .bubbles
        .iter()
        .filter(|b| {
            b.revision > request.delivered_revision
                && matches!(b.origin, BubbleOrigin::TaskAck | BubbleOrigin::TaskProgress)
                && b.status == DeliveryStatus::Sent
        })
        .filter_map(|b| b.sent_at_ms)
        .max();
    let pending = request.bubbles.iter().any(|b| {
        b.revision > request.delivered_revision
            && matches!(b.origin, BubbleOrigin::TaskAck | BubbleOrigin::TaskProgress)
            && matches!(b.status, DeliveryStatus::Pending | DeliveryStatus::Sending)
    });
    let threshold = config
        .interactions
        .heartbeat_interval_seconds
        .max(config.interactions.progress_interval_seconds)
        .max(30) as i64;
    let elapsed = now.saturating_sub(started).max(0) / 1000;
    let since = now.saturating_sub(visible.unwrap_or(started)).max(0) / 1000;
    let due = request.long_task
        && config.interactions.heartbeat_interval_seconds != 0
        && config.interactions.feedback_mode != FeedbackMode::Quiet
        && !pending
        && since >= threshold;
    json!({"ok":true,"long_task":request.long_task,"elapsed_seconds":elapsed,
        "since_last_visible_progress_seconds":visible.map(|_|since),"progress_pending":pending,"progress_due":due,
        "guidance":if due {"A long task has had no delivered model progress for a while. At your next useful pause, call task_update(kind=progress) with a concise factual update: what is verified/completed, what you are doing, or a real blocker. Do not invent progress, repeat the acknowledgement, or replace the final answer."} else {"Continue the actual task; do not send filler or another acknowledgement."}})
}

pub(super) fn reply_config(config: &SocialConfig, long: bool) -> SocialConfig {
    let mut effective = config.clone();
    if config.interactions.enabled && long {
        effective.max_bubbles = config.interactions.long_max_bubbles;
        effective.max_bubble_chars = config.interactions.long_bubble_chars;
    }
    effective
}

/// Extreme modes retain their deterministic participation rule. A precise
/// quote, or a single recent question in a private conversation, can still
/// associate the already-required reply without another model call.
pub(super) fn clarification_rule(
    state: &GroupState,
    seq: u64,
    base: Option<Decision>,
    private: bool,
    config: &SocialConfig,
) -> Result<Option<Decision>> {
    let Some(mut decision) = base else {
        return Ok(None);
    };
    if decision.action != crate::model::social::Action::NewRequest {
        return Ok(Some(decision));
    }
    let message = &state.record(seq)?.message;
    let candidates = state
        .requests
        .values()
        .filter(|r| {
            r.status == RequestStatus::WaitingForUser
                && r.cancelled_revision.is_none()
                && (r.owner_id == message.author_id
                    || r.contributor_ids.contains(&message.author_id))
        })
        .collect::<Vec<_>>();
    let quoted = candidates
        .iter()
        .filter(|r| {
            r.bubbles.iter().any(|b| {
                b.origin == BubbleOrigin::Clarification
                    && b.revision == r.revision
                    && b.status == DeliveryStatus::Sent
                    && b.reference_idx.as_ref().is_some_and(|reference| {
                        message
                            .quotes
                            .iter()
                            .any(|q| q.reference_idx.as_ref() == Some(reference))
                    })
            })
        })
        .collect::<Vec<_>>();
    let private_recent = candidates
        .iter()
        .filter(|r| {
            r.owner_id == message.author_id
                && r.bubbles
                    .iter()
                    .filter(|b| {
                        b.origin == BubbleOrigin::Clarification
                            && b.revision == r.revision
                            && b.status == DeliveryStatus::Sent
                    })
                    .filter_map(|b| b.sent_at_ms)
                    .max()
                    .is_some_and(|sent| {
                        (0..=(config.request_link_seconds * 1000) as i64)
                            .contains(&(message.timestamp_ms - sent))
                    })
        })
        .collect::<Vec<_>>();
    let selected = if quoted.len() == 1 {
        Some(**quoted.first().expect("one quote"))
    } else if quoted.is_empty()
        && private
        && candidates.len() == 1
        && private_recent.len() == 1
        && !message.text.trim_start().starts_with('/')
    {
        Some(**private_recent.first().expect("one question"))
    } else {
        None
    };
    if let Some(request) = selected {
        decision.action = crate::model::social::Action::Append;
        decision.request_id = Some(request.id);
        decision
            .request_revisions
            .insert(request.id, request.revision);
        decision.source = "clarification_reply".into();
    }
    Ok(Some(decision))
}

pub(super) fn command(
    state: &mut GroupState,
    seq: u64,
    text: &str,
    config: &SocialConfig,
    private: bool,
    target: &str,
    now: i64,
) -> Result<bool> {
    let words = text.split_whitespace().collect::<Vec<_>>();
    if words.first() != Some(&"/continue") {
        return Ok(false);
    }
    let actor = state.record(seq)?.message.author_id.clone();
    let admin = config.is_scope_admin(&actor, private, target);
    if words.len() >= 3 {
        let selected =
            super::task_identity::resolve(state, words[1]).and_then(|id| state.requests.get(&id));
        if let Some(request) = selected.filter(|r| {
            r.status == RequestStatus::WaitingForUser
                && (admin || r.owner_id == actor || r.contributor_ids.contains(&actor))
        }) {
            let id = request.id;
            let revision = request.revision;
            state.begin_decision(seq)?;
            state.apply_decision(
                seq,
                Decision {
                    action: crate::model::social::Action::Append,
                    request_id: Some(id),
                    audience_id: Some(actor),
                    source: "clarification_reply".into(),
                    request_revisions: std::collections::BTreeMap::from([(id, revision)]),
                    ..Default::default()
                },
                config.max_active_requests,
            )?;
            return Ok(true);
        }
    }
    let selected = match words.as_slice() {
        [_] => state
            .requests
            .values()
            .rev()
            .find(|r| {
                r.status == RequestStatus::AwaitingDelivery
                    && (admin || r.owner_id == actor || r.contributor_ids.contains(&actor))
            })
            .map(|r| r.id),
        [_, id] => super::task_identity::resolve(state, id),
        _ => None,
    };
    let result = selected
        .ok_or_else(|| anyhow::anyhow!("no_pending_result"))
        .and_then(|id| reanchor(state, id, seq, &actor, admin));
    state.begin_decision(seq)?;
    match result {
        Ok(()) => {
            state.apply_decision(
                seq,
                Decision {
                    source: "command".into(),
                    ..Default::default()
                },
                config.max_active_requests,
            )?;
            state
                .messages
                .iter_mut()
                .find(|m| m.seq == seq)
                .expect("source retained")
                .application = Some("command".into());
        }
        Err(_) => {
            super::control::control_reply(state,seq,"没有可继续投递的已完成结果，或你没有该请求的权限。可用 /continue 任务编号；不会重新运行任务，发送结果未知时需先核对。".into(),config,now)?;
        }
    }
    Ok(true)
}

pub(super) fn reanchor(
    state: &mut GroupState,
    id: u64,
    seq: u64,
    actor: &str,
    admin: bool,
) -> Result<()> {
    ensure!(
        state.record(seq)?.message.author_id == actor,
        "current_actor_required"
    );
    let anchor = state.record(seq)?.message.id.clone();
    let used = state
        .requests
        .values()
        .flat_map(|r| &r.bubbles)
        .filter(|b| !b.proactive && b.passive_message_id == anchor)
        .map(|b| b.msg_seq)
        .max()
        .unwrap_or(0);
    let request = state
        .requests
        .get_mut(&id)
        .ok_or_else(|| anyhow::anyhow!("request_missing"))?;
    ensure!(
        request.status == RequestStatus::AwaitingDelivery
            && request.generated_revision == request.revision
            && request.cancelled_revision.is_none(),
        "result_not_waiting"
    );
    ensure!(
        admin || request.owner_id == actor || request.contributor_ids.iter().any(|id| id == actor),
        "request_owner_required"
    );
    ensure!(
        !request.bubbles.iter().any(|b| matches!(
            b.status,
            DeliveryStatus::Sending | DeliveryStatus::Unknown
        ) && !b.origin.is_progress()),
        "delivery_requires_reconciliation"
    );
    let mut sequence = used;
    for bubble in request.bubbles.iter_mut().filter(|b| {
        b.revision == request.revision
            && b.status == DeliveryStatus::Pending
            && !b.origin.is_progress()
    }) {
        sequence += 1;
        bubble.passive_message_id = anchor.clone();
        bubble.msg_seq = if sequence <= 5 { sequence } else { 0 };
        bubble.proactive = false;
    }
    request.status = RequestStatus::Ready;
    Ok(())
}

pub(super) const POLICY: &str = "Task interaction: assess the actual request before execution. If task_interaction.enabled is false, task_update is unavailable and ordinary time and length limits apply. The following task_update guidance applies only when enabled. Ordinary chat or quick answers need no acknowledgement. Before complex research, multiple tool/search rounds, coding, or waiting for results, call clawmingle.task_update with kind ack and a short natural sentence explaining the next useful action. That call selects the configured long-task budget. When long_task_unlimited is true there is no total execution time limit; keep working until the requested outcome is complete, essential user input is missing, an unrecoverable failure occurs, or the user cancels. Do not prematurely stop because elapsed time is long; individual tools still have their own timeouts. You author the acknowledgement yourself; the host does not generate a template receipt. Keep task IDs and internal protocol states out of your conversational acknowledgement. Do not repeat an acknowledgement for an unfinished task. Even in quiet feedback mode, call task_update once for complex work to select its budget; the host controls visible delivery. In quiet mode omit subsequent milestone calls. Use a sequence of natural stage messages for long tasks: acknowledgement, useful progress, then a separate final result. Be aware of total elapsed time and time since the last delivered progress. At meaningful milestones report promptly; after a prolonged gap, give a short factual update about verified work, the current step or a real blocker rather than leaving the user guessing. Application tool results may include Host task timing with progress_due; use it as a reminder, never as evidence of completed work. Native tools may lack this hint: use time_now when needed. Never send repetitive waiting updates or reasoning/tool logs. queued is not confirmed QQ delivery. Progress never completes the request. To identify a task message in your update, supply quote_seq from current_request.messages; the host attaches the native QQ quote. Do not simulate a quote by writing a reply-prefix in text. Return outcome answer for the actual result; when essential missing user input prevents progress, return outcome needs_input with the minimal clear question. That closes this model turn but leaves the application request awaiting the user's response. Do not claim that merely asking a question completed the requested work. Use authorized tools and scope only. Later messages may revise the task through native steer. When a private response is associated with an earlier clarification, the latest user intent can revise or abandon the earlier requirements. Follow an explicit topic change rather than forcing the old task, and never repeat completed side effects merely to continue. A long task can outlive QQ's passive window; the application retains the result and may deliver it through already-authorized same-conversation push, otherwise the user can retrieve it with /continue. Do not claim delivery or promise a time you cannot support.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::social::{Bubble, ChatMessage, RequestRecord};
    fn state() -> GroupState {
        let mut state = GroupState::default();
        state
            .ingest(
                ChatMessage {
                    id: "source".into(),
                    author_id: "owner".into(),
                    timestamp_ms: 1000,
                    ..Default::default()
                },
                50,
            )
            .unwrap();
        state.requests.insert(
            1,
            RequestRecord {
                id: 1,
                owner_id: "owner".into(),
                source_seqs: vec![1],
                revision: 1,
                status: RequestStatus::Generating,
                ..Default::default()
            },
        );
        state
    }
    #[test]
    fn explicit_progress_quote_resolves_only_current_task_sources() {
        let mut state = state();
        state.messages[0].message.reference_idx = Some("qq-source-index".into());
        let config = SocialConfig::default();
        assert!(
            accept(
                &mut state,
                1,
                Update {
                    kind: Kind::Ack,
                    text: "核对路线".into(),
                    quote_seq: Some(99)
                },
                &config,
                2000,
                false
            )
            .is_err()
        );
        assert!(!state.requests[&1].long_task);
        assert!(state.requests[&1].bubbles.is_empty());
        let value = accept(
            &mut state,
            1,
            Update {
                kind: Kind::Ack,
                text: "核对路线".into(),
                quote_seq: Some(1),
            },
            &config,
            2000,
            false,
        )
        .unwrap();
        assert_eq!(value["queued"], true);
        let bubble = &state.requests[&1].bubbles[0];
        assert_eq!(bubble.reply_to_seq, Some(1));
        assert_eq!(
            bubble.reply_reference_idx.as_deref(),
            Some("qq-source-index")
        );
        assert_eq!(bubble.text, "核对路线");
        assert_eq!(state.requests[&1].quote_source_seq, None);
    }
    #[test]
    fn disabled_interactions_reject_updates_and_do_not_expand_reply_limits() {
        let mut state = state();
        let mut config = SocialConfig::default();
        config.interactions.enabled = false;
        assert!(
            accept(
                &mut state,
                1,
                Update {
                    quote_seq: None,
                    kind: Kind::Ack,
                    text: "开始整理".into()
                },
                &config,
                1100,
                false
            )
            .is_err()
        );
        assert!(!state.requests[&1].long_task);
        assert!(state.requests[&1].bubbles.is_empty());
        let effective = reply_config(&config, true);
        assert_eq!(effective.max_bubbles, config.max_bubbles);
        assert_eq!(effective.max_bubble_chars, config.max_bubble_chars);
    }

    #[test]
    fn zero_is_unlimited_total_time_without_unbounding_individual_tools() {
        let mut settings = InteractionSettings::default();
        assert_eq!(settings.long_timeout_seconds, 0);
        assert!(settings.validate().is_ok());
        for seconds in [1, 59, 3601] {
            settings.long_timeout_seconds = seconds;
            assert!(settings.validate().is_err());
        }
        settings.long_timeout_seconds = 0;
        settings.workspace_timeout_seconds = 0;
        assert!(settings.validate().is_err());
    }
    #[test]
    fn progress_selects_budget_without_completing_and_reserves_result_slots() {
        let mut state = state();
        let config = SocialConfig::default();
        assert_eq!(
            accept(
                &mut state,
                1,
                Update {
                    quote_seq: None,
                    kind: Kind::Ack,
                    text: "我先核实几处资料。".into()
                },
                &config,
                1100,
                false
            )
            .unwrap()["queued"],
            true
        );
        let request = &state.requests[&1];
        assert!(request.long_task);
        assert_eq!(request.generated_revision, 0);
        assert_eq!(request.delivered_revision, 0);
        assert_eq!(request.status, RequestStatus::Generating);
        assert_eq!(
            accept(
                &mut state,
                1,
                Update {
                    quote_seq: None,
                    kind: Kind::Ack,
                    text: "重复".into()
                },
                &config,
                1200,
                false
            )
            .unwrap()["queued"],
            false
        );
        assert_eq!(state.requests[&1].bubbles.len(), 1);
    }
    #[test]
    fn result_retrieval_requires_current_actor_and_preserves_generation() {
        let mut state = state();
        let request = state.requests.get_mut(&1).unwrap();
        request.status = RequestStatus::AwaitingDelivery;
        request.generated_revision = 1;
        request.bubbles.push(Bubble {
            origin: BubbleOrigin::ModelAnswer,
            revision: 1,
            text: "result".into(),
            ..Default::default()
        });
        state
            .ingest(
                ChatMessage {
                    id: "fresh".into(),
                    author_id: "other".into(),
                    ..Default::default()
                },
                50,
            )
            .unwrap();
        assert!(reanchor(&mut state, 1, 2, "other", false).is_err());
        state
            .ingest(
                ChatMessage {
                    id: "owner-fresh".into(),
                    author_id: "owner".into(),
                    ..Default::default()
                },
                50,
            )
            .unwrap();
        reanchor(&mut state, 1, 3, "owner", false).unwrap();
        assert_eq!(state.requests[&1].generated_revision, 1);
        assert_eq!(state.requests[&1].status, RequestStatus::Ready);
        assert_eq!(
            state.requests[&1].bubbles[0].passive_message_id,
            "owner-fresh"
        );
    }
    #[test]
    fn private_clarification_continues_only_one_recent_owned_question() {
        use crate::model::social::{Action, ParticipationMode, Quote};
        let mut state = state();
        let config = SocialConfig::default();
        {
            let r = state.requests.get_mut(&1).unwrap();
            r.status = RequestStatus::WaitingForUser;
            r.awaiting_input = true;
            r.bubbles.push(Bubble {
                origin: BubbleOrigin::Clarification,
                revision: 1,
                status: DeliveryStatus::Sent,
                sent_at_ms: Some(1000),
                reference_idx: Some("question-ref".into()),
                ..Default::default()
            });
        }
        let seq = state
            .ingest(
                ChatMessage {
                    id: "answer".into(),
                    author_id: "owner".into(),
                    text: "我用安卓".into(),
                    timestamp_ms: 2000,
                    ..Default::default()
                },
                50,
            )
            .unwrap()
            .unwrap();
        let base = || {
            super::super::state::rule_decision(
                ParticipationMode::All,
                &state.record(seq).unwrap().message,
            )
        };
        let decision = clarification_rule(&state, seq, base(), true, &config)
            .unwrap()
            .unwrap();
        assert_eq!(decision.action, Action::Append);
        assert_eq!(decision.request_id, Some(1));
        assert_eq!(
            clarification_rule(&state, seq, base(), false, &config)
                .unwrap()
                .unwrap()
                .action,
            Action::NewRequest
        );
        let mut other = state.requests[&1].clone();
        other.id = 2;
        state.requests.insert(2, other);
        let base = super::super::state::rule_decision(
            ParticipationMode::All,
            &state.record(seq).unwrap().message,
        );
        assert_eq!(
            clarification_rule(&state, seq, base, true, &config)
                .unwrap()
                .unwrap()
                .action,
            Action::NewRequest
        );
        state.requests.get_mut(&2).unwrap().bubbles[0].reference_idx = Some("other-ref".into());
        state
            .messages
            .iter_mut()
            .find(|m| m.seq == seq)
            .unwrap()
            .message
            .quotes
            .push(Quote {
                reference_idx: Some("question-ref".into()),
                ..Default::default()
            });
        let base = super::super::state::rule_decision(
            ParticipationMode::All,
            &state.record(seq).unwrap().message,
        );
        assert_eq!(
            clarification_rule(&state, seq, base, false, &config)
                .unwrap()
                .unwrap()
                .request_id,
            Some(1)
        );
    }
    #[test]
    fn explicit_clarification_command_resumes_same_request_without_closing_it() {
        let mut state = state();
        let config = SocialConfig::default();
        {
            let r = state.requests.get_mut(&1).unwrap();
            r.status = RequestStatus::WaitingForUser;
            r.awaiting_input = true;
        }
        let seq = state
            .ingest(
                ChatMessage {
                    id: "answer".into(),
                    author_id: "owner".into(),
                    text: "/continue R1 我用安卓".into(),
                    timestamp_ms: 2000,
                    ..Default::default()
                },
                50,
            )
            .unwrap()
            .unwrap();
        assert!(
            command(
                &mut state,
                seq,
                "/continue R1 我用安卓",
                &config,
                true,
                "owner",
                2000
            )
            .unwrap()
        );
        assert_eq!(state.requests[&1].revision, 2);
        assert_eq!(state.requests[&1].status, RequestStatus::Queued);
        assert!(!state.requests[&1].awaiting_input);
        assert_eq!(state.requests[&1].source_seqs, vec![1, 2]);
    }

    #[test]
    fn timestamp_continue_preserves_identity_and_cannot_take_another_users_result() {
        let mut state = state();
        state.next_request = 2;
        let config = SocialConfig::default();
        let code = "20261025-143205";
        let request = state.requests.get_mut(&1).unwrap();
        request.display_id = Some(code.into());
        request.status = RequestStatus::AwaitingDelivery;
        request.generated_revision = 1;
        request.bubbles.push(Bubble {
            origin: BubbleOrigin::ModelAnswer,
            revision: 1,
            text: "已保存结果".into(),
            ..Default::default()
        });
        for actor in ["other", "owner"] {
            let text = format!("/continue {code}");
            let seq = state
                .ingest(
                    ChatMessage {
                        id: format!("new-{actor}"),
                        author_id: actor.into(),
                        text: text.clone(),
                        timestamp_ms: 3000,
                        ..Default::default()
                    },
                    50,
                )
                .unwrap()
                .unwrap();
            command(&mut state, seq, &text, &config, false, "test-group", 3000).unwrap();
            if actor == "other" {
                assert_eq!(state.requests[&1].status, RequestStatus::AwaitingDelivery);
            }
        }
        assert_eq!(state.requests[&1].status, RequestStatus::Ready);
        assert_eq!(state.requests[&1].generated_revision, 1);
        assert_eq!(state.requests[&1].display_id.as_deref(), Some(code));
        assert_eq!(
            state.requests[&1].bubbles[0].passive_message_id,
            "new-owner"
        );
    }
    #[test]
    fn unknown_progress_does_not_block_retrieval_of_a_distinct_known_result() {
        let mut state = state();
        {
            let r = state.requests.get_mut(&1).unwrap();
            r.status = RequestStatus::AwaitingDelivery;
            r.generated_revision = 1;
            r.bubbles.push(Bubble {
                origin: BubbleOrigin::TaskAck,
                revision: 1,
                status: DeliveryStatus::Unknown,
                ..Default::default()
            });
            r.bubbles.push(Bubble {
                origin: BubbleOrigin::ModelAnswer,
                revision: 1,
                text: "ready result".into(),
                ..Default::default()
            });
        }
        let fresh = state
            .ingest(
                ChatMessage {
                    id: "retrieve".into(),
                    author_id: "owner".into(),
                    ..Default::default()
                },
                50,
            )
            .unwrap()
            .unwrap();
        reanchor(&mut state, 1, fresh, "owner", false).unwrap();
        assert_eq!(
            state.requests[&1].bubbles[0].status,
            DeliveryStatus::Unknown
        );
        assert_eq!(state.requests[&1].bubbles[1].passive_message_id, "retrieve");
        assert_eq!(state.requests[&1].status, RequestStatus::Ready);
    }
    #[test]
    fn completed_followup_resets_long_selection_but_unfinished_clarification_keeps_it() {
        use crate::model::social::{Action, Decision};
        for (status, expected) in [
            (RequestStatus::Completed, false),
            (RequestStatus::WaitingForUser, true),
            (RequestStatus::Generating, true),
        ] {
            let mut state = state();
            let r = state.requests.get_mut(&1).unwrap();
            r.status = status;
            r.long_task = true;
            let seq = state
                .ingest(
                    ChatMessage {
                        id: "follow".into(),
                        author_id: "owner".into(),
                        ..Default::default()
                    },
                    50,
                )
                .unwrap()
                .unwrap();
            state.begin_decision(seq).unwrap();
            state
                .apply_decision(
                    seq,
                    Decision {
                        action: Action::Append,
                        request_id: Some(1),
                        source: "jev".into(),
                        ..Default::default()
                    },
                    16,
                )
                .unwrap();
            assert_eq!(state.requests[&1].long_task, expected);
        }
    }
}

#[cfg(test)]
mod timing_tests {
    use super::*;
    use crate::model::social::{Bubble, RequestRecord, TaskFeedback};

    #[test]
    fn observing_time_does_not_select_long_work_or_queue_messages() {
        let mut state = GroupState::default();
        state.requests.insert(
            1,
            RequestRecord {
                id: 1,
                revision: 1,
                status: RequestStatus::Generating,
                feedback: Some(TaskFeedback {
                    revision: 1,
                    accepted_at_ms: 1000,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let before = serde_json::to_string(&state).unwrap();
        let view = accept(
            &mut state,
            1,
            Update {
                quote_seq: None,
                kind: Kind::Observe,
                text: String::new(),
            },
            &SocialConfig::default(),
            200000,
            false,
        )
        .unwrap();
        assert_eq!(view["progress_due"], false);
        assert_eq!(view["elapsed_seconds"], 199);
        assert_eq!(serde_json::to_string(&state).unwrap(), before);
    }
    #[test]
    fn disabled_interval_never_prompts_model_progress() {
        let r = RequestRecord {
            revision: 1,
            long_task: true,
            status: RequestStatus::Generating,
            feedback: Some(TaskFeedback {
                revision: 1,
                accepted_at_ms: 1000,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut config = SocialConfig::default();
        config.interactions.heartbeat_interval_seconds = 0;
        let view = task_timing(&r, &config, 3_601_000);
        assert_eq!(view["elapsed_seconds"], 3600);
        assert_eq!(view["progress_due"], false);
    }

    #[test]
    fn timing_counts_delivered_model_progress_and_waits_for_pending_updates() {
        let mut r = RequestRecord {
            id: 1,
            revision: 2,
            long_task: true,
            status: RequestStatus::Generating,
            feedback: Some(TaskFeedback {
                revision: 2,
                accepted_at_ms: 1000,
                ..Default::default()
            }),
            ..Default::default()
        };
        r.bubbles.push(Bubble {
            origin: BubbleOrigin::TaskAck,
            revision: 1,
            status: DeliveryStatus::Sent,
            sent_at_ms: Some(10000),
            ..Default::default()
        });
        r.bubbles.push(Bubble {
            origin: BubbleOrigin::TaskHeartbeat,
            revision: 2,
            status: DeliveryStatus::Sent,
            sent_at_ms: Some(199000),
            ..Default::default()
        });
        let config = SocialConfig::default();
        let view = task_timing(&r, &config, 200000);
        assert_eq!(view["since_last_visible_progress_seconds"], 190);
        assert_eq!(view["progress_due"], true);
        r.bubbles.push(Bubble {
            origin: BubbleOrigin::TaskProgress,
            revision: 2,
            status: DeliveryStatus::Pending,
            ..Default::default()
        });
        assert_eq!(task_timing(&r, &config, 200000)["progress_due"], false);
        r.bubbles.last_mut().unwrap().status = DeliveryStatus::Sent;
        r.bubbles.last_mut().unwrap().sent_at_ms = Some(190000);
        assert_eq!(
            task_timing(&r, &config, 200000)["since_last_visible_progress_seconds"],
            10
        );
        let mut quiet = config;
        quiet.interactions.feedback_mode = FeedbackMode::Quiet;
        assert_eq!(task_timing(&r, &quiet, 400000)["progress_due"], false);
    }
}
