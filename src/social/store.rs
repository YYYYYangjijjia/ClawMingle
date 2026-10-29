use std::{
    fs::File,
    path::{Path, PathBuf},
};

use anyhow::{Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::{model::social::GroupState, util::fs::atomic_write};

use super::config::{SocialConfig, private_read_limit};

const MAX_STATE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct StoredGroup {
    group_id: String,
    state: GroupState,
}

pub fn private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        if !path.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
        }
        let metadata = std::fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_dir()
                && metadata.uid() == std::fs::metadata("/proc/self")?.uid()
                && metadata.permissions().mode() & 0o777 == 0o700,
            "private_data_directory_required"
        );
    }
    #[cfg(not(unix))]
    anyhow::bail!("social_runtime_requires_linux");
    Ok(())
}

pub struct Store {
    pub path: PathBuf,
    pub group_id: String,
    pub history: std::sync::Arc<super::history::History>,
}

impl Store {
    pub fn new(data_dir: &Path, group_id: &str) -> Result<Self> {
        let _barrier = super::backup::state_write_guard()?;
        let groups = data_dir.join("groups");
        private_directory(&groups)?;
        // Group IDs are validated before they may be used in a path.
        ensure!(
            !group_id.is_empty()
                && group_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "invalid_group_id"
        );
        Ok(Self {
            path: groups.join(format!("{group_id}.json")),
            group_id: group_id.to_string(),
            history: std::sync::Arc::new(super::history::History::new(data_dir, group_id)?),
        })
    }
    pub fn load(&self) -> Result<GroupState> {
        if !self.path.exists() {
            return Ok(GroupState::default());
        }
        let raw = private_read_limit(&self.path, MAX_STATE_BYTES)?;
        let mut stored: StoredGroup =
            serde_json::from_str(&raw).map_err(|_| anyhow::anyhow!("group_state_invalid"))?;
        ensure!(
            stored.group_id == self.group_id && stored.state.schema_version == 1,
            "group_state_identity_or_version_mismatch"
        );
        super::task_identity::migrate(&mut stored.state);
        Ok(stored.state)
    }
    pub fn save(&self, state: &GroupState) -> Result<()> {
        let _barrier = super::backup::state_write_guard()?;
        self.write_state(state)?;
        self.history.save(state)
    }
    /// Canonical state commit for identity-only changes; no chat archive changed.
    pub(super) fn save_state(&self, state: &GroupState) -> Result<()> {
        let _barrier = super::backup::state_write_guard()?;
        self.write_state(state)
    }
    fn write_state(&self, state: &GroupState) -> Result<()> {
        // Do not preserve unexpectedly loose existing permissions through atomic_write.
        if self.path.exists() {
            let _ = private_read_limit(&self.path, MAX_STATE_BYTES)?;
        }
        let raw = serde_json::to_string(&StoredGroup {
            group_id: self.group_id.clone(),
            state: state.clone(),
        })?;
        ensure!(
            raw.len() as u64 <= MAX_STATE_BYTES,
            "group_state_capacity_reached"
        );
        atomic_write(&self.path, &raw)
    }
}

pub fn acquire_lock(data_dir: &Path) -> Result<File> {
    private_directory(data_dir)?;
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(0o400000)
            .open(data_dir.join("operator.lock"))?
    };
    #[cfg(not(unix))]
    let file = File::open(data_dir.join("operator.lock"))?;
    file.try_lock_exclusive()
        .map_err(|_| anyhow::anyhow!("social_runtime_already_running"))?;
    Ok(file)
}

pub fn show_status(config: &SocialConfig) -> Result<()> {
    let _lock = acquire_lock(&config.data_dir)?;
    for (index, group) in config.allowed_groups.iter().enumerate() {
        let state = Store::new(&config.data_dir, group)?.load()?;
        println!(
            "group {}: messages={}, decision_cursor={}",
            index,
            state.messages.len(),
            state.decided_through_seq
        );
        for request in state.requests.values() {
            println!(
                "  R{}: {:?}, current={}, submitted={}, accepted={}, generated={}, delivered={}",
                request.id,
                request.status,
                request.revision,
                request.submitted_revision,
                request.accepted_revision,
                request.generated_revision,
                request.delivered_revision
            );
            for (bubble, record) in request.bubbles.iter().enumerate() {
                println!("    bubble {}: {:?}", bubble, record.status);
            }
        }
    }
    Ok(())
}

