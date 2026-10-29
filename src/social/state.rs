use std::collections::BTreeMap;

use anyhow::{Result, bail, ensure};

use crate::model::social::{
    Action, Bubble, ChatMessage, Decision, DecisionStatus, DeliveryStatus, GroupState,
    MessageRecord, ParticipationMode, RequestRecord, RequestStatus,
};

pub fn rule_decision(mode: ParticipationMode, message: &ChatMessage) -> Option<Decision> {
    let (action, source) = if message.at_bot {
        (Action::NewRequest, "mention")
    } else {
        match mode {
            ParticipationMode::Mentions => (Action::Silent, "mode-1"),
            ParticipationMode::All => (Action::NewRequest, "mode-all"),
            _ => return None,
        }
    };
    Some(Decision {
        action,
        source: source.into(),
        audience_id: Some(message.author_id.clone()),
        style: "answer".into(),
        policy_version: 1,
        ..Default::default()
    })
}

/// Mentions guarantee an opportunity to answer, not a new task per fragment.
/// Middle modes can associate a mention with existing work through Jev.
pub(super) fn routing_rule(
    state: &GroupState,
    mode: ParticipationMode,
    message: &ChatMessage,
) -> Option<Decision> {
    if message.at_bot
        && matches!(mode, ParticipationMode::Helpful | ParticipationMode::Social)
        && state.requests.values().any(|r| {
            !r.control
                && r.cancelled_revision.is_none()
                && !matches!(
                    r.status,
                    RequestStatus::Completed | RequestStatus::Cancelled | RequestStatus::Failed
                )
                && r.source_seqs
                    .iter()
                    .any(|seq| *seq >= state.context_start_seq)
        })
    {
        None
    } else {
        rule_decision(mode, message)
    }
}

impl GroupState {
    /// Returns None for a replay. Stronger late @ evidence reopens a silent decision.
    pub fn ingest(&mut self, message: ChatMessage, capacity: usize) -> Result<Option<u64>> {
        ensure!(
            !message.id.is_empty() && !message.author_id.is_empty(),
            "invalid_message_identity"
        );
        ensure!(message.text.len() <= 65_536, "message_too_large");
        if let Some(record) = self
            .messages
            .iter_mut()
            .find(|r| r.message.id == message.id)
        {
            if message.at_bot && !record.message.at_bot {
                record.message.at_bot = true;
                if record.applied_request.is_none()
                    && !record
                        .decision
                        .as_ref()
                        .is_some_and(|d| d.source == "command")
                {
                    record.status = DecisionStatus::Pending;
                    record.decision = None;
                    record.application = None;
                    self.decided_through_seq =
                        self.decided_through_seq.min(record.seq.saturating_sub(1));
                    return Ok(Some(record.seq));
                }
            }
            return Ok(None);
        }
        ensure!(self.messages.len() < capacity, "message_capacity_reached");
        let seq = self.next_seq;
        self.next_seq += 1;
        self.messages.push(MessageRecord {
            seq,
            message,
            jev_attempts: Some(0),
            ..Default::default()
        });
        Ok(Some(seq))
    }

    pub fn record(&self, seq: u64) -> Result<&MessageRecord> {
        self.messages
            .iter()
            .find(|r| r.seq == seq)
            .ok_or_else(|| anyhow::anyhow!("message_missing"))
    }

    pub fn next_pending(&self, through: u64) -> Option<u64> {
        self.messages
            .iter()
            .find(|r| r.seq <= through && r.status == DecisionStatus::Pending)
            .map(|r| r.seq)
    }

    pub fn has_pending(&self) -> bool {
        self.messages
            .iter()
            .any(|r| matches!(r.status, DecisionStatus::Pending | DecisionStatus::Running))
    }

