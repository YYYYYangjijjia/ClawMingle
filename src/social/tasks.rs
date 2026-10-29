//! Conversation-scoped schedules and durable execution/delivery checkpoints.
//! The store is never mounted into a model's workspace. Model calls receive a
//! host-bound identity; neither paths nor recipients are accepted in arguments.
use std::{collections::BTreeMap, fs::OpenOptions, path::PathBuf, sync::Arc};

use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

use super::{
    config::{SocialConfig, private_read_limit},
    store::private_directory,
};
use crate::{
    model::cron::CronKind,
    qq::{GroupMessageAck, GroupSendError},
    util::fs::atomic_write,
};

const MAX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_JOBS: usize = 64;
const MAX_RUNS: usize = 512;

/// Recheck the persisted contributors for each call: native steer can add a
/// member after this turn's lease was created. Mixed-author requests cannot
/// borrow the original owner's management permission.
pub(super) fn may_manage_request(
    history: &super::history::History,
    config: &SocialConfig,
    private: bool,
    target: &str,
    request_id: u64,
) -> bool {
    let check = || -> Result<bool> {
        let reader = history.reader()?;
        let Some(request) = reader.request(request_id)? else {
            return Ok(false);
        };
        if request.status == crate::model::social::RequestStatus::Cancelled
            || request.source_seqs.is_empty()
        {
            return Ok(false);
        }
        for seq in request.source_seqs {
            let Some(super::history::Item::Incoming(record)) = reader.lookup(&format!("M{seq}"))?
            else {
                return Ok(false);
            };
            if !config.is_scope_admin(&record.message.author_id, private, target) {
                return Ok(false);
            }
        }
        Ok(true)
    };
    check().unwrap_or(false)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Schedule {
    Once {
        at_ms: i64,
    },
    Cron {
        expression: String,
        timezone: String,
    },
}

impl Schedule {
    fn next(&self, now: i64) -> Result<Option<i64>> {
        let kind = match self {
            Self::Once { at_ms } => CronKind::OneShot { at: time(*at_ms)? },
            Self::Cron {
                expression,
                timezone,
            } => CronKind::Recurring {
                cron: expression.clone(),
                tz: timezone.clone(),
            },
        };
        Ok(crate::scheduler::next_after(&kind, time(now)?)?.map(|date| date.timestamp_millis()))
    }
}

fn time(ms: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_millis(ms).ok_or_else(|| anyhow::anyhow!("task_time_invalid"))
}

fn error_code(error: &anyhow::Error) -> String {
    let code = error.to_string();
    if (code.starts_with("task_") || code == "tasks_disabled")
        && code.len() <= 96
        && code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        code
    } else {
        "task_operation_failed".into()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct ScheduledJob {
    pub id: u64,
    pub owner: String,
    pub kind: String,
    pub content: String,
    pub schedule: Schedule,
    pub next_due_ms: Option<i64>,
    pub paused: bool,
    pub cancelled: bool,
    pub manual: Option<String>,
    creation_key: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Status {
    Running,
    Ready,
    Sending,
    Sent,
    Deferred,
    Unknown,
    Failed,
    Interrupted,
    Cancelled,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Run {
    pub id: u64,
    pub job_id: u64,
    pub due_ms: i64,
    pub status: Status,
    pub output: Option<String>,
    pub error: Option<String>,
    pub ack_id: Option<String>,
    #[serde(default)]
    pub ack_reference: Option<String>,
    #[serde(default)]
    pub sent_at_ms: Option<i64>,
    #[serde(default)]
    pub platform_time: bool,
    #[serde(default)]
    pub archive_request_id: Option<u64>,
    #[serde(default)]
    pub delivery_sequence: Option<u32>,
    #[serde(default)]
    archive_fingerprint: Option<String>,
    pub trigger: String,
    pub updated_at_ms: i64,
}

#[derive(Clone, Serialize, Deserialize)]
struct State {
    schema: u32,
    private: bool,
    target: String,
    push_enabled: bool,
    next_job: u64,
    next_run: u64,
    #[serde(default)]
    next_delivery_sequence: u32,
    jobs: BTreeMap<u64, ScheduledJob>,
    runs: BTreeMap<u64, Run>,
    #[serde(default)]
    removed_creation_keys: Vec<String>,
}

impl State {
    fn new(private: bool, target: &str) -> Self {
        Self {
            schema: 1,
            private,
            target: target.into(),
            push_enabled: true,
            next_job: 1,
            next_run: 1,
            next_delivery_sequence: 0,
            jobs: BTreeMap::new(),
            runs: BTreeMap::new(),
            removed_creation_keys: Vec::new(),
        }
    }
    fn prune_runs(&mut self) -> Result<()> {
        while self.runs.len() >= MAX_RUNS {
            let removable = self
                .runs
                .iter()
                .find(|(_, run)| {
                    matches!(
                        run.status,
                        Status::Sent | Status::Failed | Status::Cancelled | Status::Interrupted
                    ) && (run.output.is_none()
                        || run.archive_fingerprint.as_deref() == Some(fingerprint(run).as_str()))
                })
                .map(|(id, _)| *id);
            let id = removable.ok_or_else(|| anyhow::anyhow!("task_run_capacity_reached"))?;
            self.runs.remove(&id);
        }
        Ok(())
    }
}

pub(super) struct TaskStore {
    directory: PathBuf,
    private: bool,
    target: String,
}

fn scope(private: bool, target: &str) -> String {
    format!(
        "{}_{:x}",
        if private { "p" } else { "g" },
        Sha1::digest(target.as_bytes())
    )
}

fn fingerprint(run: &Run) -> String {
    format!(
        "{:x}",
        Sha1::digest(
            serde_json::to_vec(&(
                run.status,
                &run.output,
                &run.error,
                &run.ack_id,
                &run.ack_reference,
                run.sent_at_ms,
                run.platform_time
            ))
            .expect("task fingerprint serialization")
        )
    )
}

impl TaskStore {
    pub fn new(config: &SocialConfig, private: bool, target: &str) -> Result<Arc<Self>> {
        ensure!(
            !target.is_empty()
                && target
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
            "task_target_invalid"
        );
        let directory = config.data_dir.join("tasks");
        private_directory(&directory)?;
        Ok(Arc::new(Self {
            directory,
            private,
            target: target.into(),
        }))
    }
    fn path(&self) -> PathBuf {
        self.directory
            .join(format!("{}.json", scope(self.private, &self.target)))
    }
    fn access<T>(&self, write: bool, action: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        private_directory(&self.directory)?;
        let path = self.path();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(0o400000)
            .open(path.with_extension("lock"))?;
        let metadata = lock.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == std::fs::metadata("/proc/self")?.uid()
                && metadata.permissions().mode() & 0o777 == 0o600,
            "task_lock_invalid"
        );
        if write {
            lock.lock_exclusive()?;
        } else {
            FileExt::lock_shared(&lock)?;
        }
        let mut state = if path.exists() {
            serde_json::from_str::<State>(&private_read_limit(&path, MAX_BYTES)?)
                .map_err(|_| anyhow::anyhow!("task_state_invalid"))?
        } else {
            State::new(self.private, &self.target)
        };
        ensure!(
            state.schema == 1 && state.private == self.private && state.target == self.target,
            "task_scope_mismatch"
        );
        let before = if write {
            Some(serde_json::to_string(&state)?)
        } else {
            None
        };
        let result = action(&mut state)?;
        if write {
            let raw = serde_json::to_string(&state)?;
            ensure!(raw.len() as u64 <= MAX_BYTES, "task_state_capacity_reached");
            if before.as_deref() != Some(raw.as_str()) {
                atomic_write(&path, &raw)?;
            }
        }
        Ok(result)
    }
    pub fn recover(&self, now: i64) -> Result<()> {
        self.access(true, |state| {
            for run in state.runs.values_mut() {
                match run.status {
                    Status::Running => {
                        run.status = Status::Interrupted;
                        run.error = Some("process_interrupted_check_workspace_before_retry".into());
                        run.updated_at_ms = now;
                    }
                    Status::Sending => {
                        run.status = Status::Unknown;
                        run.error = Some("delivery_interrupted_reconcile_before_resend".into());
                        run.updated_at_ms = now;
                    }
                    _ => {}
                }
            }
            Ok(())
        })
    }
    pub fn private_targets(config: &SocialConfig) -> Result<Vec<String>> {
        let directory = config.data_dir.join("tasks");
        if !directory.exists() {
            return Ok(Vec::new());
        }
        private_directory(&directory)?;
        let mut targets = Vec::new();
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with("p_") || !name.ends_with(".json") {
                continue;
            }
            let state: State = serde_json::from_str(&private_read_limit(&entry.path(), MAX_BYTES)?)
                .map_err(|_| anyhow::anyhow!("task_state_invalid"))?;
            ensure!(
                state.schema == 1
                    && state.private
                    && name == format!("{}.json", scope(true, &state.target)),
                "task_scope_mismatch"
            );
            if state.target.is_empty()
                || !state
                    .target
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
            {
                anyhow::bail!("task_target_invalid");
            }
            let needs_worker = state.jobs.values().any(|job| {
                !job.cancelled && !job.paused && (job.next_due_ms.is_some() || job.manual.is_some())
            }) || state.runs.values().any(|run| {
                matches!(
                    run.status,
                    Status::Running
                        | Status::Ready
                        | Status::Sending
                        | Status::Deferred
                        | Status::Unknown
                ) || (run.output.is_some()
                    && run.archive_fingerprint.as_deref() != Some(fingerprint(run).as_str()))
            });
            if needs_worker
                && config.private_enabled
                && (config.private_allowed_users.is_empty()
                    || config.private_allowed_users.contains(&state.target))
            {
                targets.push(state.target);
            }
        }
        targets.sort();
        targets.dedup();
        ensure!(
            targets.len() <= config.max_private_sessions,
            "scheduled_private_session_limit_exceeded"
        );
        Ok(targets)
    }
    #[cfg(test)]
    pub fn claim(&self, config: &SocialConfig, now: i64) -> Result<Option<(ScheduledJob, Run)>> {
        self.claim_with_model_admission(config, now, true)
    }

    pub fn claim_with_model_admission(
        &self,
        config: &SocialConfig,
        now: i64,
        model_ready: bool,
    ) -> Result<Option<(ScheduledJob, Run)>> {
        if !config.tasks_enabled {
            return Ok(None);
        }
        self.access(true, |state| {
            if state
                .runs
                .values()
                .any(|r| matches!(r.status, Status::Running | Status::Sending))
            {
                return Ok(None);
            }
            let due = state
                .jobs
                .values()
                .filter(|job| {
                    !job.cancelled
                        && (job.kind == "message" || model_ready)
                        && (job.manual.is_some()
                            || (!job.paused && job.next_due_ms.is_some_and(|due| due <= now)))
                        && !state.runs.values().any(|run| {
                            run.job_id == job.id
                                && matches!(
                                    run.status,
                                    Status::Ready | Status::Deferred | Status::Unknown
                                )
                        })
                })
                .min_by_key(|job| (job.manual.is_none(), job.next_due_ms.unwrap_or(now)))
                .map(|job| job.id);
            let Some(id) = due else { return Ok(None) };
            if !config.is_scope_admin(&state.jobs[&id].owner, self.private, &self.target) {
                state.jobs.get_mut(&id).expect("selected").paused = true;
                return Ok(None);
            }
            state.prune_runs()?;
            let job = state.jobs.get_mut(&id).expect("selected");
            let manual = job.manual.take();
            let due_ms = if manual.is_some() {
                now
            } else {
                job.next_due_ms.expect("due schedule")
            };
            let trigger = manual.unwrap_or_else(|| format!("scheduled:{due_ms}"));
            if job.next_due_ms.is_some_and(|due| due <= now) {
                job.next_due_ms = job.schedule.next(now)?;
            }
            let run = Run {
                id: state.next_run,
                job_id: id,
                due_ms,
                status: Status::Running,
                output: None,
                error: None,
                ack_id: None,
                ack_reference: None,
                sent_at_ms: None,
                platform_time: false,
                archive_request_id: None,
                delivery_sequence: None,
                archive_fingerprint: None,
                trigger,
                updated_at_ms: now,
            };
            state.next_run = state
                .next_run
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("task_counter_exhausted"))?;
            let job = job.clone();
            state.runs.insert(run.id, run.clone());
            Ok(Some((job, run)))
        })
    }
    /// Only a confirmed pre-dispatch refusal may return an occurrence to the queue.
    /// Never replay a task that reached the model or override newer operator intent.
    pub fn defer_unstarted(&self, run_id: u64, now: i64) -> Result<()> {
        self.access(true, |state| {
            let Some(run) = state.runs.get_mut(&run_id) else {
                return Ok(());
            };
            if run.status != Status::Running {
                return Ok(());
            }
            run.status = Status::Interrupted;
            run.error = Some("task_model_admission_deferred".into());
            run.updated_at_ms = now;
            if let Some(job) = state.jobs.get_mut(&run.job_id)
                && !job.cancelled
                && !job.paused
                && job.manual.is_none()
            {
                job.manual = Some(run.trigger.clone());
            }
            Ok(())
        })
    }
    pub fn cancelled(&self, run_id: u64) -> Result<bool> {
        self.access(false, |state| {
            Ok(state
                .runs
                .get(&run_id)
                .is_none_or(|run| run.status != Status::Running))
        })
    }
    /// Revoke only the running occurrence; future occurrences use fresh grants.
    pub fn revoke_capabilities(&self, run_id: u64, now: i64) -> Result<()> {
        self.access(true, |state| {
            if let Some(run) = state.runs.get_mut(&run_id)
                && run.status == Status::Running
            {
                run.status = Status::Cancelled;
                run.error = Some("task_capability_revoked".into());
                run.updated_at_ms = now;
            }
            Ok(())
        })
    }
    pub fn complete(&self, run_id: u64, output: Result<String>, now: i64) -> Result<()> {
        self.access(true, |state| {
            let run = state
                .runs
                .get_mut(&run_id)
                .ok_or_else(|| anyhow::anyhow!("task_run_missing"))?;
            if run.status != Status::Running {
                return Ok(());
            }
            match output {
                Ok(text) if !text.trim().is_empty() && text.chars().count() <= 4000 => {
                    run.output = Some(text);
                    run.status = Status::Ready;
                }
                _ => {
                    run.status = Status::Failed;
                    run.error = Some("task_execution_failed_or_output_invalid".into());
                }
            }
            run.updated_at_ms = now;
            Ok(())
        })
    }
    pub fn take_delivery(&self, config: &SocialConfig, now: i64) -> Result<Option<Run>> {
        if !config.tasks_enabled {
            return Ok(None);
        }
        self.access(true, |state| {
            let push = state.push_enabled;
            for run in state
                .runs
                .values_mut()
                .filter(|run| run.status == Status::Ready)
            {
                let allowed = state.jobs.get(&run.job_id).is_some_and(|job| {
                    !job.cancelled && config.is_scope_admin(&job.owner, self.private, &self.target)
                });
                if !allowed {
                    run.status = Status::Cancelled;
                    continue;
                }
                if !push {
                    run.status = Status::Deferred;
                    run.error = Some("push_disabled".into());
                    continue;
                }
                if run.delivery_sequence.is_none() {
                    state.next_delivery_sequence = state
                        .next_delivery_sequence
                        .max(
                            u32::try_from(state.next_run)
                                .map_err(|_| anyhow::anyhow!("task_message_sequence_exhausted"))?,
                        )
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("task_message_sequence_exhausted"))?;
                    run.delivery_sequence = Some(state.next_delivery_sequence);
                }
                run.status = Status::Sending;
                run.updated_at_ms = now;
                return Ok(Some(run.clone()));
            }
            Ok(None)
        })
    }

    /// Shared same-scope push authorization and durable sequence reservation.
    pub fn push_enabled(&self, config: &SocialConfig) -> Result<bool> {
        if !config.tasks_enabled {
            return Ok(false);
        }
        self.access(false, |state| Ok(state.push_enabled))
    }

    pub fn reserve_result_delivery(&self, config: &SocialConfig) -> Result<Option<u32>> {
        if !config.tasks_enabled {
            return Ok(None);
        }
        self.access(true, |state| {
            if !state.push_enabled {
                return Ok(None);
            }
            state.next_delivery_sequence = state
                .next_delivery_sequence
                .max(
                    u32::try_from(state.next_run)
                        .map_err(|_| anyhow::anyhow!("task_message_sequence_exhausted"))?,
                )
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("task_message_sequence_exhausted"))?;
            Ok(Some(state.next_delivery_sequence))
        })
    }

    /// Scheduling priority probe; it does not claim execution or delivery.
    pub fn has_due_work(&self, config: &SocialConfig, now: i64) -> Result<bool> {
        if !config.tasks_enabled {
            return Ok(false);
        }
        self.access(false, |state| {
            Ok(
                state.push_enabled && state.runs.values().any(|run| run.status == Status::Ready)
                    || state.jobs.values().any(|job| {
                        !job.cancelled
                            && config.is_scope_admin(&job.owner, self.private, &self.target)
                            && (job.manual.is_some()
                                || (!job.paused && job.next_due_ms.is_some_and(|due| due <= now)))
                            && !state.runs.values().any(|run| {
                                run.job_id == job.id
                                    && matches!(
                                        run.status,
                                        Status::Ready | Status::Deferred | Status::Unknown
                                    )
                            })
                    }),
            )
        })
    }
    pub fn finish_delivery(
        &self,
        run_id: u64,
        result: std::result::Result<GroupMessageAck, GroupSendError>,
        now: i64,
    ) -> Result<()> {
        self.access(true, |state| {
            let run = state
                .runs
                .get_mut(&run_id)
                .ok_or_else(|| anyhow::anyhow!("task_run_missing"))?;
            ensure!(run.status == Status::Sending, "task_delivery_not_sending");
            match result {
                Ok(ack) => {
                    run.status = Status::Sent;
                    run.platform_time = ack.timestamp.is_some();
                    run.ack_reference = ack.ref_idx().map(str::to_owned);
                    run.sent_at_ms = Some(
                        ack.timestamp
                            .map(|date| date.timestamp_millis())
                            .unwrap_or(now),
                    );
                    run.ack_id = Some(ack.id);
                    run.error = None;
                }
                Err(GroupSendError::Unknown { .. }) => {
                    run.status = Status::Unknown;
                    run.error = Some("delivery_unknown_do_not_resend".into());
                }
                Err(_) => {
                    run.status = Status::Deferred;
                    run.error = Some("proactive_unavailable_explicit_deliver_required".into());
                }
            }
            run.updated_at_ms = now;
            Ok(())
        })
    }
    pub fn archive_records(&self) -> Result<Vec<(ScheduledJob, Run)>> {
        self.access(false, |state| {
            Ok(state
                .runs
                .values()
                .filter(|run| {
                    run.output.is_some()
                        && run.archive_fingerprint.as_deref() != Some(fingerprint(run).as_str())
                })
                .filter_map(|run| {
                    state
                        .jobs
                        .get(&run.job_id)
                        .map(|job| (job.clone(), run.clone()))
                })
                .collect())
        })
    }
    pub fn mark_archived(&self, archived: &Run, request_id: u64) -> Result<()> {
        self.access(true, |state| {
            let run = state
                .runs
                .get_mut(&archived.id)
                .ok_or_else(|| anyhow::anyhow!("task_run_missing"))?;
            run.archive_request_id = Some(request_id);
            if fingerprint(run) == fingerprint(archived) {
                run.archive_fingerprint = Some(fingerprint(run));
            }
            Ok(())
        })
    }
    pub fn call(
        &self,
        args: &str,
        author: &str,
        administrator: bool,
        source_key: &str,
        enabled: bool,
        now: i64,
    ) -> Value {
        match self.operation(args, author, administrator, source_key, enabled, now) {
            Ok(value) => json!({"ok":true,"result":value}),
            Err(error) => {
                json!({"ok":false,"error":{"code":error_code(&error),"message":"任务操作未完成；请核对权限、时间、ID或任务状态。"}})
            }
        }
    }
    fn operation(
        &self,
        args: &str,
        author: &str,
        administrator: bool,
        source_key: &str,
        enabled: bool,
        now: i64,
    ) -> Result<Value> {
        ensure!(args.len() <= 16384, "task_arguments_too_large");
        let operation: Operation =
            serde_json::from_str(args).map_err(|_| anyhow::anyhow!("task_arguments_invalid"))?;
        let read = matches!(operation.action.as_str(), "list" | "status");
        ensure!(read || enabled, "tasks_disabled");
        ensure!(read || administrator, "task_admin_required");
        self.access(!read, |state| {
            apply(state, operation, author, source_key, now)
        })
    }
    pub fn command(
        &self,
        text: &str,
        author: &str,
        administrator: bool,
        source_key: &str,
        enabled: bool,
        now: i64,
    ) -> Option<String> {
        if text.trim() != "/task" && !text.trim_start().starts_with("/task ") {
            return None;
        }
        let result = command_operation(text).and_then(|operation| {
            self.operation(
                &serde_json::to_string(&operation)?,
                author,
                administrator,
                source_key,
                enabled,
                now,
            )
        });
        let reply = match result {
            Ok(result) => format!(
                "任务操作结果（时间均含时区）：\n{}",
                serde_json::to_string_pretty(&result).unwrap_or_default()
            ),
            Err(error) => format!("任务操作未完成：{}\n{}", error_code(&error), HELP),
        };
        Some(if reply.chars().count() > 4000 {
            format!(
                "{}\n…结果较多；用 /task status J编号 或 T编号 查看具体任务。",
                reply.chars().take(3900).collect::<String>()
            )
        } else {
            reply
        })
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Operation {
    action: String,
    job_id: Option<u64>,
    run_id: Option<u64>,
    kind: Option<String>,
    content: Option<String>,
    at: Option<String>,
    cron: Option<String>,
    timezone: Option<String>,
    enabled: Option<bool>,
    was_sent: Option<bool>,
}

fn apply(
    state: &mut State,
    operation: Operation,
    author: &str,
    source_key: &str,
    now: i64,
) -> Result<Value> {
    let job_id = operation.job_id;
    let run_id = operation.run_id;
    match operation.action.as_str() {
        "list" | "status" => {
            let jobs=state.jobs.values().filter(|job|job_id.is_none_or(|id|id==job.id)&&run_id.is_none()).map(|job|json!({"id":format!("J{}",job.id),"kind":job.kind,"content":job.content.chars().take(if job_id.is_some(){4000}else{120}).collect::<String>(),"schedule":job.schedule,"next_at":job.next_due_ms.and_then(|ms|time(ms).ok()).map(|time|time.to_rfc3339()),"paused":job.paused,"cancelled":job.cancelled,"manual_pending":job.manual.is_some()})).collect::<Vec<_>>();
            let runs=state.runs.values().rev().filter(|run|job_id.is_none_or(|id|id==run.job_id)&&run_id.is_none_or(|id|id==run.id)).take(if job_id.is_some()||run_id.is_some(){5}else{20}).map(|run|json!({"id":format!("T{}",run.id),"job":format!("J{}",run.job_id),"due_at":time(run.due_ms).ok().map(|t|t.to_rfc3339()),"status":run.status,"output":if job_id.is_some()||run_id.is_some(){run.output.clone()}else{None},"error":run.error,"acknowledged":run.ack_id.is_some(),"history_ref":run.archive_request_id.map(|id|format!("B{id}.0"))})).collect::<Vec<_>>();
            Ok(json!({"push_enabled":state.push_enabled,"jobs":jobs,"recent_runs":runs}))
        }
        "create" => {
            let kind = operation.kind.unwrap_or_else(|| "message".into());
            ensure!(
                matches!(kind.as_str(), "message" | "agent"),
                "task_kind_invalid"
            );
            let content = operation
                .content
                .ok_or_else(|| anyhow::anyhow!("task_content_required"))?;
            ensure!(
                !content.trim().is_empty() && content.chars().count() <= 4000,
                "task_content_invalid"
            );
            let schedule = match (operation.at, operation.cron, operation.timezone) {
                (Some(at), None, None) => {
                    let at = DateTime::parse_from_rfc3339(&at)
                        .map_err(|_| anyhow::anyhow!("task_time_requires_rfc3339_offset"))?
                        .timestamp_millis();
                    ensure!(at > now, "task_time_must_be_future");
                    Schedule::Once { at_ms: at }
                }
                (None, Some(expression), Some(timezone)) => {
                    ensure!(
                        expression.len() <= 128 && timezone.len() <= 64,
                        "task_schedule_invalid"
                    );
                    let schedule = Schedule::Cron {
                        expression,
                        timezone,
                    };
                    ensure!(schedule.next(now)?.is_some(), "task_schedule_has_no_future");
                    schedule
                }
                _ => anyhow::bail!("task_requires_at_or_cron_and_timezone"),
            };
            let creation_key = format!(
                "{}:{:x}",
                source_key,
                Sha1::digest(serde_json::to_vec(
                    &json!({"kind":kind,"content":content,"schedule":schedule})
                )?)
            );
            ensure!(
                !state.removed_creation_keys.contains(&creation_key),
                "task_creation_was_removed"
            );
            if let Some(job) = state
                .jobs
                .values()
                .find(|job| job.creation_key == creation_key)
            {
                return Ok(json!({"job_id":format!("J{}",job.id),"already_created":true}));
            }
            ensure!(state.jobs.len() < MAX_JOBS, "task_job_capacity_reached");
            let id = state.next_job;
            state.next_job = state
                .next_job
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("task_counter_exhausted"))?;
            let next_due_ms = schedule.next(now)?;
            state.jobs.insert(
                id,
                ScheduledJob {
                    id,
                    owner: author.into(),
                    kind,
                    content,
                    schedule,
                    next_due_ms,
                    paused: false,
                    cancelled: false,
                    manual: None,
                    creation_key,
                    created_at_ms: now,
                },
            );
            Ok(
                json!({"job_id":format!("J{id}"),"next_at":next_due_ms.and_then(|t|time(t).ok()).map(|t|t.to_rfc3339()),"push_enabled":state.push_enabled,"delivery_notice":if state.push_enabled {"主动投递已开启；真实可达性以QQ ACK为准。"}else{"主动投递尚未开启，任务会执行并保留结果；要按时收到QQ消息，请 /task push on。"}}),
            )
        }
        "push" => {
            let enabled = operation
                .enabled
                .ok_or_else(|| anyhow::anyhow!("task_push_value_required"))?;
            state.push_enabled = enabled;
            if enabled {
                for run in state.runs.values_mut().filter(|run| {
                    run.status == Status::Deferred && run.error.as_deref() == Some("push_disabled")
                }) {
                    run.status = Status::Ready;
                    run.error = None;
                }
            }
            Ok(json!({"push_enabled":enabled}))
        }
        "remove" => {
            let id = job_id.ok_or_else(|| anyhow::anyhow!("task_job_id_required"))?;
            let job = state
                .jobs
                .get(&id)
                .ok_or_else(|| anyhow::anyhow!("task_job_missing"))?;
            ensure!(
                job.cancelled
                    || (matches!(job.schedule, Schedule::Once { .. })
                        && job.next_due_ms.is_none()
                        && job.manual.is_none()),
                "task_remove_requires_terminal_job"
            );
            ensure!(
                state
                    .runs
                    .values()
                    .filter(|run| run.job_id == id)
                    .all(|run| !matches!(
                        run.status,
                        Status::Running
                            | Status::Sending
                            | Status::Ready
                            | Status::Deferred
                            | Status::Unknown
                    ) && (run.output.is_none()
                        || run.archive_fingerprint.as_deref() == Some(fingerprint(run).as_str()))),
                "task_remove_requires_settled_archived_runs"
            );
            let removed = state.jobs.remove(&id).expect("validated job");
            state.removed_creation_keys.push(removed.creation_key);
            if state.removed_creation_keys.len() > 512 {
                state.removed_creation_keys.remove(0);
            }
            Ok(json!({"removed_job_id":format!("J{id}"),"run_history_preserved":true}))
        }
        "pause" | "resume" | "cancel" | "run" => {
            let id = job_id.ok_or_else(|| anyhow::anyhow!("task_job_id_required"))?;
            let job = state
                .jobs
                .get_mut(&id)
                .ok_or_else(|| anyhow::anyhow!("task_job_missing"))?;
            ensure!(
                !job.cancelled || operation.action == "cancel",
                "task_job_cancelled"
            );
            match operation.action.as_str() {
                "pause" => job.paused = true,
                "resume" => {
                    job.paused = false;
                    if matches!(job.schedule, Schedule::Cron { .. }) {
                        job.next_due_ms = job.schedule.next(now)?;
                    }
                }
                "cancel" => {
                    job.cancelled = true;
                    job.manual = None;
                    job.next_due_ms = None;
                    for run in state.runs.values_mut().filter(|run| run.job_id == id) {
                        if !matches!(run.status, Status::Sent | Status::Sending | Status::Unknown) {
                            run.status = Status::Cancelled;
                            run.updated_at_ms = now;
                        }
                    }
                }
                "run" => {
                    let key = format!("manual:{source_key}");
                    if !state
                        .runs
                        .values()
                        .any(|run| run.job_id == id && run.trigger == key)
                    {
                        ensure!(
                            job.manual.as_ref().is_none_or(|old| old == &key),
                            "task_manual_run_already_pending"
                        );
                        job.manual = Some(key);
                    }
                }
                _ => unreachable!(),
            }
            Ok(json!({"job_id":format!("J{id}"),"action":operation.action}))
        }
        "retry" | "deliver" | "resolve" => {
            let id = run_id.ok_or_else(|| anyhow::anyhow!("task_run_id_required"))?;
            if operation.action == "retry"
                && state
                    .runs
                    .values()
                    .any(|run| run.trigger == format!("retry:{id}:{source_key}"))
            {
                return Ok(json!({"run_id":format!("T{id}"),"already_requested":true}));
            }
            let run = state
                .runs
                .get_mut(&id)
                .ok_or_else(|| anyhow::anyhow!("task_run_missing"))?;
            match operation.action.as_str() {
                "retry" => {
                    ensure!(
                        matches!(run.status, Status::Failed | Status::Interrupted),
                        "task_retry_requires_failed_or_interrupted"
                    );
                    let job = state
                        .jobs
                        .get_mut(&run.job_id)
                        .ok_or_else(|| anyhow::anyhow!("task_job_missing"))?;
                    ensure!(
                        !job.cancelled && job.manual.is_none(),
                        "task_job_cancelled_or_pending"
                    );
                    job.manual = Some(format!("retry:{id}:{source_key}"));
                }
                "deliver" => {
                    ensure!(
                        run.status == Status::Deferred && run.output.is_some(),
                        "task_delivery_not_deferred"
                    );
                    run.status = Status::Ready;
                    run.error = None;
                }
                "resolve" => {
                    ensure!(run.status == Status::Unknown, "task_delivery_not_unknown");
                    let sent = operation
                        .was_sent
                        .ok_or_else(|| anyhow::anyhow!("task_observation_required"))?;
                    let cancelled = state.jobs.get(&run.job_id).is_none_or(|job| job.cancelled);
                    run.status = if sent {
                        Status::Sent
                    } else if cancelled {
                        Status::Cancelled
                    } else {
                        Status::Ready
                    };
                    run.error = None;
                }
                _ => unreachable!(),
            }
            run.updated_at_ms = now;
            Ok(json!({"run_id":format!("T{id}"),"action":operation.action}))
        }
        _ => anyhow::bail!("task_action_unknown"),
    }
}