pub fn resolve_delivery(
    config: &SocialConfig,
    group: usize,
    request: u64,
    bubble: usize,
    was_sent: bool,
) -> Result<()> {
    let _lock = acquire_lock(&config.data_dir)?;
    let group_id = config
        .allowed_groups
        .get(group)
        .ok_or_else(|| anyhow::anyhow!("group_index_out_of_range"))?;
    let store = Store::new(&config.data_dir, group_id)?;
    let mut state = store.load()?;
    ensure!(
        state
            .requests
            .get(&request)
            .is_none_or(|r| r.scheduled_run_id.is_none()),
        "scheduled_delivery_use_task_resolve"
    );
    state.recover();
    state.resolve(request, bubble, was_sent)?;
    store.save(&state)?;
    println!("Delivery reconciled from the operator's observation; no messages sent.");
    Ok(())
}

pub fn retry_failed(config: &SocialConfig, group: usize) -> Result<()> {
    use crate::model::social::{DecisionStatus, DeliveryStatus, RequestStatus};
    let _lock = acquire_lock(&config.data_dir)?;
    let group_id = config
        .allowed_groups
        .get(group)
        .ok_or_else(|| anyhow::anyhow!("group_index_out_of_range"))?;
    let store = Store::new(&config.data_dir, group_id)?;
    let mut state = store.load()?;
    state.recover();
    for record in &mut state.messages {
        // Queue overflow retains its classified intent and is admitted by the
        // runtime when capacity returns; never replay its old message timeline.
        if record.status == DecisionStatus::Failed {
            record.status = DecisionStatus::Pending;
            record.application = None;
            state.decided_through_seq = state.decided_through_seq.min(record.seq.saturating_sub(1));
        }
    }
    let mut remaining = config.max_active_requests.saturating_sub(
        state
            .requests
            .values()
            .filter(|r| {
                r.scheduled_run_id.is_none()
                    && !matches!(
                        r.status,
                        RequestStatus::Completed | RequestStatus::Cancelled | RequestStatus::Failed
                    )
            })
            .count(),
    );
    for request in state.requests.values_mut().filter(|r| {
        r.scheduled_run_id.is_none()
            && r.status == RequestStatus::Failed
            && r.cancelled_revision.is_none()
    }) {
        if remaining == 0 {
            break;
        }
        if request
            .bubbles
            .iter()
            .any(|b| b.status == DeliveryStatus::Unknown)
        {
            continue;
        }
        remaining -= 1;
        if request.failure_code.take().is_some() {
            // Explicit retry starts a new model attempt. Keep delivered notices as
            // evidence, but never resend an unsent notice from the failed attempt.
            for bubble in request.bubbles.iter_mut().filter(|b| {
                b.revision == request.revision
                    && matches!(b.status, DeliveryStatus::Pending | DeliveryStatus::Failed)
            }) {
                bubble.status = DeliveryStatus::Cancelled;
            }
            request.status = RequestStatus::Queued;
        } else if request.generated_revision == request.revision
            && request
                .bubbles
                .iter()
                .any(|b| b.revision == request.revision)
        {
            for bubble in request
                .bubbles
                .iter_mut()
                .filter(|b| b.revision == request.revision && b.status == DeliveryStatus::Failed)
            {
                bubble.status = DeliveryStatus::Pending;
            }
            request.status = RequestStatus::Ready;
        } else {
            request.status = RequestStatus::Queued;
        }
    }
    store.save(&state)?;
    println!(
        "失败记录已重新排队；未知发送未重试，已发送气泡保持不变。定时任务请用 /task retry、deliver 或 resolve。未连接网络。"
    );
    Ok(())
}

#[cfg(test)]
mod failure_notice_tests {
    use super::*;
    use crate::model::social::{Bubble, ChatMessage, DeliveryStatus, RequestRecord, RequestStatus};
    use std::os::unix::fs::PermissionsExt;