    /// Preserve the already-classified independent intent. Historical messages
    /// must not be replayed through Jev's current-message time window.
    pub fn admit_overflow(&mut self, max_active: usize) -> Result<Option<u64>> {
        let Some(record) = self
            .messages
            .iter()
            .filter(|m| m.application.as_deref() == Some("request_queue_full"))
            .min_by_key(|m| {
                (
                    m.decision.as_ref().is_some_and(|d| d.style == "chat")
                        && !super::traffic::directed_to_bot(m),
                    m.seq,
                )
            })
        else {
            return Ok(None);
        };
        let decision = record
            .decision
            .clone()
            .ok_or_else(|| anyhow::anyhow!("overflow_decision_missing"))?;
        ensure!(
            decision.action == Action::NewRequest,
            "overflow_intent_not_independent"
        );
        let seq = record.seq;
        let explicit = decision.style != "chat" || super::traffic::directed_to_bot(record);
        if !self.request_slot(max_active, explicit) {
            return Ok(None);
        }
        let record = self
            .messages
            .iter_mut()
            .find(|m| m.seq == seq)
            .expect("overflow retained");
        record.status = DecisionStatus::Running;
        self.apply_decision(seq, decision, max_active)?;
        Ok(Some(seq))
    }

    fn request_slot(&mut self, max_active: usize, explicit: bool) -> bool {
        let active = self
            .requests
            .values()
            .filter(|r| {
                !r.control
                    && !matches!(
                        r.status,
                        RequestStatus::Completed | RequestStatus::Cancelled | RequestStatus::Failed
                    )
            })
            .count();
        if active < max_active {
            return true;
        }
        if explicit
            && let Some(id) = self
                .requests
                .values()
                .find(|r| super::traffic::withdrawable_chat(self, r))
                .map(|r| r.id)
        {
            let request = self
                .requests
                .get_mut(&id)
                .expect("optional request retained");
            request.status = RequestStatus::Cancelled;
            request.cancelled_revision = Some(request.revision);
            request.failure_code = Some("optional_reply_yielded_to_explicit".into());
            return active == max_active;
        }
        false
    }

    pub fn begin_decision(&mut self, seq: u64) -> Result<()> {
        let record = self
            .messages
            .iter_mut()
            .find(|r| r.seq == seq)
            .ok_or_else(|| anyhow::anyhow!("message_missing"))?;
        ensure!(
            record.status == DecisionStatus::Pending,
            "decision_not_pending"
        );
        record.status = DecisionStatus::Running;
        record.attempts += 1;
        Ok(())
    }

    pub fn fail_decision(&mut self, seq: u64) {
        if let Some(record) = self.messages.iter_mut().find(|r| r.seq == seq) {
            record.status = DecisionStatus::Failed;
            record.application = Some("decision_failed".into());
        }
    }

