//! One bounded read-only turn can answer several logical requests. Ownership,
//! revisions, outboxes and later native history remain per request.
mod execution;
mod review;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::config::SocialConfig;
use crate::model::social::{GroupState, RequestRecord, RequestStatus};

pub(super) use execution::{Batch, Oversized, execute};
pub(super) use review::Review;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    pub max_requests: usize,
    pub max_input_tokens: usize,
    pub review_after_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_requests: 4,
            max_input_tokens: 12_000,
            review_after_seconds: 30,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (2..=8).contains(&self.max_requests)
                && (2000..=30_000).contains(&self.max_input_tokens)
                && (10..=300).contains(&self.review_after_seconds),
            "invalid_reply_batching"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Member {
    pub id: u64,
    pub revision: u64,
}

impl Member {
    pub fn matches(&self, state: &GroupState, status: RequestStatus) -> bool {
        state.requests.get(&self.id).is_some_and(|r| {
            r.revision == self.revision && r.status == status && r.cancelled_revision.is_none()
        })
    }
}

fn fresh(request: &RequestRecord) -> bool {
    request.status == RequestStatus::Queued
        && !request.control
        && !request.long_task
        && request.cancelled_revision.is_none()
        && request.failure_code.is_none()
        && request.submitted_revision == 0
        && request.codex_started_revision == 0
        && request.codex_thread_id.is_none()
        && request.bubbles.is_empty()
        && request.scheduled_run_id.is_none()
        && request.batch_individual_revision != Some(request.revision)
}

/// Configuration changes invalidate the entire read-only view, including a
/// reset whose request revisions might otherwise happen to match.
pub(super) fn policy_key(state: &GroupState) -> String {
    serde_json::json!([
        state.context_start_seq,
        state.group_profile,
        state.identity,
        state.mode_override,
        state.model_alias,
        state.enabled_skills,
        state.tools_override,
        state.browser_enabled,
        state.plugin_grants
    ])
    .to_string()
}

struct Stamp {
    revision: u64,
    epoch: u64,
    at: i64,
    short: bool,
    policy: String,
    through: u64,
    delivered: u64,
    unavailable: bool,
}

fn delivered_watermark(state: &GroupState) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut stamp = std::collections::hash_map::DefaultHasher::new();
    for r in state.requests.values() {
        for (index, b) in r.bubbles.iter().enumerate() {
            if b.status == crate::model::social::DeliveryStatus::Sent {
                (r.id, index, b.revision, b.sent_at_ms).hash(&mut stamp);
            }
        }
    }
    stamp.finish()
}

#[derive(Default)]
pub(super) struct Coordinator {
    reviewed: BTreeMap<u64, Stamp>,
}

pub(super) enum Plan {
    Review(Review),
    Batch(Vec<Member>),
    Single,
}

impl Coordinator {
    fn valid(
        &self,
        state: &GroupState,
        id: u64,
        config: &SocialConfig,
        epoch: u64,
        now: i64,
    ) -> bool {
        self.reviewed.get(&id).is_some_and(|stamp| {
            stamp.revision == state.requests[&id].revision
                && stamp.epoch == epoch
                && stamp.policy == policy_key(state)
                && now.saturating_sub(stamp.at) < config.batching.review_after_seconds as i64 * 1000
                && (stamp.unavailable
                    || !super::traffic::optional_chat(state, &state.requests[&id])
                    || (stamp.through == state.next_seq.saturating_sub(1)
                        && stamp.delivered == delivered_watermark(state)))
        })
    }

    /// Keep ordinary FIFO aging, but do not let an old optional reply whose
    /// review keeps going stale block an explicit question behind it.
    pub fn select(
        &self,
        state: &GroupState,
        first: u64,
        ready: impl Fn(u64) -> bool,
        config: &SocialConfig,
        epoch: u64,
        now: i64,
    ) -> u64 {
        if config.batching.enabled
            && super::traffic::optional_chat(state, &state.requests[&first])
            && self.reviewed.contains_key(&first)
            && !self.valid(state, first, config, epoch, now)
        {
            return state
                .requests
                .values()
                .find(|r| {
                    r.status == RequestStatus::Queued
                        && !super::traffic::optional_chat(state, r)
                        && ready(r.id)
                })
                .map_or(first, |r| r.id);
        }
        first
    }

