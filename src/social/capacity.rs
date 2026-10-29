//! Bounded overload handling for the rolling message window. Archive rejected
//! arrivals before pruning; never evict an active source or stop other scopes.
use anyhow::Result;

use super::{
    commands::{ChatCommand, parse_command},
    config::SocialConfig,
    control,
};
use crate::model::social::{Action, ChatMessage, Decision, GroupState};

#[derive(Default)]
pub(super) struct Capacity {
    last_notice_ms: Option<i64>,
}

impl Capacity {
    /// Returns an ordinary message for normal routing, or None when replayed or
    /// handled under pressure. The caller MUST save/archive before pruning.
    pub fn receive(
        &mut self,
        state: &mut GroupState,
        message: ChatMessage,
        config: &SocialConfig,
        private: bool,
        target: &str,
        now: i64,
    ) -> Result<Option<u64>> {
        let full = state.messages.len() >= config.max_messages;
        // One transient record lets recovery commands reach their normal
        // authorization checks. Only the existing bounded control outbox may
        // retain additional sources; rejected arrivals are archived and pruned.
        let limit = config
            .max_messages
            .max(state.messages.len().saturating_add(1));
        let Some(seq) = state.ingest(message, limit)? else {
            return Ok(None);
        };
        if !full {
            return Ok(Some(seq));
        }
        let record = state.record(seq)?;
        let mut text = record.message.text.trim();
        if record.message.at_bot
            && text.starts_with("<@")
            && let Some(end) = text.find('>')
        {
            text = text[end + 1..].trim_start();
        }
        match parse_command(text) {
            Some(
                command @ (ChatCommand::Cancel(_)
                | ChatCommand::CancelCode(_)
                | ChatCommand::Status
                | ChatCommand::StatusRequest(_)
                | ChatCommand::StatusCode(_)
                | ChatCommand::Help
                | ChatCommand::Ping),
            ) => {
                control::handle_command(state, seq, command, config, private, target, now)?;
            }
            _ => {
                state.begin_decision(seq)?;
                state.apply_decision(
                    seq,
                    Decision {
                        action: Action::Silent,
                        source: "capacity".into(),
                        snapshot_through_seq: seq,
                        ..Default::default()
                    },
                    config.max_active_requests,
                )?;
                if self
                    .last_notice_ms
                    .is_none_or(|last| now.saturating_sub(last) >= 60_000)
                {
                    control::control_reply(state, seq,
                        "当前对话积压已满，这条消息已存档但未加入任务队列。请先用 /status 查看、用 /cancel 取消不再需要的任务，腾出空间后重新发送。".into(),
                        config, now)?;
                    self.last_notice_ms = Some(now);
                }
                state
                    .messages
                    .iter_mut()
                    .find(|r| r.seq == seq)
                    .expect("arrival retained")
                    .application = Some("message_capacity_rejected".into());
                tracing::warn!(
                    "conversation message capacity reached; arrival archived without execution"
                );
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::social::{DeliveryStatus, RequestStatus};
    use crate::social::{history::Item, store::Store};
    use std::os::unix::fs::PermissionsExt;

    fn message(id: usize, text: &str) -> ChatMessage {
        ChatMessage {
            id: format!("fixture-{id}"),
            author_id: "member".into(),
            text: text.into(),
            timestamp_ms: id as i64,
            at_bot: true,
            ..Default::default()
        }
    }

    #[test]
    fn pinned_sources_do_not_pin_the_rolling_window_or_archive() {
        for status in [
            RequestStatus::WaitingForUser,
            RequestStatus::UnknownDelivery,
            RequestStatus::Generating,
            RequestStatus::Queued,
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let store = Store::new(dir.path(), "fixture").unwrap();
            let config = SocialConfig {
                data_dir: dir.path().into(),
                ..Default::default()
            };
            let mut state = GroupState::default();
            let first = state.ingest(message(1, "task"), 500).unwrap().unwrap();
            state.begin_decision(first).unwrap();
            state
                .apply_decision(
                    first,
                    Decision {
                        action: Action::NewRequest,
                        ..Default::default()
                    },
                    16,
                )
                .unwrap();
            state.requests.get_mut(&1).unwrap().status = status;
            if status == RequestStatus::UnknownDelivery {
                state
                    .requests
                    .get_mut(&1)
                    .unwrap()
                    .bubbles
                    .push(crate::model::social::Bubble {
                        status: DeliveryStatus::Unknown,
                        passive_message_id: "fixture-1".into(),
                        ..Default::default()
                    });
            }
            for id in 2..=500 {
                let seq = state.ingest(message(id, "chat"), 500).unwrap().unwrap();
                state.begin_decision(seq).unwrap();
                state.apply_decision(seq, Decision::default(), 16).unwrap();
            }
            store.save(&state).unwrap();
            state.prune(500, 10_000, 500);
            assert_eq!(state.messages.len(), 499);
            assert!(state.record(1).is_ok());
            assert!(state.record(2).is_err());
            let mut capacity = Capacity::default();
            let seq = capacity
                .receive(
                    &mut state,
                    message(501, "/status"),
                    &config,
                    false,
                    "fixture",
                    501,
                )
                .unwrap()
                .unwrap();
            control::handle_command(
                &mut state,
                seq,
                ChatCommand::Status,
                &config,
                false,
                "fixture",
                501,
            )
            .unwrap();
            store.save(&state).unwrap();
            let reader = store.history.reader().unwrap();
            assert!(matches!(
                reader.lookup("fixture-2").unwrap(),
                Some(Item::Incoming(_))
            ));
            drop(reader);
            let mut restored = store.load().unwrap();
            restored.recover();
            restored.prune(10_000, 1, 500);
            assert!(restored.messages.len() < 500);
            if status != RequestStatus::Generating {
                assert!(restored.record(1).is_ok());
            }
        }
    }

    #[test]
    fn fully_pinned_window_archives_overload_and_still_accepts_authorized_cancel() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = Store::new(dir.path(), "fixture").unwrap();
        let config = SocialConfig {
            max_messages: 2,
            data_dir: dir.path().into(),
            ..Default::default()
        };
        let mut state = GroupState::default();
        for id in 1..=2 {
            let seq = state.ingest(message(id, "task"), 2).unwrap().unwrap();
            state.begin_decision(seq).unwrap();
            state
                .apply_decision(
                    seq,
                    Decision {
                        action: Action::NewRequest,
                        ..Default::default()
                    },
                    16,
                )
                .unwrap();
            state.requests.get_mut(&(id as u64)).unwrap().status = RequestStatus::WaitingForUser;
        }
        let mut capacity = Capacity::default();
        for id in 3..103 {
            assert!(
                capacity
                    .receive(
                        &mut state,
                        message(id, "more work"),
                        &config,
                        false,
                        "fixture",
                        id as i64
                    )
                    .unwrap()
                    .is_none()
            );
            store.save(&state).unwrap();
            state.prune(id as i64, 10_000, 2);
            store.save(&state).unwrap();
            assert!(state.messages.len() <= 3); // two active sources plus one notice
            assert!(state.record(1).is_ok() && state.record(2).is_ok());
        }
        let Item::Incoming(rejected) = store
            .history
            .reader()
            .unwrap()
            .lookup("fixture-102")
            .unwrap()
            .unwrap()
        else {
            panic!("archived arrival")
        };
        assert_eq!(
            rejected.application.as_deref(),
            Some("message_capacity_rejected")
        );
        let mut stranger = message(103, "/cancel R1");
        stranger.author_id = "other-member".into();
        capacity
            .receive(&mut state, stranger, &config, false, "fixture", 103)
            .unwrap();
        assert_eq!(state.requests[&1].status, RequestStatus::WaitingForUser);
        capacity
            .receive(
                &mut state,
                message(104, "/cancel R1"),
                &config,
                false,
                "fixture",
                104,
            )
            .unwrap();
        assert_eq!(state.requests[&1].status, RequestStatus::Cancelled);
        store.save(&state).unwrap();
        let mut restored = store.load().unwrap();
        restored.recover();
        restored.prune(1000, 10_000, 2);
        assert!(restored.record(1).is_err());
        // Pressure is per conversation; an independent scope still admits work.
        assert!(
            capacity
                .receive(
                    &mut GroupState::default(),
                    message(1, "question"),
                    &config,
                    false,
                    "second",
                    104
                )
                .unwrap()
                .is_some()
        );
    }
}