    pub fn apply_decision(
        &mut self,
        seq: u64,
        mut decision: Decision,
        max_active: usize,
    ) -> Result<()> {
        let record = self.record(seq)?;
        // A late @ event may supersede an in-flight semantic decision.
        if record.status != DecisionStatus::Running {
            return Ok(());
        }
        let message = record.message.clone();
        // A semantic target can become stale while Jev runs. A real mention
        // still deserves an independent answer; it cannot mutate that target.
        if message.at_bot
            && decision.source == "jev"
            && matches!(decision.action, Action::Append | Action::Revise)
            && decision
                .request_id
                .and_then(|id| self.requests.get(&id))
                .is_none_or(|r| {
                    r.cancelled_revision.is_some()
                        || matches!(
                            r.status,
                            RequestStatus::Cancelled | RequestStatus::UnknownDelivery
                        )
                        || (decision.action == Action::Revise && r.owner_id != message.author_id)
                        || decision
                            .request_revisions
                            .get(&r.id)
                            .is_some_and(|revision| *revision != r.revision)
                })
        {
            decision.action = Action::NewRequest;
            decision.request_id = None;
        }
        let bypassed = decision.source != "jev";
        let mut applied = None;
        let mut application = "silent";
        match decision.action {
            Action::Silent => {}
            Action::NewRequest => {
                let explicit = decision.style != "chat"
                    || message.at_bot
                    || ["exchange", "you_referent"].iter().any(|key| {
                        decision
                            .judgments
                            .get(*key)
                            .is_some_and(|j| j.choice == "bot")
                    });
                if !self.request_slot(max_active, explicit) {
                    application = "request_queue_full";
                } else {
                    let id = self.next_request;
                    self.next_request += 1;
                    let display_id = super::task_identity::allocate(self, id, message.timestamp_ms);
                    self.requests.insert(
                        id,
                        RequestRecord {
                            id,
                            display_id,
                            owner_id: message.author_id.clone(),
                            contributor_ids: vec![message.author_id.clone()],
                            audience_id: decision.audience_id.clone(),
                            source_seqs: vec![seq],
                            revision: 1,
                            updated_at_ms: message.timestamp_ms,
                            style: decision.style.clone(),
                            ..Default::default()
                        },
                    );
                    applied = Some(id);
                    application = "created";
                }
            }
            Action::Append | Action::Revise | Action::Cancel => {
                if let Some(request) = decision
                    .request_id
                    .and_then(|id| self.requests.get_mut(&id))
                {
                    if request.status == RequestStatus::Cancelled
                        || request.cancelled_revision.is_some()
                    {
                        application = "request_cancelled";
                    } else if request.status == RequestStatus::UnknownDelivery {
                        application = "delivery_requires_reconciliation";
                    } else if matches!(decision.action, Action::Revise | Action::Cancel)
                        && request.owner_id != message.author_id
                    {
                        application = "owner_required";
                    } else if decision
                        .request_revisions
                        .get(&request.id)
                        .is_some_and(|revision| *revision != request.revision)
                    {
                        application = "stale_decision";
                    } else {
                        // A follow-up to completed work starts with ordinary limits.
                        // Active revisions and clarification continuations retain the
                        // already-selected budget for the unfinished logical task.
                        if request.status == RequestStatus::Completed {
                            request.long_task = false;
                            request.feedback = None;
                        }
                        request.revision += 1;
                        request.failure_code = None;
                        request.awaiting_input = false;
                        request.quote_source_seq = None;
                        request.suppress_auto_quote = false;
                        request.updated_at_ms = message.timestamp_ms;
                        request.source_seqs.push(seq);
                        if decision.action != Action::Cancel {
                            request.audience_id = decision.audience_id.clone();
                            request.style = decision.style.clone();
                        } else {
                            request.cancelled_revision = Some(request.revision);
                        }
                        if !request.contributor_ids.contains(&message.author_id) {
                            request.contributor_ids.push(message.author_id.clone());
                        }
                        applied = Some(request.id);
                        for bubble in &mut request.bubbles {
                            if bubble.status == DeliveryStatus::Pending {
                                bubble.status = DeliveryStatus::Cancelled;
                            }
                        }
                        request.status = if decision.action == Action::Cancel {
                            RequestStatus::Cancelled
                        } else {
                            RequestStatus::Queued
                        };
                        application = if decision.action == Action::Cancel {
                            "cancelled"
                        } else {
                            "updated"
                        };
                    }
                } else {
                    application = "request_missing";
                }
            }
        }
        let applied_revision = applied
            .and_then(|id| self.requests.get(&id))
            .map(|request| request.revision);
        let record = self
            .messages
            .iter_mut()
            .find(|r| r.seq == seq)
            .expect("message retained");
        record.status = if bypassed {
            DecisionStatus::Bypassed
        } else {
            DecisionStatus::Decided
        };
        record.decision = Some(decision);
        record.applied_request = applied;
        record.applied_revision = applied_revision;
        record.application = Some(application.into());
        self.advance_cursor();
        Ok(())
    }

    fn advance_cursor(&mut self) {
        for record in &self.messages {
            if record.seq > self.decided_through_seq {
                if !record.status.is_terminal() {
                    break;
                }
                self.decided_through_seq = record.seq;
            }
        }
    }