    pub fn plan(
        &mut self,
        state: &GroupState,
        first: u64,
        ready: impl Fn(u64) -> bool,
        config: &SocialConfig,
        epoch: u64,
        now: i64,
    ) -> Plan {
        if !config.batching.enabled {
            return Plan::Single;
        }
        self.reviewed.retain(|id, _| {
            state
                .requests
                .get(id)
                .is_some_and(|r| r.status == RequestStatus::Queued)
        });
        if !fresh(&state.requests[&first]) {
            return Plan::Single;
        }
        let candidates: Vec<_> = std::iter::once(first)
            .chain(state.requests.keys().copied().filter(|id| *id != first))
            .filter(|id| ready(*id) && fresh(&state.requests[id]))
            .take(16)
            .collect();
        let valid = |id: &u64| self.valid(state, *id, config, epoch, now);
        let aged = candidates.iter().any(|id| {
            super::traffic::optional_chat(state, &state.requests[id])
                && now.saturating_sub(state.requests[id].updated_at_ms)
                    >= config.batching.review_after_seconds as i64 * 1000
        });
        if (candidates.len() >= 2 || aged || epoch > 0) && !valid(&first) {
            // One review per revision/policy/admission epoch. More unrelated
            // chat may invalidate its relevance, but must not create an endless
            // review loop. Execute individually with current context instead of
            // using the stale short/obsolete judgment or replaying old work.
            if self.reviewed.get(&first).is_some_and(|stamp| {
                stamp.revision == state.requests[&first].revision
                    && stamp.epoch == epoch
                    && stamp.policy == policy_key(state)
            }) {
                return Plan::Single;
            }
            return Plan::Review(Review::new(state, &candidates, epoch, now));
        }
        if !candidates.contains(&first) {
            return Plan::Single;
        }
        let members: Vec<_> = candidates
            .into_iter()
            .filter(|id| {
                valid(id)
                    && self.reviewed[id].short
                    && state.requests[id].model_alias == state.requests[&first].model_alias
            })
            .take(config.batching.max_requests)
            .map(|id| Member {
                id,
                revision: state.requests[&id].revision,
            })
            .collect();
        if members.len() >= 2 && members[0].id == first {
            Plan::Batch(members)
        } else {
            Plan::Single
        }
    }

    pub fn reviewed(
        &mut self,
        state: &mut GroupState,
        review: &Review,
        answers: Result<Vec<review::Verdict>>,
        now: i64,
    ) {
        if policy_key(state) != review.policy {
            return;
        }
        // A failed review retains every request and falls back to ordinary
        // execution. Cache that result briefly instead of retrying every tick.
        let unavailable = answers.is_err();
        let answers = answers.unwrap_or_default();
        for member in &review.members {
            if !member.matches(state, RequestStatus::Queued) {
                continue;
            }
            let answer = answers.iter().find(|a| a.id == member.id);
            let obsolete = answer.is_some_and(|a| a.obsolete)
                && state.next_seq.saturating_sub(1) == review.through
                && delivered_watermark(state) == review.delivered
                && super::traffic::optional_chat(state, &state.requests[&member.id]);
            if obsolete {
                let request = state.requests.get_mut(&member.id).expect("review member");
                request.status = RequestStatus::Cancelled;
                request.cancelled_revision = Some(request.revision);
                request.failure_code = Some("optional_reply_obsolete".into());
                tracing::info!(
                    request_id = member.id,
                    "obsolete optional reply withdrawn before execution"
                );
            } else {
                self.reviewed.insert(
                    member.id,
                    Stamp {
                        revision: member.revision,
                        epoch: review.epoch,
                        at: now,
                        short: answer.is_some_and(|a| a.short),
                        policy: review.policy.clone(),
                        through: review.through,
                        delivered: review.delivered,
                        unavailable,
                    },
                );
            }
        }
    }

    pub fn individual(state: &mut GroupState, members: &[Member]) {
        for member in members {
            if let Some(r) = state.requests.get_mut(&member.id)
                && r.revision == member.revision
            {
                r.batch_individual_revision = Some(member.revision);
            }
        }
    }
}