pub(super) const HELP: &str = "/task once 2026-12-01T09:00:00+08:00 提醒内容\n/task agent-once 时间 带工具的任务\n/task cron Asia/Shanghai 0 0 9 * * * | 提醒内容\n/task agent-cron 时区 cron表达式 | 任务\n/task list；/task status J1；/task pause|resume|cancel|run|remove J1\n/task retry T1（会再次执行，先核对工作区）；/task deliver T1（仅重投递）\n/task resolve T1 sent|unsent（核对未知发送）；/task push on|off\n修改操作仅当前对话管理员；主动推送默认开启，可用 /task push off 关闭；任务仍需明确创建。";

fn command_operation(text: &str) -> Result<Operation> {
    let args = text.trim().strip_prefix("/task").unwrap_or_default().trim();
    let (action, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let rest = rest.trim();
    let mut operation = Operation {
        action: action.into(),
        ..Default::default()
    };
    match action {
        "" => operation.action = "list".into(),
        "list" => ensure!(rest.is_empty(), "task_command_invalid"),
        "status" => {
            if !rest.is_empty() {
                if rest.starts_with('T') {
                    operation.run_id = Some(parse_id(rest, 'T')?);
                } else {
                    operation.job_id = Some(parse_id(rest, 'J')?);
                }
            }
        }
        "once" | "agent-once" => {
            let (at, content) = rest
                .split_once(char::is_whitespace)
                .ok_or_else(|| anyhow::anyhow!("task_command_invalid"))?;
            operation.action = "create".into();
            operation.kind = Some(if action == "once" { "message" } else { "agent" }.into());
            operation.at = Some(at.into());
            operation.content = Some(content.trim().into());
        }
        "cron" | "agent-cron" => {
            let (schedule, content) = rest
                .split_once('|')
                .ok_or_else(|| anyhow::anyhow!("task_command_requires_separator"))?;
            let (timezone, expression) = schedule
                .trim()
                .split_once(char::is_whitespace)
                .ok_or_else(|| anyhow::anyhow!("task_timezone_required"))?;
            operation.action = "create".into();
            operation.kind = Some(if action == "cron" { "message" } else { "agent" }.into());
            operation.timezone = Some(timezone.into());
            operation.cron = Some(expression.trim().into());
            operation.content = Some(content.trim().into());
        }
        "pause" | "resume" | "cancel" | "run" | "remove" => {
            operation.job_id = Some(parse_id(rest, 'J')?)
        }
        "retry" | "deliver" => operation.run_id = Some(parse_id(rest, 'T')?),
        "resolve" => {
            let (id, sent) = rest
                .split_once(char::is_whitespace)
                .ok_or_else(|| anyhow::anyhow!("task_observation_required"))?;
            operation.run_id = Some(parse_id(id, 'T')?);
            operation.was_sent = Some(match sent.trim() {
                "sent" => true,
                "unsent" => false,
                _ => anyhow::bail!("task_observation_required"),
            });
        }
        "push" => {
            operation.enabled = Some(match rest {
                "on" => true,
                "off" => false,
                _ => anyhow::bail!("task_push_value_required"),
            })
        }
        _ => anyhow::bail!("task_action_unknown"),
    }
    Ok(operation)
}
fn parse_id(value: &str, prefix: char) -> Result<u64> {
    value
        .strip_prefix(prefix)
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|id| *id > 0)
        .ok_or_else(|| anyhow::anyhow!("task_id_invalid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steered_contributors_cannot_inherit_the_original_admins_task_permission() {
        use crate::model::social::{ChatMessage, GroupState, RequestRecord, RequestStatus};
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = SocialConfig {
            command_admins: vec!["admin".into()],
            ..Default::default()
        };
        let store = super::super::store::Store::new(directory.path(), "test-group").unwrap();
        let mut state = GroupState::default();
        let first = state
            .ingest(
                ChatMessage {
                    id: "first".into(),
                    author_id: "admin".into(),
                    ..Default::default()
                },
                500,
            )
            .unwrap()
            .unwrap();
        state.requests.insert(
            1,
            RequestRecord {
                id: 1,
                owner_id: "admin".into(),
                source_seqs: vec![first],
                status: RequestStatus::Generating,
                ..Default::default()
            },
        );
        store.save(&state).unwrap();
        assert!(may_manage_request(
            &store.history,
            &config,
            false,
            "test-group",
            1
        ));
        let next = state
            .ingest(
                ChatMessage {
                    id: "append".into(),
                    author_id: "member".into(),
                    ..Default::default()
                },
                500,
            )
            .unwrap()
            .unwrap();
        state.requests.get_mut(&1).unwrap().source_seqs.push(next);
        store.save(&state).unwrap();
        assert!(!may_manage_request(
            &store.history,
            &config,
            false,
            "test-group",
            1
        ));
        assert!(!may_manage_request(
            &store.history,
            &config,
            false,
            "test-group",
            999
        ));
        let mut both = config.clone();
        both.command_admins.push("member".into());
        assert!(may_manage_request(
            &store.history,
            &both,
            false,
            "test-group",
            1
        ));
        state.requests.get_mut(&1).unwrap().status = RequestStatus::Cancelled;
        store.save(&state).unwrap();
        assert!(!may_manage_request(
            &store.history,
            &both,
            false,
            "test-group",
            1
        ));
    }
    fn fixture() -> (tempfile::TempDir, SocialConfig, Arc<TaskStore>) {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        private_directory(dir.path()).unwrap();
        let config = SocialConfig {
            data_dir: dir.path().into(),
            command_admins: vec!["admin".into()],
            ..Default::default()
        };
        let store = TaskStore::new(&config, false, "group").unwrap();
        (dir, config, store)
    }
    fn op(store: &TaskStore, args: Value, key: &str, now: i64) -> Value {
        store.call(&args.to_string(), "admin", true, key, true, now)
    }
    fn create(store: &TaskStore, kind: &str) -> Value {
        op(
            store,
            json!({"action":"create","kind":kind,"content":"hello","at":"2026-12-01T09:00:00+08:00"}),
            "source",
            0,
        )
    }
    #[test]
    fn admission_defers_unstarted_occurrence_without_losing_it_or_replaying_success() {
        let (_dir, config, store) = fixture();
        assert_eq!(create(&store, "agent")["ok"], true);
        let now = 1_900_000_000_000;
        assert!(
            store
                .claim_with_model_admission(&config, now, false)
                .unwrap()
                .is_none()
        );
        let (_, run) = store.claim(&config, now).unwrap().unwrap();
        store.defer_unstarted(run.id, now + 1).unwrap();
        let (_, retry) = store.claim(&config, now + 2).unwrap().unwrap();
        assert_eq!(retry.trigger, run.trigger);
        store
            .complete(retry.id, Ok("done".into()), now + 3)
            .unwrap();
        store.defer_unstarted(retry.id, now + 4).unwrap();
        assert!(store.claim(&config, now + 5).unwrap().is_none());
    }
    #[test]
    fn admission_does_not_block_fixed_reminders_or_revive_cancelled_jobs() {
        let (_dir, config, store) = fixture();
        assert_eq!(create(&store, "message")["ok"], true);
        let now = 1_900_000_000_000;
        let (_, run) = store
            .claim_with_model_admission(&config, now, false)
            .unwrap()
            .unwrap();
        store
            .access(true, |state| {
                state.jobs.get_mut(&run.job_id).unwrap().cancelled = true;
                Ok(())
            })
            .unwrap();
        store.defer_unstarted(run.id, now + 1).unwrap();
        assert!(store.claim(&config, now + 2).unwrap().is_none());
    }
    #[test]
    fn admin_and_scope_are_host_bound_and_creation_is_idempotent() {
        let (_dir, config, store) = fixture();
        let args = json!({"action":"create","content":"hello","at":"2026-12-01T09:00:00+08:00"})
            .to_string();
        assert_eq!(
            store.call(&args, "member", false, "source", true, 0)["ok"],
            false
        );
        assert_eq!(create(&store, "message")["result"]["job_id"], "J1");
        assert_eq!(create(&store, "message")["result"]["already_created"], true);
        let other = TaskStore::new(&config, false, "other").unwrap();
        assert!(other.claim(&config, i64::MAX / 2).unwrap().is_none());
        assert!(
            !op(&store, json!({"action":"list","target":"other"}), "x", 0)["ok"]
                .as_bool()
                .unwrap()
        );
    }
    #[test]
    fn claim_is_durable_coalesces_missed_runs_and_checks_current_admin() {
        let (_dir, config, store) = fixture();
        assert_eq!(
            op(
                &store,
                json!({"action":"create","content":"x","cron":"0 * * * * *","timezone":"Asia/Shanghai"}),
                "source",
                0
            )["ok"],
            true
        );
        let (_, run) = store.claim(&config, 3600000).unwrap().unwrap();
        assert!(store.claim(&config, 3600000).unwrap().is_none());
        store.recover(3600001).unwrap();
        assert_eq!(
            store
                .access(false, |state| Ok(state.runs[&run.id].status))
                .unwrap(),
            Status::Interrupted
        );
        let mut revoked = config;
        revoked.command_admins.clear();
        assert!(store.claim(&revoked, 7200000).unwrap().is_none());
    }

    #[test]
    fn explicit_run_precedes_an_already_due_recurring_job() {
        let (_dir, config, store) = fixture();
        assert_eq!(create(&store, "message")["ok"], true);
        assert_eq!(
            op(
                &store,
                json!({"action":"create","content":"periodic","cron":"0 * * * * *","timezone":"Asia/Shanghai"}),
                "periodic",
                0
            )["ok"],
            true
        );
        assert_eq!(
            op(
                &store,
                json!({"action":"run","job_id":1}),
                "run-now",
                3600000
            )["ok"],
            true
        );
        let (job, _) = store.claim(&config, 3600000).unwrap().unwrap();
        assert_eq!(job.id, 1);
    }
    #[test]
    fn push_defaults_on_and_explicit_off_survives_reload() {
        let (_dir, config, store) = fixture();
        assert!(store.push_enabled(&config).unwrap());
        op(&store, json!({"action":"push","enabled":false}), "off", 1);
        assert!(!store.push_enabled(&config).unwrap());
        assert!(store.reserve_result_delivery(&config).unwrap().is_none());
        op(&store, json!({"action":"push","enabled":true}), "on", 2);
        assert!(store.reserve_result_delivery(&config).unwrap().is_some());
        let mut disabled = config;
        disabled.tasks_enabled = false;
        assert!(!store.push_enabled(&disabled).unwrap());
    }

    #[test]
    fn execution_and_delivery_are_separate_unknown_is_never_resent() {
        let (_dir, config, store) = fixture();
        op(
            &store,
            json!({"action":"push","enabled":false}),
            "disable",
            0,
        );
        create(&store, "message");
        let (_, run) = store.claim(&config, 1800000000000).unwrap().unwrap();
        store.complete(run.id, Ok("result".into()), 1).unwrap();
        assert!(store.take_delivery(&config, 2).unwrap().is_none());
        assert_eq!(
            op(&store, json!({"action":"push","enabled":true}), "push", 3)["ok"],
            true
        );
        store.take_delivery(&config, 4).unwrap().unwrap();
        store.recover(5).unwrap();
        assert!(store.take_delivery(&config, 6).unwrap().is_none());
        assert_eq!(
            op(
                &store,
                json!({"action":"deliver","run_id":run.id}),
                "deliver",
                7
            )["ok"],
            false
        );
        assert_eq!(
            op(
                &store,
                json!({"action":"resolve","run_id":run.id,"was_sent":false}),
                "resolve",
                8
            )["ok"],
            true
        );
        assert_eq!(
            store
                .take_delivery(&config, 9)
                .unwrap()
                .unwrap()
                .output
                .as_deref(),
            Some("result")
        );
    }
    #[test]
    fn capability_revocation_cancels_occurrence_without_failure_or_late_delivery() {
        let (_dir, config, store) = fixture();
        create(&store, "agent");
        op(&store, json!({"action":"run","job_id":1}), "manual", 1);
        let (_, run) = store.claim(&config, 2).unwrap().unwrap();
        store.revoke_capabilities(run.id, 3).unwrap();
        store
            .complete(run.id, Ok("stale output".into()), 4)
            .unwrap();
        store
            .complete(run.id, Err(anyhow::anyhow!("cancelled")), 5)
            .unwrap();
        store
            .access(false, |state| {
                assert_eq!(state.runs[&run.id].status, Status::Cancelled);
                assert!(state.runs[&run.id].output.is_none());
                assert!(!state.jobs[&1].cancelled);
                Ok(())
            })
            .unwrap();
        assert!(store.take_delivery(&config, 6).unwrap().is_none());
    }
    #[test]
    fn cancelling_running_job_prevents_result_publication_and_manual_run_is_deduped() {
        let (_dir, config, store) = fixture();
        create(&store, "agent");
        op(&store, json!({"action":"run","job_id":1}), "manual", 1);
        let (_, run) = store.claim(&config, 2).unwrap().unwrap();
        op(&store, json!({"action":"run","job_id":1}), "manual", 3);
        assert!(
            store
                .access(false, |state| Ok(state.jobs[&1].manual.is_none()))
                .unwrap()
        );
        op(&store, json!({"action":"cancel","job_id":1}), "cancel", 4);
        assert!(store.cancelled(run.id).unwrap());
        store
            .complete(run.id, Ok("must not publish".into()), 5)
            .unwrap();
        assert!(store.take_delivery(&config, 6).unwrap().is_none());
    }

    #[test]
    fn priority_probe_does_not_claim_a_due_task_or_delivery() {
        let (_dir, config, store) = fixture();
        assert!(!store.has_due_work(&config, 0).unwrap());
        create(&store, "agent");
        op(&store, json!({"action":"run","job_id":1}), "manual", 1);
        assert!(store.has_due_work(&config, 2).unwrap());
        assert!(
            store
                .access(false, |state| Ok(state.runs.is_empty()))
                .unwrap()
        );
        let (_, run) = store.claim(&config, 2).unwrap().unwrap();
        store.complete(run.id, Ok("result".into()), 3).unwrap();
        op(&store, json!({"action":"push","enabled":true}), "push", 4);
        assert!(store.has_due_work(&config, 5).unwrap());
        assert_eq!(
            store
                .access(false, |state| Ok(state.runs[&run.id].status))
                .unwrap(),
            Status::Ready
        );
    }
    #[test]
    fn command_parser_requires_explicit_offset_and_cron_timezone() {
        assert!(command_operation("/task once 2026-12-01T09:00:00+08:00 hello world").is_ok());
        assert!(command_operation("/task cron Asia/Shanghai 0 0 9 * * * | hello").is_ok());
        let (_dir, _config, store) = fixture();
        assert_eq!(
            op(
                &store,
                json!({"action":"create","content":"hello","at":"2026-12-01T09:00:00"}),
                "bad",
                0
            )["ok"],
            false
        );
        assert_eq!(
            op(
                &store,
                json!({"action":"create","content":"hello","cron":"0 * * * * *"}),
                "bad",
                0
            )["ok"],
            false
        );
    }
    #[test]
    fn private_schedules_are_discovered_at_restart_and_pause_does_not_cancel_run() {
        let (_dir, config, _store) = fixture();
        let store = TaskStore::new(&config, true, "user").unwrap();
        create(&store, "agent");
        assert_eq!(TaskStore::private_targets(&config).unwrap(), vec!["user"]);
        let (_, run) = store.claim(&config, 1800000000000).unwrap().unwrap();
        op(&store, json!({"action":"pause","job_id":1}), "pause", 1);
        assert!(!store.cancelled(run.id).unwrap());
    }

    #[test]
    fn deferred_delivery_never_reexecutes_and_completed_private_history_does_not_reserve_a_worker()
    {
        let (_dir, config, _store) = fixture();
        let store = TaskStore::new(&config, true, "user").unwrap();
        create(&store, "agent");
        let (_, run) = store.claim(&config, 1800000000000).unwrap().unwrap();
        store
            .complete(run.id, Ok("already generated".into()), 1)
            .unwrap();
        op(&store, json!({"action":"push","enabled":true}), "push", 2);
        store.take_delivery(&config, 3).unwrap().unwrap();
        store
            .finish_delivery(
                run.id,
                Err(GroupSendError::Rejected {
                    status: 403,
                    code: None,
                }),
                4,
            )
            .unwrap();
        assert!(store.claim(&config, 1800000000001).unwrap().is_none());
        assert!(store.take_delivery(&config, 5).unwrap().is_none());
        op(
            &store,
            json!({"action":"deliver","run_id":run.id}),
            "deliver",
            6,
        );
        assert_eq!(
            store
                .take_delivery(&config, 7)
                .unwrap()
                .unwrap()
                .output
                .as_deref(),
            Some("already generated")
        );
        store
            .finish_delivery(
                run.id,
                Ok(GroupMessageAck {
                    id: "sent".into(),
                    timestamp: None,
                    ext_info: None,
                }),
                8,
            )
            .unwrap();
        let pending = store.archive_records().unwrap();
        assert_eq!(pending.len(), 1);
        store.mark_archived(&pending[0].1, 100).unwrap();
        assert!(TaskStore::private_targets(&config).unwrap().is_empty());
    }

    #[test]
    fn explicit_scope_admins_override_default_and_an_empty_list_denies() {
        let (_dir, mut config, store) = fixture();
        config.scope_admins.insert("group:group".into(), Vec::new());
        assert!(!config.is_scope_admin("admin", false, "group"));
        assert!(config.is_scope_admin("admin", false, "other"));
        create(&store, "message");
        assert!(store.claim(&config, 1800000000000).unwrap().is_none());
    }

    #[test]
    fn removing_only_settled_archived_jobs_releases_definition_capacity_and_keeps_runs() {
        let (_dir, config, store) = fixture();
        create(&store, "message");
        let (_, run) = store.claim(&config, 1800000000000).unwrap().unwrap();
        assert_eq!(
            op(&store, json!({"action":"remove","job_id":1}), "remove", 0)["ok"],
            false
        );
        store.complete(run.id, Ok("result".into()), 1).unwrap();
        op(&store, json!({"action":"push","enabled":true}), "push", 2);
        store.take_delivery(&config, 3).unwrap().unwrap();
        store
            .finish_delivery(
                run.id,
                Ok(GroupMessageAck {
                    id: "ack".into(),
                    timestamp: None,
                    ext_info: None,
                }),
                4,
            )
            .unwrap();
        assert_eq!(
            op(&store, json!({"action":"remove","job_id":1}), "remove", 5)["ok"],
            false
        );
        let pending = store.archive_records().unwrap();
        store.mark_archived(&pending[0].1, 100).unwrap();
        assert_eq!(
            op(&store, json!({"action":"remove","job_id":1}), "remove", 6)["ok"],
            true
        );
        assert!(
            store
                .access(false, |state| Ok(
                    state.jobs.is_empty() && state.runs.contains_key(&run.id)
                ))
                .unwrap()
        );
        assert_eq!(
            create(&store, "message")["ok"],
            false,
            "replay cannot recreate a removed definition"
        );
        assert_eq!(
            op(
                &store,
                json!({"action":"create","content":"new task","at":"2026-12-01T09:00:00+08:00"}),
                "new-source",
                0
            )["result"]["job_id"],
            "J2"
        );
    }
}