    pub fn begin_generation(&mut self, id: u64) -> Result<u64> {
        let request = self
            .requests
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("request_missing"))?;
        ensure!(
            request.status == RequestStatus::Queued,
            "request_not_queued"
        );
        request.status = RequestStatus::Generating;
        request.submitted_revision = request.revision;
        Ok(request.revision)
    }

    pub fn finish_generation(
        &mut self,
        id: u64,
        revision: u64,
        texts: Vec<String>,
    ) -> Result<bool> {
        self.finish_generation_with_images(id, revision, texts, Vec::new())
    }

    pub fn finish_generation_with_images(
        &mut self,
        id: u64,
        revision: u64,
        texts: Vec<String>,
        images: Vec<crate::model::social::OutputImage>,
    ) -> Result<bool> {
        self.finish_generation_with_media(id, revision, texts, images, Vec::new())
    }

    pub fn finish_generation_with_media(
        &mut self,
        id: u64,
        revision: u64,
        texts: Vec<String>,
        images: Vec<crate::model::social::OutputImage>,
        files: Vec<crate::model::social::OutputFile>,
    ) -> Result<bool> {
        let count = texts.len() + images.len() + files.len();
        ensure!(count > 0, "empty_reply");
        let request = self
            .requests
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("request_missing"))?;
        if request.revision != revision || request.status != RequestStatus::Generating {
            return Ok(false);
        }
        let source = *request
            .source_seqs
            .last()
            .ok_or_else(|| anyhow::anyhow!("request_has_no_source"))?;
        let passive_message_id = self.record(source)?.message.id.clone();
        // A new revision uses the latest contributing message as its passive anchor.
        let used = self
            .requests
            .values()
            .flat_map(|r| &r.bubbles)
            .filter(|b| !b.proactive && b.passive_message_id == passive_message_id)
            .map(|b| b.msg_seq)
            .max()
            .unwrap_or(0);
        // Beyond the passive quota, retain the result for authorized proactive
        // delivery or explicit fresh-anchor retrieval. Never discard completed work.
        let request = self.requests.get_mut(&id).expect("request retained");
        for bubble in &mut request.bubbles {
            if bubble.origin.is_progress() && bubble.status == DeliveryStatus::Pending {
                bubble.status = DeliveryStatus::Cancelled;
            }
        }
        request.accepted_revision = revision;
        request.generated_revision = revision;
        request.bubbles.extend(
            texts
                .into_iter()
                .map(|text| (text, None, None))
                .chain(
                    images
                        .into_iter()
                        .map(|image| (String::new(), Some(image), None)),
                )
                .chain(
                    files
                        .into_iter()
                        .map(|file| (String::new(), None, Some(file))),
                )
                .enumerate()
                .map(|(index, (text, image, file))| Bubble {
                    origin: crate::model::social::BubbleOrigin::ModelAnswer,
                    revision,
                    text,
                    image,
                    file,
                    passive_message_id: passive_message_id.clone(),
                    msg_seq: if used + (index as u32) < 5 {
                        used + index as u32 + 1
                    } else {
                        0
                    },
                    ..Default::default()
                }),
        );
        request.status = RequestStatus::Ready;
        Ok(true)
    }

    /// Queue one application-authored failure notice without advancing model progress.
    /// Returning true means the failure was recorded, including an exhausted send budget.
    pub fn fail_generation_with_notice(
        &mut self,
        id: u64,
        revision: u64,
        code: &str,
        notice: &str,
    ) -> Result<bool> {
        let request = self
            .requests
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("request_missing"))?;
        if request.revision != revision
            || request.status != RequestStatus::Generating
            || request.cancelled_revision.is_some()
        {
            return Ok(false);
        }
        ensure!(
            !code.is_empty()
                && code.len() <= 64
                && code
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_'),
            "invalid_failure_code"
        );
        ensure!(
            !notice.trim().is_empty() && notice.chars().count() <= 1000,
            "invalid_failure_notice"
        );
        let source = *request
            .source_seqs
            .last()
            .ok_or_else(|| anyhow::anyhow!("request_has_no_source"))?;
        let passive_message_id = self.record(source)?.message.id.clone();
        let used = self
            .requests
            .values()
            .flat_map(|r| &r.bubbles)
            .filter(|b| !b.proactive && b.passive_message_id == passive_message_id)
            .map(|b| b.msg_seq)
            .max()
            .unwrap_or(0);
        let request = self.requests.get_mut(&id).expect("request retained");
        request.failure_code = Some(code.into());
        for bubble in &mut request.bubbles {
            if bubble.origin.is_progress() && bubble.status == DeliveryStatus::Pending {
                bubble.status = DeliveryStatus::Cancelled;
            }
        }
        if used >= 5 {
            request.status = RequestStatus::Failed;
            return Ok(true);
        }
        request.bubbles.push(Bubble {
            origin: crate::model::social::BubbleOrigin::FailureNotice,
            revision,
            text: notice.into(),
            passive_message_id,
            msg_seq: used + 1,
            ..Default::default()
        });
        request.status = RequestStatus::Ready;
        Ok(true)
    }

    pub fn recover(&mut self) {
        for record in &mut self.messages {
            if record.status == DecisionStatus::Running {
                record.status = DecisionStatus::Pending;
            }
        }
        for request in self.requests.values_mut() {
            for bubble in &mut request.bubbles {
                if bubble.status == DeliveryStatus::Sending {
                    bubble.status = DeliveryStatus::Unknown;
                } else if bubble.origin.is_progress() && bubble.status == DeliveryStatus::Pending {
                    bubble.status = DeliveryStatus::Cancelled;
                }
            }
            if request
                .bubbles
                .iter()
                .any(|b| b.status == DeliveryStatus::Unknown && !b.origin.is_progress())
            {
                request.status = RequestStatus::UnknownDelivery;
            } else if request.cancelled_revision.is_some() {
                request.status = RequestStatus::Cancelled;
            } else if request.status == RequestStatus::Generating {
                // A crash may leave a live/unknown Codex turn. Require an explicit retry.
                request.status = RequestStatus::Failed;
            } else if request.status == RequestStatus::Sending {
                request.status = RequestStatus::Ready;
            }
        }
    }

    pub fn resolve(&mut self, id: u64, index: usize, was_sent: bool) -> Result<()> {
        let request = self
            .requests
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("request_missing"))?;
        let bubble = request
            .bubbles
            .get_mut(index)
            .ok_or_else(|| anyhow::anyhow!("bubble_missing"))?;
        ensure!(
            bubble.status == DeliveryStatus::Unknown,
            "bubble_is_not_unknown"
        );
        if bubble.origin.is_progress() && request.status != RequestStatus::UnknownDelivery {
            bubble.status = if was_sent {
                DeliveryStatus::Sent
            } else {
                DeliveryStatus::Cancelled
            };
            return Ok(());
        }
        bubble.status = if was_sent {
            DeliveryStatus::Sent
        } else if bubble.revision != request.revision || request.cancelled_revision.is_some() {
            DeliveryStatus::Cancelled
        } else {
            DeliveryStatus::Pending
        };
        request.status = if request
            .bubbles
            .iter()
            .any(|b| b.status == DeliveryStatus::Unknown && !b.origin.is_progress())
        {
            RequestStatus::UnknownDelivery
        } else if request.cancelled_revision.is_some() {
            RequestStatus::Cancelled
        } else if request.failure_code.is_some() {
            // Reconciliation may resume the program notice, never the failed model turn.
            RequestStatus::Ready
        } else if request.generated_revision != request.revision {
            RequestStatus::Queued
        } else {
            RequestStatus::Ready
        };
        Ok(())
    }

    pub fn prune(&mut self, now: i64, retention_ms: i64, max_messages: usize) {
        let mut needed = self
            .requests
            .values()
            .filter(|r| {
                !matches!(
                    r.status,
                    RequestStatus::Completed | RequestStatus::Cancelled | RequestStatus::Failed
                )
            })
            .flat_map(|r| r.source_seqs.iter().copied())
            .collect::<Vec<_>>();
        needed.extend(
            self.messages
                .iter()
                .filter(|m| m.application.as_deref() == Some("request_queue_full"))
                .map(|m| m.seq),
        );
        // /continue anchors are delivery context, not new model requirements.
        // Keep them until pending deliveries no longer rely on their QQ window.
        for record in &self.messages {
            if self.requests.values().any(|request| {
                request.bubbles.iter().any(|bubble| {
                    !bubble.proactive
                        && !bubble.origin.is_progress()
                        && bubble.passive_message_id == record.message.id
                        && matches!(
                            bubble.status,
                            DeliveryStatus::Pending
                                | DeliveryStatus::Sending
                                | DeliveryStatus::Unknown
                        )
                })
            }) {
                needed.push(record.seq);
            }
        }
        // An unfinished source may be anywhere in the rolling window. Keep it
        // without pinning unrelated, settled messages behind it. Sequence IDs
        // and archive cursors are stable even when the window has gaps.
        let mut remaining = self.messages.len();
        self.messages.retain(|r| {
            let remove = r.status.is_terminal()
                && !needed.contains(&r.seq)
                && (r.message.timestamp_ms < now.saturating_sub(retention_ms)
                    || remaining >= max_messages);
            if remove {
                remaining -= 1;
            }
            !remove
        });
        let retained = self.messages.iter().map(|r| r.seq).collect::<Vec<_>>();
        self.requests.retain(|_, r| {
            !matches!(
                r.status,
                RequestStatus::Completed | RequestStatus::Cancelled | RequestStatus::Failed
            ) || r.updated_at_ms >= now - retention_ms
                && r.source_seqs.iter().all(|seq| retained.contains(seq))
        });
    }
}

