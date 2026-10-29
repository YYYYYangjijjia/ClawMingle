//! Host feedback uses recorded events, never model silence as proof of progress.
use anyhow::Result;

use super::{config::SocialConfig, interaction::FeedbackMode};
use crate::model::social::{
    Bubble, BubbleOrigin, CommandButton, DeliveryStatus, GroupState, RequestRecord, RequestStatus,
    TaskFeedback,
};

pub(super) fn ensure(request: &mut RequestRecord, now: i64) -> &mut TaskFeedback {
    if request
        .feedback
        .as_ref()
        .is_none_or(|f| f.revision != request.revision)
    {
        let previous = request.feedback.take();
        request.feedback = Some(TaskFeedback {
            revision: request.revision,
            accepted_at_ms: previous.as_ref().map_or(now, |f| f.accepted_at_ms),
            // Revisions remain the same logical task. Old progress content is
            // intentionally not attributed to the newly accepted requirements.
            started_at_ms: previous.as_ref().and_then(|f| f.started_at_ms),
            last_update_attempt_ms: previous.and_then(|f| f.last_update_attempt_ms),
            ..Default::default()
        });
    }
    request.feedback.as_mut().expect("feedback initialized")
}

pub(super) fn started(request: &mut RequestRecord, now: i64) {
    let feedback = ensure(request, now);
    feedback.started_at_ms.get_or_insert(now);
}

/// Records activity only for a revision which this worker knows is executing.
pub(super) fn activity(request: &mut RequestRecord, revision: u64, now: i64) {
    if request.revision == revision
        && request.status == RequestStatus::Generating
        && request.cancelled_revision.is_none()
    {
        let feedback = ensure(request, now);
        feedback.started_at_ms.get_or_insert(now);
        feedback.last_activity_at_ms = Some(now);
    }
}

/// Queues an update; the worker still verifies push permission at send time.
/// No update changes execution budget, completion or final-delivery state here.
pub(super) fn queue(
    state: &mut GroupState,
    id: u64,
    origin: BubbleOrigin,
    text: String,
    config: &SocialConfig,
    now: i64,
    private: bool,
) -> Result<bool> {
    let request = &state.requests[&id];
    if !config.interactions.enabled
        || request.control
        || (origin == BubbleOrigin::TaskAck && !request.long_task)
        || request.cancelled_revision.is_some()
        || !matches!(
            request.status,
            RequestStatus::Queued | RequestStatus::Generating
        )
        || (origin != BubbleOrigin::TaskAck
            && config.interactions.feedback_mode == FeedbackMode::Quiet)
    {
        return Ok(false);
    }
    let revision = request.revision;
    let public_id = super::task_identity::label(request);
    let text = if request.display_id.is_some() && origin != BubbleOrigin::TaskHeartbeat {
        format!("{text}\n\n任务：{public_id}")
    } else {
        text
    };
    if origin == BubbleOrigin::TaskAck
        && let Some(bubble) = state
            .requests
            .get_mut(&id)
            .expect("request retained")
            .bubbles
            .iter_mut()
            .find(|bubble| {
                bubble.origin == BubbleOrigin::TaskAck
                    && bubble.revision == revision
                    && bubble.status == DeliveryStatus::Pending
                    && legacy_host_ack(id, &bubble.text)
            })
    {
        // Replace a pre-upgrade unsent host template with the model's actual
        // receipt without consuming another QQ message sequence.
        bubble.text = text;
        return Ok(true);
    }
    let request = &state.requests[&id];
    let anchor = state
        .record(*request.source_seqs.last().expect("task source"))?
        .message
        .clone();
    let progress: Vec<_> = request
        .bubbles
        .iter()
        .filter(|b| b.origin.is_progress() && b.revision > request.delivered_revision)
        .collect();
    if (origin == BubbleOrigin::TaskAck && progress.iter().any(|b| b.origin == origin))
        || progress.iter().any(|b| matches!(b.status, DeliveryStatus::Pending | DeliveryStatus::Sending))
        // A failed/uncertain update must not create a stream of retries. A new
        // user anchor permits new feedback, never retries the uncertain bubble.
        || progress.iter().any(|b| b.passive_message_id == anchor.id
            && matches!(b.status, DeliveryStatus::Failed | DeliveryStatus::Unknown | DeliveryStatus::Cancelled))
        || (origin != BubbleOrigin::TaskAck && request.feedback.as_ref()
            .and_then(|f| f.last_update_attempt_ms)
            .is_some_and(|at| now - at < config.interactions.progress_interval_seconds as i64 * 1000))
    {
        return Ok(false);
    }
    let used = state
        .requests
        .values()
        .flat_map(|r| &r.bubbles)
        .filter(|b| !b.proactive && b.passive_message_id == anchor.id)
        .map(|b| b.msg_seq)
        .max()
        .unwrap_or(0);
    let passive_progress = state
        .requests
        .values()
        .flat_map(|r| &r.bubbles)
        .filter(|b| {
            b.origin.is_progress()
                && !b.proactive
                && b.passive_message_id == anchor.id
                && b.msg_seq > 0
        })
        .count();
    let passive = used < super::runtime::passive_limit(private) - 2
        && passive_progress < config.interactions.max_progress_messages;
    let request = state.requests.get_mut(&id).expect("task retained");
    ensure(request, now).last_update_attempt_ms = Some(now);
    request.bubbles.push(Bubble {
        origin,
        revision: request.revision,
        text,
        passive_message_id: anchor.id,
        // Zero routes through the existing permission-checked push path.
        msg_seq: if passive { used + 1 } else { 0 },
        markdown: config.reply_format == "markdown",
        command_buttons: vec![CommandButton {
            label: "查看任务进度".into(),
            command: format!("/status {public_id}"),
        }],
        ..Default::default()
    });
    Ok(true)
}