    fn setup() -> (tempfile::TempDir, SocialConfig, Store, GroupState) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = SocialConfig {
            data_dir: directory.path().into(),
            allowed_groups: vec!["test-group".into()],
            ..Default::default()
        };
        let store = Store::new(directory.path(), "test-group").unwrap();
        let mut state = GroupState::default();
        let source = state
            .ingest(
                ChatMessage {
                    id: "source-message".into(),
                    author_id: "member".into(),
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
                revision: 1,
                submitted_revision: 1,
                accepted_revision: 1,
                generated_revision: 1,
                source_seqs: vec![source],
                status: RequestStatus::Generating,
                ..Default::default()
            },
        );
        state
            .fail_generation_with_notice(1, 1, "image_input_failed", "图片读取失败，请重新发送。")
            .unwrap();
        (directory, config, store, state)
    }

    #[test]
    fn explicit_retry_clears_failure_and_regenerates_without_repeating_sent_notices() {
        let (_directory, config, store, mut state) = setup();
        let request = state.requests.get_mut(&1).unwrap();
        request.status = RequestStatus::Failed;
        request.bubbles[0].status = DeliveryStatus::Sent;
        request.bubbles.push(Bubble {
            revision: 1,
            text: "未发提示".into(),
            passive_message_id: "source-message".into(),
            msg_seq: 2,
            status: DeliveryStatus::Failed,
            ..Default::default()
        });
        store.save(&state).unwrap();
        retry_failed(&config, 0).unwrap();
        let mut recovered = store.load().unwrap();
        let request = &recovered.requests[&1];
        assert_eq!(request.failure_code, None);
        assert_eq!(request.status, RequestStatus::Queued);
        assert_eq!(request.bubbles[0].status, DeliveryStatus::Sent);
        assert_eq!(request.bubbles[1].status, DeliveryStatus::Cancelled);
        let revision = recovered.begin_generation(1).unwrap();
        recovered
            .finish_generation(1, revision, vec!["重新生成成功".into()])
            .unwrap();
        let request = &recovered.requests[&1];
        assert_eq!(request.status, RequestStatus::Ready);
        assert_eq!(request.bubbles.len(), 3);
        assert_eq!(request.bubbles[2].msg_seq, 3);
        assert_eq!(request.bubbles[2].status, DeliveryStatus::Pending);
        assert_eq!(request.bubbles[0].status, DeliveryStatus::Sent);
    }

    #[test]
    fn explicit_retry_preserves_unknown_and_cancelled_failure_notices() {
        let (_directory, config, store, mut state) = setup();
        state.requests.get_mut(&1).unwrap().bubbles[0].status = DeliveryStatus::Unknown;
        state.requests.get_mut(&1).unwrap().status = RequestStatus::Failed;
        let mut cancelled = state.requests[&1].clone();
        cancelled.id = 2;
        cancelled.cancelled_revision = Some(1);
        cancelled.bubbles[0].status = DeliveryStatus::Pending;
        state.requests.insert(2, cancelled);
        store.save(&state).unwrap();
        retry_failed(&config, 0).unwrap();
        let recovered = store.load().unwrap();
        assert_eq!(
            recovered.requests[&1].status,
            RequestStatus::UnknownDelivery
        );
        assert_eq!(
            recovered.requests[&1].bubbles[0].status,
            DeliveryStatus::Unknown
        );
        assert_eq!(recovered.requests[&2].status, RequestStatus::Cancelled);
        assert!(
            recovered
                .requests
                .values()
                .all(|request| request.failure_code.as_deref() == Some("image_input_failed"))
        );
    }

    #[test]
    fn chat_recovery_cannot_retry_or_reconcile_scheduled_projections() {
        let (_directory, config, store, mut state) = setup();
        let request = state.requests.get_mut(&1).unwrap();
        request.control = true;
        request.scheduled_run_id = Some(1);
        request.status = RequestStatus::Failed;
        request.bubbles[0].status = DeliveryStatus::Failed;
        store.save(&state).unwrap();
        retry_failed(&config, 0).unwrap();
        let mut restored = store.load().unwrap();
        assert_eq!(restored.requests[&1].status, RequestStatus::Failed);
        assert_eq!(
            restored.requests[&1].bubbles[0].status,
            DeliveryStatus::Failed
        );
        let request = restored.requests.get_mut(&1).unwrap();
        request.status = RequestStatus::UnknownDelivery;
        request.bubbles[0].status = DeliveryStatus::Unknown;
        store.save(&restored).unwrap();
        assert!(resolve_delivery(&config, 0, 1, 0, true).is_err());
        assert_eq!(
            store.load().unwrap().requests[&1].bubbles[0].status,
            DeliveryStatus::Unknown
        );
    }

    #[test]
    fn failure_notices_survive_restart_and_old_records_default_to_no_failure() {
        let (_directory, _config, store, state) = setup();
        store.save(&state).unwrap();
        let mut recovered = store.load().unwrap();
        recovered.recover();
        assert_eq!(recovered.requests[&1].status, RequestStatus::Ready);
        assert_eq!(
            recovered.requests[&1].failure_code.as_deref(),
            Some("image_input_failed")
        );
        assert_eq!(
            recovered.requests[&1].bubbles[0].text,
            "图片读取失败，请重新发送。"
        );
        assert_eq!(recovered.requests[&1].delivered_revision, 0);
        let legacy: RequestRecord = serde_json::from_str(r#"{"id":1,"revision":1}"#).unwrap();
        assert!(legacy.failure_code.is_none());
    }
}