/// Counted quiet-time collection only: this never establishes semantic relations.
#[derive(Default)]
pub struct CollectionWindow {
    first_ms: Option<i64>,
    last_ms: Option<i64>,
}

impl CollectionWindow {
    #[cfg(test)]
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    pub fn observe(&mut self, now: i64) {
        self.first_ms.get_or_insert(now);
        self.last_ms = Some(now);
    }
    pub fn due(&self, now: i64, quiet: i64, maximum: i64) -> bool {
        self.first_ms.is_some_and(|first| {
            now - first >= maximum || self.last_ms.is_some_and(|last| now - last >= quiet)
        })
    }
}

pub fn validate_bubbles(texts: &[String], maximum: usize, max_chars: usize) -> Result<()> {
    ensure!(
        !texts.is_empty() && texts.len() <= maximum,
        "invalid_bubble_count"
    );
    for text in texts {
        if text.trim().is_empty() || text.chars().count() > max_chars {
            bail!("invalid_bubble_text");
        }
    }
    Ok(())
}

pub fn request_revisions(state: &GroupState) -> BTreeMap<u64, u64> {
    state
        .requests
        .values()
        .map(|r| (r.id, r.revision))
        .collect()
}

/// Stable pseudonyms also across turns and after the rolling window changes.
pub fn participant_alias(id: &str) -> String {
    use sha1::{Digest, Sha1};
    format!("U{}", &format!("{:x}", Sha1::digest(id.as_bytes()))[..12])
}