fn legacy_host_ack(id: u64, text: &str) -> bool {
    text.starts_with(&format!("任务 R{id} 已收到，"))
        || text.starts_with(&format!("任务 R{id} 已开始处理，结果还没出来。"))
}

/// Recheck a queued acknowledgement at the actual send boundary. Model-written
/// receipts are useful only while that revision is still being worked on.
/// Legacy host-generated receipts may survive a restart; never publish them.
pub(super) fn prepare_send(state: &mut GroupState, id: u64, index: usize) -> bool {
    let request = state.requests.get_mut(&id).expect("request retained");
    let bubble = &mut request.bubbles[index];
    if bubble.origin != BubbleOrigin::TaskAck {
        return true;
    }
    let legacy = legacy_host_ack(id, &bubble.text);
    if legacy
        || !request.long_task
        || request.control
        || request.cancelled_revision.is_some()
        || bubble.revision != request.revision
        || !matches!(
            request.status,
            RequestStatus::Queued | RequestStatus::Generating
        )
    {
        bubble.status = DeliveryStatus::Cancelled;
        return false;
    }
    true
}

/// Returns whether durable state changed. `running` is this worker's live job,
/// not a persisted Generating flag left behind by a crash.
pub(super) fn tick(
    state: &mut GroupState,
    running: Option<(u64, u64)>,
    config: &SocialConfig,
    now: i64,
    private: bool,
) -> Result<bool> {
    let ids: Vec<_> = state
        .requests
        .values()
        .filter(|r| {
            !r.control
                && r.cancelled_revision.is_none()
                && matches!(r.status, RequestStatus::Queued | RequestStatus::Generating)
        })
        .map(|r| r.id)
        .collect();
    let mut changed = false;
    for id in ids {
        let request = state.requests.get_mut(&id).expect("task retained");
        changed |= request
            .feedback
            .as_ref()
            .is_none_or(|f| f.revision != request.revision);
        ensure(request, now);
        if !config.interactions.enabled {
            continue;
        }
        let request = &state.requests[&id];
        let feedback = request.feedback.as_ref().expect("feedback initialized");
        let interval = config.interactions.heartbeat_interval_seconds as i64 * 1000;
        if !request.long_task
            || config.interactions.feedback_mode == FeedbackMode::Quiet
            || interval == 0
            || running != Some((id, request.revision))
            || request.status != RequestStatus::Generating
        {
            continue;
        }
        let last_notice = feedback
            .last_update_attempt_ms
            .unwrap_or(feedback.accepted_at_ms)
            .max(feedback.last_progress_at_ms.unwrap_or(0));
        if now - last_notice < interval {
            continue;
        }
        let activity = feedback
            .last_activity_at_ms
            .or(feedback.started_at_ms)
            .unwrap_or(feedback.accepted_at_ms);
        // No repeated 'still waiting' messages without new execution evidence.
        if feedback
            .stale_notice_after_ms
            .is_some_and(|at| activity <= at)
        {
            continue;
        }
        let fresh = feedback
            .last_activity_at_ms
            .is_some_and(|at| now - at < interval);
        let public_id = super::task_identity::label(request);
        let text = if fresh {
            format!("任务 {public_id} 还没完成，最近仍收到执行活动。最终结果出来后会再回复你。")
        } else {
            format!(
                "任务 {public_id} 还没有最终结果，暂未收到新的执行反馈。我现在无法确认是否仍在推进，可查看任务状态。"
            )
        };
        if queue(
            state,
            id,
            BubbleOrigin::TaskHeartbeat,
            text,
            config,
            now,
            private,
        )? {
            state
                .requests
                .get_mut(&id)
                .expect("task retained")
                .feedback
                .as_mut()
                .expect("feedback initialized")
                .stale_notice_after_ms = if fresh { None } else { Some(activity) };
            changed = true;
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::social::ChatMessage;

    fn state() -> GroupState {
        let mut state = GroupState::default();
        state
            .ingest(
                ChatMessage {
                    id: "message".into(),
                    author_id: "member".into(),
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
                revision: 1,
                source_seqs: vec![1],
                status: RequestStatus::Generating,
                ..Default::default()
            },
        );
        started(state.requests.get_mut(&1).unwrap(), 1000);
        state
    }
    fn model_ack(state: &mut GroupState, config: &SocialConfig) {
        super::super::interaction::accept(
            state,
            1,
            super::super::interaction::Update {
                quote_seq: None,
                kind: super::super::interaction::Kind::Ack,
                text: "我先核对游客办理条件，再比较能保号的低价套餐。".into(),
            },
            config,
            6000,
            false,
        )
        .unwrap();
    }
    fn sent(state: &mut GroupState, now: i64) {
        let bubble = state
            .requests
            .get_mut(&1)
            .unwrap()
            .bubbles
            .last_mut()
            .unwrap();
        bubble.status = DeliveryStatus::Sent;
        bubble.sent_at_ms = Some(now);
    }
    #[test]
    fn time_alone_never_classifies_work_or_creates_an_ack() {
        let config = SocialConfig::default();
        let mut state = state();
        for now in [6000, 31000, 96000, 300000] {
            tick(&mut state, Some((1, 1)), &config, now, false).unwrap();
            assert!(state.requests[&1].bubbles.is_empty());
            assert!(!state.requests[&1].long_task);
        }
        state.requests.get_mut(&1).unwrap().status = RequestStatus::Queued;
        tick(&mut state, None, &config, 400000, false).unwrap();
        assert!(state.requests[&1].bubbles.is_empty());
    }
    #[test]
    fn model_selects_long_work_and_authors_the_only_ack() {
        let config = SocialConfig::default();
        let mut state = state();
        model_ack(&mut state, &config);
        model_ack(&mut state, &config);
        let request = &state.requests[&1];
        assert!(request.long_task);
        assert_eq!(request.bubbles.len(), 1);
        assert!(request.bubbles[0].text.starts_with("我先核对"));
        assert_eq!(request.generated_revision, 0);
        assert_eq!(request.delivered_revision, 0);
        assert!(prepare_send(&mut state, 1, 0));
    }
    #[test]
    fn completed_work_light_followup_does_not_repeat_a_receipt() {
        let config = SocialConfig::default();
        let mut state = state();
        model_ack(&mut state, &config);
        sent(&mut state, 6000);
        let r = state.requests.get_mut(&1).unwrap();
        r.delivered_revision = 1;
        r.revision = 2;
        r.long_task = false;
        r.feedback = None;
        started(r, 100000);
        tick(&mut state, Some((1, 2)), &config, 300000, false).unwrap();
        assert_eq!(state.requests[&1].bubbles.len(), 1);
    }
    #[test]
    fn no_activity_notice_is_once_and_real_events_restore_feedback() {
        let config = SocialConfig::default();
        let mut state = state();
        model_ack(&mut state, &config);
        sent(&mut state, 6000);
        assert!(tick(&mut state, Some((1, 1)), &config, 96000, false).unwrap());
        assert!(state.requests[&1].bubbles[1].text.contains("无法确认"));
        sent(&mut state, 96000);
        assert!(!tick(&mut state, Some((1, 1)), &config, 186000, false).unwrap());
        activity(state.requests.get_mut(&1).unwrap(), 1, 185000);
        assert!(tick(&mut state, Some((1, 1)), &config, 186000, false).unwrap());
        assert!(state.requests[&1].bubbles[2].text.contains("收到执行活动"));
        assert_eq!(state.requests[&1].bubbles[2].msg_seq, 0);
    }
    #[test]
    fn quiet_finished_or_missing_live_job_never_gets_a_heartbeat() {
        let mut config = SocialConfig::default();
        let mut base = state();
        model_ack(&mut base, &config);
        sent(&mut base, 6000);
        for status in [
            RequestStatus::Completed,
            RequestStatus::Cancelled,
            RequestStatus::Failed,
            RequestStatus::WaitingForUser,
            RequestStatus::AwaitingDelivery,
            RequestStatus::UnknownDelivery,
        ] {
            let mut state = base.clone();
            state.requests.get_mut(&1).unwrap().status = status;
            assert!(!tick(&mut state, Some((1, 1)), &config, 200000, false).unwrap());
        }
        assert!(!tick(&mut base, None, &config, 200000, false).unwrap());
        config.interactions.feedback_mode = FeedbackMode::Quiet;
        assert!(!tick(&mut base, Some((1, 1)), &config, 200000, false).unwrap());
    }
    #[test]
    fn failed_or_unknown_updates_are_not_retried() {
        let config = SocialConfig::default();
        for status in [
            DeliveryStatus::Failed,
            DeliveryStatus::Unknown,
            DeliveryStatus::Cancelled,
        ] {
            let mut state = state();
            model_ack(&mut state, &config);
            state.requests.get_mut(&1).unwrap().bubbles[0].status = status;
            assert!(!tick(&mut state, Some((1, 1)), &config, 200000, false).unwrap());
            assert_eq!(state.requests[&1].bubbles.len(), 1);
        }
    }
    #[test]
    fn send_boundary_skips_stale_and_legacy_host_receipts() {
        let config = SocialConfig::default();
        let mut base = state();
        model_ack(&mut base, &config);
        for status in [
            RequestStatus::Ready,
            RequestStatus::Completed,
            RequestStatus::Cancelled,
            RequestStatus::Failed,
        ] {
            let mut state = base.clone();
            state.requests.get_mut(&1).unwrap().status = status;
            assert!(!prepare_send(&mut state, 1, 0));
            assert_eq!(
                state.requests[&1].bubbles[0].status,
                DeliveryStatus::Cancelled
            );
        }
        let r = base.requests.get_mut(&1).unwrap();
        r.bubbles[0].text = "任务 R1 已收到，正在准备执行；还没收到模型开始处理的确认。".into();
        assert!(!prepare_send(&mut base, 1, 0));
    }
    #[test]
    fn model_receipt_replaces_a_legacy_pending_template_without_a_second_send() {
        let config = SocialConfig::default();
        let mut state = state();
        model_ack(&mut state, &config);
        state.requests.get_mut(&1).unwrap().bubbles[0].text =
            "任务 R1 已收到，正在排队；还没有开始生成回复。".into();
        model_ack(&mut state, &config);
        assert_eq!(state.requests[&1].bubbles.len(), 1);
        assert!(state.requests[&1].bubbles[0].text.starts_with("我先核对"));
        assert!(prepare_send(&mut state, 1, 0));
    }
    #[test]
    fn active_revision_does_not_repeat_ack_or_use_old_activity() {
        let config = SocialConfig::default();
        let mut state = state();
        model_ack(&mut state, &config);
        sent(&mut state, 6000);
        let r = state.requests.get_mut(&1).unwrap();
        r.revision = 2;
        ensure(r, 7000);
        activity(r, 1, 8000);
        assert!(r.feedback.as_ref().unwrap().last_activity_at_ms.is_none());
        activity(r, 2, 9000);
        assert_eq!(r.feedback.as_ref().unwrap().last_activity_at_ms, Some(9000));
        model_ack(&mut state, &config);
        assert_eq!(state.requests[&1].bubbles.len(), 1);
    }
}
