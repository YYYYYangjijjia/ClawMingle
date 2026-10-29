//! Per-request collection and finite decision fences. New unrelated traffic
//! must not keep an already collected request or its outbox waiting forever.
use std::collections::BTreeMap;

use crate::model::social::{DecisionStatus, GroupState, RequestStatus};

use super::state::CollectionWindow;

#[derive(Default)]
struct Collection {
    revision: u64,
    window: CollectionWindow,
    through: Option<u64>,
    due: bool,
}

#[derive(Default)]
pub(super) struct Dispatch {
    requests: BTreeMap<u64, Collection>,
    deliveries: BTreeMap<(u64, usize, u64), u64>,
}

pub(super) fn pending_through(state: &GroupState, through: u64) -> bool {
    state.messages.iter().any(|r| {
        r.seq <= through && matches!(r.status, DecisionStatus::Pending | DecisionStatus::Running)
    })
}

impl Dispatch {
    pub fn observe(&mut self, state: &GroupState, now: i64, quiet: u64, maximum: u64) {
        self.requests.retain(|id, _| {
            state
                .requests
                .get(id)
                .is_some_and(|r| r.status == RequestStatus::Queued)
        });
        self.deliveries.retain(|(id, index, revision), _| {
            state.requests.get(id).is_some_and(|r| {
                r.revision == *revision
                    && r.bubbles.get(*index).is_some_and(|b| {
                        matches!(
                            b.status,
                            crate::model::social::DeliveryStatus::Pending
                                | crate::model::social::DeliveryStatus::Sending
                        )
                    })
            })
        });
        for r in state
            .requests
            .values()
            .filter(|r| r.status == RequestStatus::Queued)
        {
            let entry = self.requests.entry(r.id).or_default();
            if entry.revision != r.revision {
                entry.revision = r.revision;
                entry.window.observe(now);
            }
            entry.due = entry.window.due(now, quiet as i64, maximum as i64);
            if entry.through.is_none() && entry.due {
                entry.through = Some(state.next_seq.saturating_sub(1));
            }
        }
    }

    pub fn ready(&self, state: &GroupState, id: u64) -> bool {
        self.requests
            .get(&id)
            .filter(|entry| entry.due)
            .and_then(|entry| entry.through)
            .is_some_and(|through| !pending_through(state, through))
    }

    pub fn forget_delivery(&mut self, id: u64, index: usize) {
        self.deliveries
            .retain(|(r, i, _), _| *r != id || *i != index);
    }

    pub fn delivery_ready(&mut self, state: &GroupState, id: u64, index: usize) -> bool {
        let request = &state.requests[&id];
        if request.control || request.bubbles[index].origin.is_progress() {
            return true;
        }
        let through = *self
            .deliveries
            .entry((id, index, request.revision))
            .or_insert_with(|| state.next_seq.saturating_sub(1));
        !pending_through(state, through)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::social::{ChatMessage, RequestRecord};

    fn state() -> GroupState {
        let mut state = GroupState::default();
        state.requests.insert(
            1,
            RequestRecord {
                id: 1,
                revision: 1,
                ..Default::default()
            },
        );
        state
    }

    #[test]
    fn ongoing_unrelated_messages_do_not_move_a_frozen_fence() {
        let mut state = state();
        let first = state
            .ingest(
                ChatMessage {
                    id: "first".into(),
                    author_id: "member".into(),
                    ..Default::default()
                },
                100,
            )
            .unwrap()
            .unwrap();
        let mut dispatch = Dispatch::default();
        dispatch.observe(&state, 0, 1200, 4000);
        dispatch.observe(&state, 1200, 1200, 4000);
        assert!(!dispatch.ready(&state, 1));
        state.fail_decision(first);
        state
            .ingest(
                ChatMessage {
                    id: "later".into(),
                    author_id: "member".into(),
                    ..Default::default()
                },
                100,
            )
            .unwrap();
        dispatch.observe(&state, 1300, 1200, 4000);
        assert!(state.has_pending());
        assert!(dispatch.ready(&state, 1));
    }

    #[test]
    fn related_revisions_coalesce_but_cannot_extend_the_hard_deadline() {
        let mut state = state();
        let mut dispatch = Dispatch::default();
        dispatch.observe(&state, 0, 1200, 4000);
        for now in [1000, 2000, 3000] {
            state.requests.get_mut(&1).unwrap().revision += 1;
            dispatch.observe(&state, now, 1200, 4000);
            assert!(!dispatch.ready(&state, 1));
        }
        dispatch.observe(&state, 4000, 1200, 4000);
        assert!(dispatch.ready(&state, 1));
        state.requests.get_mut(&1).unwrap().status = RequestStatus::Cancelled;
        dispatch.observe(&state, 4100, 1200, 4000);
        assert!(!dispatch.ready(&state, 1));
    }
    #[test]
    fn a_frozen_request_still_collects_related_changes_until_its_maximum() {
        let mut state = state();
        let mut dispatch = Dispatch::default();
        dispatch.observe(&state, 0, 1200, 4000);
        dispatch.observe(&state, 1200, 1200, 4000);
        assert!(dispatch.ready(&state, 1));
        state.requests.get_mut(&1).unwrap().revision += 1;
        dispatch.observe(&state, 1300, 1200, 4000);
        assert!(!dispatch.ready(&state, 1));
        dispatch.observe(&state, 2500, 1200, 4000);
        assert!(dispatch.ready(&state, 1));
    }
    #[test]
    fn each_bubble_and_resumed_delivery_gets_a_new_finite_fence() {
        use crate::model::social::Bubble;
        let mut state = state();
        state.requests.get_mut(&1).unwrap().bubbles = vec![
            Bubble {
                revision: 1,
                ..Default::default()
            },
            Bubble {
                revision: 1,
                ..Default::default()
            },
        ];
        let mut dispatch = Dispatch::default();
        assert!(dispatch.delivery_ready(&state, 1, 0));
        let seq = state
            .ingest(
                ChatMessage {
                    id: "new-change".into(),
                    author_id: "member".into(),
                    ..Default::default()
                },
                100,
            )
            .unwrap()
            .unwrap();
        assert!(dispatch.delivery_ready(&state, 1, 0));
        assert!(!dispatch.delivery_ready(&state, 1, 1));
        dispatch.forget_delivery(1, 0);
        assert!(!dispatch.delivery_ready(&state, 1, 0));
        state.fail_decision(seq);
        assert!(dispatch.delivery_ready(&state, 1, 0));
    }
}