#[cfg(test)]
mod failure_notice_tests {
    use super::*;

    fn generating() -> GroupState {
        let mut state = GroupState::default();
        let seq = state
            .ingest(
                ChatMessage {
                    id: "source-message".into(),
                    author_id: "member".into(),
                    text: "请处理图片".into(),
                    ..Default::default()
                },
                20,
            )
            .unwrap()
            .unwrap();
        state.requests.insert(
            1,
            RequestRecord {
                id: 1,
                owner_id: "member".into(),
                source_seqs: vec![seq],
                revision: 3,
                submitted_revision: 3,
                accepted_revision: 2,
                generated_revision: 2,
                delivered_revision: 1,
                status: RequestStatus::Generating,
                ..Default::default()
            },
        );
        state
    }

    #[test]
    fn failure_notice_uses_shared_passive_quota_without_advancing_model_progress() {
        let mut state = generating();
        state.requests.insert(
            2,
            RequestRecord {
                id: 2,
                bubbles: vec![Bubble {
                    passive_message_id: "source-message".into(),
                    msg_seq: 2,
                    status: DeliveryStatus::Sent,
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        assert!(
            state
                .fail_generation_with_notice(
                    1,
                    3,
                    "image_input_failed",
                    "图片未能读取，请重新发送。"
                )
                .unwrap()
        );
        let request = &state.requests[&1];
        assert_eq!(request.failure_code.as_deref(), Some("image_input_failed"));
        assert_eq!(request.status, RequestStatus::Ready);
        assert_eq!(
            (
                request.submitted_revision,
                request.accepted_revision,
                request.generated_revision,
                request.delivered_revision
            ),
            (3, 2, 2, 1)
        );
        let notice = &request.bubbles[0];
        assert_eq!(notice.revision, 3);
        assert_eq!(notice.msg_seq, 3);
        assert_eq!(notice.status, DeliveryStatus::Pending);
        assert_eq!(notice.passive_message_id, "source-message");
        assert_eq!(notice.text, "图片未能读取，请重新发送。");
        assert!(notice.image.is_none());
        assert!(!notice.markdown);
        assert!(
            !state
                .fail_generation_with_notice(1, 3, "image_input_failed", "重复提示")
                .unwrap()
        );
        assert_eq!(state.requests[&1].bubbles.len(), 1);
    }

    #[test]
    fn exhausted_failure_notice_quota_records_failure_without_sending_or_success_progress() {
        let mut state = generating();
        state.requests.get_mut(&1).unwrap().bubbles.push(Bubble {
            revision: 2,
            passive_message_id: "source-message".into(),
            msg_seq: 5,
            status: DeliveryStatus::Sent,
            ..Default::default()
        });
        assert!(
            state
                .fail_generation_with_notice(1, 3, "image_input_failed", "请重新发送图片。")
                .unwrap()
        );
        let request = &state.requests[&1];
        assert_eq!(request.status, RequestStatus::Failed);
        assert_eq!(request.failure_code.as_deref(), Some("image_input_failed"));
        assert_eq!(request.bubbles.len(), 1);
        assert_eq!(
            (
                request.accepted_revision,
                request.generated_revision,
                request.delivered_revision
            ),
            (2, 2, 1)
        );
    }

    #[test]
    fn failure_notice_rejects_cancelled_stale_and_non_generating_turns_without_mutation() {
        for (revision, status, cancelled) in [
            (2, RequestStatus::Generating, None),
            (3, RequestStatus::Cancelled, Some(3)),
            (3, RequestStatus::Generating, Some(3)),
            (3, RequestStatus::Queued, None),
            (3, RequestStatus::UnknownDelivery, None),
        ] {
            let mut state = generating();
            let request = state.requests.get_mut(&1).unwrap();
            request.status = status;
            request.cancelled_revision = cancelled;
            let before = serde_json::to_string(&state).unwrap();
            assert!(
                !state
                    .fail_generation_with_notice(1, revision, "image_input_failed", "失败提示")
                    .unwrap()
            );
            assert_eq!(serde_json::to_string(&state).unwrap(), before);
        }
    }

    #[test]
    fn reconciled_failure_notice_never_requeues_the_model_turn() {
        for was_sent in [true, false] {
            let mut state = generating();
            state
                .fail_generation_with_notice(1, 3, "image_input_failed", "请重新发送图片。")
                .unwrap();
            let request = state.requests.get_mut(&1).unwrap();
            request.bubbles[0].status = DeliveryStatus::Unknown;
            request.status = RequestStatus::UnknownDelivery;
            state.resolve(1, 0, was_sent).unwrap();
            let request = &state.requests[&1];
            assert_eq!(request.status, RequestStatus::Ready);
            assert_eq!(
                request.bubbles[0].status,
                if was_sent {
                    DeliveryStatus::Sent
                } else {
                    DeliveryStatus::Pending
                }
            );
            assert_eq!(request.generated_revision, 2);
            assert_eq!(request.failure_code.as_deref(), Some("image_input_failed"));
            assert!(state.begin_generation(1).is_err());
        }
    }

    #[test]
    fn a_new_user_revision_clears_the_previous_failure_code_and_cancels_unsent_notice() {
        let mut state = generating();
        state
            .fail_generation_with_notice(1, 3, "image_input_failed", "请重新发送图片。")
            .unwrap();
        let seq = state
            .ingest(
                ChatMessage {
                    id: "new-source".into(),
                    author_id: "member".into(),
                    text: "重新发一次".into(),
                    ..Default::default()
                },
                20,
            )
            .unwrap()
            .unwrap();
        state.begin_decision(seq).unwrap();
        state
            .apply_decision(
                seq,
                Decision {
                    action: Action::Revise,
                    request_id: Some(1),
                    source: "jev".into(),
                    ..Default::default()
                },
                16,
            )
            .unwrap();
        let request = &state.requests[&1];
        assert_eq!(request.revision, 4);
        assert_eq!(request.failure_code, None);
        assert_eq!(request.status, RequestStatus::Queued);
        assert_eq!(request.bubbles[0].status, DeliveryStatus::Cancelled);
        assert_eq!(request.generated_revision, 2);
    }

    #[test]
    fn failure_notice_accepts_only_bounded_application_codes_and_nonempty_text() {
        for (code, notice) in [
            ("raw error: private details", "失败"),
            ("image_input_failed", "  "),
            ("", "失败"),
        ] {
            let mut state = generating();
            assert!(
                state
                    .fail_generation_with_notice(1, 3, code, notice)
                    .is_err()
            );
            assert_eq!(state.requests[&1].status, RequestStatus::Generating);
            assert!(state.requests[&1].failure_code.is_none());
            assert!(state.requests[&1].bubbles.is_empty());
        }
        assert!(
            generating()
                .fail_generation_with_notice(1, 3, "image_input_failed", &"字".repeat(1001))
                .is_err()
        );
    }
}
