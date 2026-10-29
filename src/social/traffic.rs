//! Admission pacing is separate from participation and semantic association.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::model::social::{
    GroupState, MessageRecord, ParticipationMode, RequestRecord, RequestStatus,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TrafficSettings {
    pub mode3_start_interval_ms: u64,
    pub mode3_chat_interval_ms: u64,
}

impl Default for TrafficSettings {
    fn default() -> Self {
        Self {
            mode3_start_interval_ms: 6_000,
            mode3_chat_interval_ms: 15_000,
        }
    }
}

impl TrafficSettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.mode3_start_interval_ms <= self.mode3_chat_interval_ms
                && self.mode3_chat_interval_ms <= 60_000,
            "invalid_traffic_intervals"
        );
        Ok(())
    }
}

pub(super) fn optional_chat(state: &GroupState, request: &RequestRecord) -> bool {
    request.style == "chat"
        && !request
            .source_seqs
            .iter()
            .any(|seq| state.record(*seq).is_ok_and(directed_to_bot))
}

pub(super) fn directed_to_bot(record: &MessageRecord) -> bool {
    record.message.at_bot
        || record.decision.as_ref().is_some_and(|d| {
            ["exchange", "you_referent"]
                .iter()
                .any(|key| d.judgments.get(*key).is_some_and(|j| j.choice == "bot"))
        })
}

/// Capacity may withdraw only an unsolicited contribution that has never run.
/// Work with tools, a continuation, clarification or uncertain delivery stays put.
pub(super) fn withdrawable_chat(state: &GroupState, request: &RequestRecord) -> bool {
    optional_chat(state, request)
        && request.status == RequestStatus::Queued
        && !request.control
        && !request.long_task
        && request.cancelled_revision.is_none()
        && request.failure_code.is_none()
        && request.submitted_revision == 0
        && request.codex_started_revision == 0
        && request.codex_thread_id.is_none()
        && request.bubbles.is_empty()
        && request.scheduled_run_id.is_none()
}

#[derive(Default)]
pub(super) struct Pacing {
    last_start: Option<i64>,
}

impl Pacing {
    pub fn ready(
        &self,
        state: &GroupState,
        id: u64,
        config: &super::config::SocialConfig,
        private: bool,
        elapsed_ms: i64,
        wall_ms: i64,
    ) -> bool {
        if private || state.mode_override.unwrap_or(config.mode) != ParticipationMode::Social {
            return true;
        }
        let request = &state.requests[&id];
        // Match the selector's aging policy, so ongoing direct questions cannot
        // keep a queued social contribution permanently ineligible.
        let interval = if optional_chat(state, request)
            && wall_ms.saturating_sub(request.updated_at_ms) < 60_000
        {
            config.traffic.mode3_chat_interval_ms
        } else {
            config.traffic.mode3_start_interval_ms
        };
        self.last_start
            .is_none_or(|last| elapsed_ms.saturating_sub(last) >= interval as i64)
    }

    pub fn started(&mut self, now: i64) {
        self.last_start = Some(now);
    }
}

/// First-change deadline is stable during a burst. Observe model-facing
/// revisions, not every unrelated group message. Cancellation bypasses this.
#[derive(Default)]
pub(super) struct Amendments {
    first: Option<i64>,
    last: i64,
    revision: u64,
    through: Option<u64>,
}

impl Amendments {
    pub fn decision_fence(&mut self, latest: u64) -> u64 {
        *self.through.get_or_insert(latest)
    }
    pub fn ready(
        &mut self,
        revision: u64,
        pending_decisions: bool,
        now: i64,
        quiet: u64,
        maximum: u64,
    ) -> bool {
        let first = *self.first.get_or_insert(now);
        if revision != self.revision {
            self.revision = revision;
            self.last = now;
        }
        !pending_decisions
            && (now.saturating_sub(first) >= maximum as i64
                || now.saturating_sub(self.last) >= quiet as i64)
    }
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mode3_paces_starts_without_delaying_the_first_or_other_modes() {
        let mut state = GroupState::default();
        state.requests.insert(
            1,
            RequestRecord {
                id: 1,
                style: "chat".into(),
                ..Default::default()
            },
        );
        let mut pacing = Pacing::default();
        let mut config = super::super::config::SocialConfig {
            mode: ParticipationMode::Social,
            ..Default::default()
        };
        assert!(pacing.ready(&state, 1, &config, false, 0, 0));
        pacing.started(0);
        assert!(!pacing.ready(&state, 1, &config, false, 14_999, 14_999));
        assert!(pacing.ready(&state, 1, &config, false, 15_000, 15_000));
        assert!(pacing.ready(&state, 1, &config, false, 6_000, 60_000));
        state.requests.get_mut(&1).unwrap().style = "answer".into();
        assert!(!pacing.ready(&state, 1, &config, false, 5_999, 5_999));
        assert!(pacing.ready(&state, 1, &config, false, 6_000, 6_000));
        assert!(pacing.ready(&state, 1, &config, true, 1, 1));
        config.mode = ParticipationMode::Helpful;
        assert!(pacing.ready(&state, 1, &config, false, 1, 1));
    }
    #[test]
    fn a_burst_coalesces_without_starving_in_continuous_chat() {
        let mut batch = Amendments::default();
        assert!(!batch.ready(2, false, 0, 1200, 4000));
        assert!(!batch.ready(3, false, 1000, 1200, 4000));
        assert!(!batch.ready(3, true, 2200, 1200, 4000));
        assert!(batch.ready(3, false, 2200, 1200, 4000));
        batch.clear();
        assert!(!batch.ready(4, true, 2300, 1200, 4000));
        assert!(!batch.ready(5, true, 6000, 1200, 4000));
        assert!(!batch.ready(6, true, 6300, 1200, 4000));
        assert!(batch.ready(6, false, 6301, 1200, 4000));
    }
}
