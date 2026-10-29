//! Runs a single Codex turn over a shared `AppServerSupervisor`.
//!
//! Drives `thread/start` or `thread/resume` + `turn/start`, then consumes
//! notifications until `turn/completed` (or failure). Translates each
//! notification into the existing [`ExecutionUpdate`] stream consumed by
//! `PassiveTurnEmitter` so the QQ output is byte-identical to the previous
//! `codex exec --json` pipeline.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use serde_json::Value as JsonValue;
use tokio::{
    sync::{Mutex, broadcast, mpsc, oneshot},
    time::{sleep, timeout},
};
use tracing::{debug, info, warn};

use crate::{
    codex::{
        types::{CompactRequest, ExecutionRequest, ExecutionResult, ExecutionUpdate},
        types::{TokenUsage, TokenUsageInfo},
    },
    model::settings::{ContextMode, ReasoningEffort, ServiceTier},
};

use super::{
    client::{JsonRpcClient, Notification},
    events::{self as translator, TurnOutcome, TurnState},
    protocol::{
        ApprovalPolicy, ApprovalsReviewer, CollaborationMode, CollaborationSettings,
        CompactedNotification, ItemNotification, ModeKind, ModelReroutedNotification, SandboxMode,
        SandboxPolicy, ThreadCompactStartParams, ThreadCompactStartResponse, ThreadResumeParams,
        ThreadResumeResponse, ThreadStartParams, ThreadStartResponse, ThreadUnsubscribeParams,
        ThreadUnsubscribeResponse, ThreadUnsubscribeStatus, TokenUsageUpdatedNotification,
        TurnCompletedNotification, TurnInputItem, TurnInterruptParams, TurnInterruptResponse,
        TurnStartParams, TurnStartResponse, TurnSteerParams, TurnSteerResponse, method,
    },
    supervisor::AppServerSupervisor,
};

const OUTPUT_IDLE_TIMEOUT: Duration = Duration::from_secs(180);
const INTERRUPT_WAIT: Duration = Duration::from_secs(10);
const COMPACT_MAX_ATTEMPTS: usize = 2;
const COMPACT_RETRY_DELAY: Duration = Duration::from_secs(2);
const THREAD_UNLOAD_TIMEOUT: Duration = Duration::from_secs(12);
const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
#[error("codex_notification_stream_incomplete")]
struct NotificationLoss;

/// The subset of Codex config that is fixed when a thread is loaded.
///
/// `turn/start` can override some per-turn fields, but app-server keeps a
/// running thread's config snapshot for values like `model_context_window`.
/// Track what we last loaded so a session-level `/model`, `/reasoning`, or
/// `/context` change can force a reload before the next turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeConfigSignature {
    model: Option<String>,
    reasoning_effort: String,
    context_mode: Option<ContextMode>,
    service_tier: Option<ServiceTier>,
    cwd: PathBuf,
    native_environment: bool,
}

impl RuntimeConfigSignature {
    fn from_request(req: &ExecutionRequest) -> Self {
        Self::from_parts(
            req.model.clone(),
            req.reasoning_effort,
            req.context_mode,
            req.service_tier,
            req.workspace_dir.clone(),
        )
    }

    fn from_compact_request(req: &CompactRequest) -> Self {
        Self::from_parts(
            req.model.clone(),
            req.reasoning_effort,
            req.context_mode,
            req.service_tier,
            req.workspace_dir.clone(),
        )
    }

    fn from_parts(
        model: Option<String>,
        reasoning_effort: ReasoningEffort,
        context_mode: Option<ContextMode>,
        service_tier: Option<ServiceTier>,
        cwd: PathBuf,
    ) -> Self {
        Self {
            model,
            reasoning_effort: reasoning_effort.as_str().to_string(),
            context_mode,
            service_tier,
            cwd,
            native_environment: false,
        }
    }
}

/// How the turn should classify sandbox/approval for Codex.
///
/// For both fields, `None` means "defer to the server's config.toml default"
/// (e.g. `sandbox_mode`, `approval_policy`, and the `sandbox_workspace_write.*`
/// knobs in `~/.codex-claw/.codex/config.toml`). `Some(...)` explicitly
/// overrides the config for that turn.
#[derive(Debug, Clone, Default)]
pub struct TurnPolicy {
    pub(crate) approval_policy: Option<ApprovalPolicy>,
    pub(crate) approvals_reviewer: Option<ApprovalsReviewer>,
    pub(crate) sandbox_policy: Option<SandboxPolicy>,
    pub(crate) plan_mode: bool,
}

impl TurnPolicy {
    /// Inherit approval + sandbox from `config.toml`. Used as the default
    /// when the user hasn't opted into a specific mode.
    pub fn inherit_from_config() -> Self {
        Self::default()
    }

    fn social() -> Self {
        Self {
            approval_policy: Some(ApprovalPolicy::Never),
            sandbox_policy: Some(SandboxPolicy::ReadOnly {
                network_access: false,
            }),
            ..Self::default()
        }
    }

    /// Plan mode: read-only sandbox + no approvals (the agent only observes),
    /// with `collaboration_mode = Plan` applied at turn/start.
    pub(crate) fn plan_mode() -> Self {
        Self {
            approval_policy: Some(ApprovalPolicy::Never),
            approvals_reviewer: None,
            sandbox_policy: Some(SandboxPolicy::ReadOnly {
                network_access: false,
            }),
            plan_mode: true,
        }
    }

    /// Explicit approval policy override without touching sandbox.
    pub(crate) fn with_approval_policy(policy: ApprovalPolicy) -> Self {
        Self {
            approval_policy: Some(policy),
            approvals_reviewer: None,
            sandbox_policy: None,
            plan_mode: false,
        }
    }

    pub(crate) fn with_approvals_reviewer(reviewer: ApprovalsReviewer) -> Self {
        Self {
            approval_policy: None,
            approvals_reviewer: Some(reviewer),
            sandbox_policy: None,
            plan_mode: false,
        }
    }
}

pub(crate) struct AppServerSession {
    supervisor: Arc<AppServerSupervisor>,
    runtime_configs: Arc<Mutex<HashMap<String, RuntimeConfigSignature>>>,
}

impl AppServerSession {
    pub(crate) fn new(
        supervisor: Arc<AppServerSupervisor>,
        runtime_configs: Arc<Mutex<HashMap<String, RuntimeConfigSignature>>>,
    ) -> Self {
        Self {
            supervisor,
            runtime_configs,
        }
    }

    /// Execute one turn against the app-server. Preserves the public contract
    /// of the legacy `CodexExecutor::execute` so callers don't change.
    pub(crate) async fn execute(
        &self,
        request: ExecutionRequest,
        policy: TurnPolicy,
        cancel_rx: Option<oneshot::Receiver<()>>,
        update_tx: Option<mpsc::UnboundedSender<ExecutionUpdate>>,
    ) -> Result<ExecutionResult> {
        self.execute_inner(
            request,
            policy,
            None,
            super::SocialTurnInput::default(),
            cancel_rx,
            update_tx,
        )
        .await
    }

    pub(crate) async fn execute_social(
        &self,
        request: ExecutionRequest,
        output_schema: JsonValue,
        input: super::SocialTurnInput,
        cancel_rx: Option<oneshot::Receiver<()>>,
        update_tx: Option<mpsc::UnboundedSender<ExecutionUpdate>>,
    ) -> Result<ExecutionResult> {
        self.execute_inner(
            request,
            TurnPolicy::social(),
            Some(output_schema),
            input,
            cancel_rx,
            update_tx,
        )
        .await
    }

    async fn execute_inner(
        &self,
        request: ExecutionRequest,
        policy: TurnPolicy,
        output_schema: Option<JsonValue>,
        mut input: super::SocialTurnInput,
        cancel_rx: Option<oneshot::Receiver<()>>,
        update_tx: Option<mpsc::UnboundedSender<ExecutionUpdate>>,
    ) -> Result<ExecutionResult> {
        let mut policy = policy;
        let final_only = output_schema.is_some();
        let client = self.supervisor.client().await?;
        let notifications = self.supervisor.subscribe_notifications();
        // Replacing a stable environment ID must never redirect an old thread's
        // MCP teardown to the new executor. Close before registering the guard.
        if self.supervisor.is_isolated()
            && input.tools.iter().any(|name| name == "workspace_exec")
            && let Some(existing) = &request.session_state.session_id
        {
            self.unload_thread(&client, existing)
                .await
                .context("native thread must close before environment registration")?;
            self.runtime_configs.lock().await.remove(existing);
        }
        let mut native =
            super::native_session::NativeSession::start(&self.supervisor, &client, &mut input)
                .await?;
        if native.is_some() {
            policy.sandbox_policy = Some(SandboxPolicy::ExternalSandbox {
                network_access: "restricted".into(),
            });
        }

        let thread_id = self
            .ensure_thread(&client, &request, &policy, &input.tools, native.as_ref())
            .await
            .context("establish thread")?;
        // Every error after loading the thread uses the same ordered cleanup,
        // including tool binding and turn/start failures before drive() begins.
        let mut started_turn = None;
        let result = async {
            let tools_guard = if final_only {
                Some(
                    self.supervisor
                        .social_tools
                        .activate(&thread_id, &input.tools)?,
                )
            } else {
                None
            };
            if let (Some(guard), Some(workspace)) = (&tools_guard, &input.workspace) {
                guard.set_workspace(self.supervisor.validate_tool_workspace(workspace)?)?;
            }
            if let (Some(guard), Some(messages)) = (&tools_guard, &input.messages) {
                guard.set_messages(messages.clone())?;
            }
            if let Some(guard) = &tools_guard {
                if native.is_some() && self.supervisor.is_task_worker() {
                    guard.enable_native_children(client.connection_id)?;
                }
                if let Some(query) = &input.identity {
                    guard.set_identity(query.clone())?;
                }
                if let Some(query) = &input.group_profile {
                    guard.set_group_profile(query.clone())?;
                }
                if let Some(query) = &input.interaction {
                    guard.set_interaction(query.clone())?;
                }
                if let Some(limit) = input.workspace_timeout_seconds {
                    guard.set_workspace_timeout(limit)?;
                }
                if let Some(limit) = input.tool_calls {
                    guard.set_call_limit(limit)?;
                }
                guard.set_skills(
                    self.supervisor
                        .validate_tool_skills(&input.skill_packages)?,
                )?;
                if let Some(integrations) = &input.integrations {
                    guard.set_integrations(
                        integrations
                            .clone()
                            .with_codex_authority(self.supervisor.clone()),
                    )?;
                }
                if let Some(query) = &input.documents {
                    guard.set_documents(query.clone())?;
                }
                if let Some(query) = &input.qq_info {
                    guard.set_qq_info(query.clone())?;
                }
                if let Some(query) = &input.stickers {
                    guard.set_stickers(query.clone())?;
                }
            }

            // Announce the thread id up front so the caller can persist it for
            // resume even if this turn is later interrupted (/stop) or fails
            // mid-flight — those paths return Err and would otherwise drop it.
            if let Some(tx) = update_tx.as_ref() {
                let _ = tx.send(ExecutionUpdate::SessionStarted {
                    session_id: thread_id.clone(),
                });
            }

            let effort = Some(request.reasoning_effort.as_str().to_string());
            let service_tier_wire = request.service_tier.map(service_tier_to_wire);

            // Plan mode needs a concrete model on the CollaborationMode, so fall
            // back to whatever the caller is already using for this turn.
            let collab = if policy.plan_mode {
                request.model.clone().map(|model| CollaborationMode {
                    mode: ModeKind::Plan,
                    settings: CollaborationSettings {
                        model,
                        reasoning_effort: effort.clone(),
                        developer_instructions: None,
                    },
                })
            } else {
                None
            };

            let mut turn_input = build_turn_input(&request);
            turn_input.extend(
                input
                    .images
                    .into_iter()
                    .map(|url| TurnInputItem::Image { url }),
            );
            let turn_params = TurnStartParams {
                environments: self.supervisor.is_isolated().then(|| {
                    native
                        .as_ref()
                        .map(|environment| vec![environment.environment()])
                        .unwrap_or_default()
                }),
                thread_id: thread_id.clone(),
                input: turn_input,
                approval_policy: policy.approval_policy,
                approvals_reviewer: policy.approvals_reviewer.clone(),
                sandbox_policy: policy.sandbox_policy.clone(),
                permissions: None,
                runtime_workspace_roots: build_runtime_workspace_roots(&request.add_dirs),
                model: request.model.clone(),
                effort,
                service_tier: service_tier_wire,
                collaboration_mode: collab,
                output_schema,
            };
            info!(
                thread_id = %thread_id,
                approval_policy = ?policy.approval_policy,
                sandbox_override = policy.sandbox_policy.is_some(),
                plan_mode = policy.plan_mode,
                "sending turn/start"
            );

            if input
                .model_observer
                .as_ref()
                .is_some_and(|observer| !observer.valid())
            {
                // A configured lifecycle Hook may already have run during
                // thread setup. Do not automatically replay that setup.
                if self.supervisor.task_hooks().is_some() {
                    return Err(anyhow!("model_admission_changed_after_hooks"));
                }
                return Err(super::admission::AdmissionDeferred.into());
            }
            let turn_resp: TurnStartResponse = client
                .request("turn/start", &turn_params)
                .await
                .context("turn/start")?;
            let turn_id = turn_resp.turn.id.clone();
            started_turn = Some(turn_id.clone());
            if let Some(guard) = &tools_guard {
                guard.bind_turn(&turn_id)?;
            }
            if let Some(tx) = update_tx.as_ref() {
                let _ = tx.send(ExecutionUpdate::TurnStarted {
                    session_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                });
            }
            debug!(thread_id = %thread_id, turn_id = %turn_id, "turn started");

            let mut runner = TurnRunner {
                native: native.as_mut(),
                tool_cancel: tools_guard
                    .as_ref()
                    .map(|guard| guard.cancellation_handle()),
                client: client.clone(),
                notifications,
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                state: TurnState::default(),
                update_tx,
                cancel_rx,
                cancel_requested: false,
                final_only,
                steer_rx: input.steer,
                allow_long_wait: input.allow_long_wait,
                steer_client_id: None,
                steer_consumed: false,
                steer_sequence: 0,
                activity: crate::codex::app_server::activity::ActivityGate::default(),
                model_observer: input.model_observer.take(),
                model_failure: None,
            };
            let outcome = runner.drive().await?;

            let token_usage_info = runner.build_token_usage_info();
            anyhow::ensure!(
                !runner.state.image_output_invalid,
                "codex_generated_image_invalid"
            );
            let text = if final_only && matches!(&outcome, TurnOutcome::Completed) {
                runner.social_response()?
            } else if final_only {
                String::new()
            } else {
                runner.state.agent_text_parts.join("\n\n").trim().to_owned()
            };

            match outcome {
                TurnOutcome::Completed => Ok(ExecutionResult {
                    session_id: Some(thread_id.clone()),
                    text,
                    changed_files: runner.state.changed_files.clone(),
                    generated_images: runner.state.generated_images.clone(),
                    token_usage_info: token_usage_info.clone(),
                    context_window: token_usage_info
                        .as_ref()
                        .and_then(|info| info.model_context_window),
                }),
                TurnOutcome::Interrupted => Err(anyhow!("codex turn aborted by user")),
                TurnOutcome::Failed(msg) => Err(runner
                    .model_failure
                    .map(anyhow::Error::from)
                    .unwrap_or_else(|| anyhow!("codex turn failed: {msg}"))),
            }
        }
        .await;
        if native.is_some() || final_only {
            if result.is_err() {
                if let Some(native) = &mut native {
                    native.cancel_browser();
                }
                if let Some(turn_id) = started_turn {
                    // A local validation/transport error need not mean the
                    // model turn has settled. Stop it before awaiting unload.
                    // An already interrupted/failed turn may reject this safely.
                    let _ = timeout(
                        CONTROL_REQUEST_TIMEOUT,
                        client.request::<_, TurnInterruptResponse>(
                            "turn/interrupt",
                            &TurnInterruptParams {
                                thread_id: thread_id.clone(),
                                turn_id: Some(turn_id),
                            },
                        ),
                    )
                    .await;
                }
            }
            // Native MCP shutdown still needs its executor transport. A dead
            // executor makes upstream thread shutdown time out and stay loaded.
            let unloaded = self.unload_thread(&client, &thread_id).await;
            if unloaded.is_ok() {
                self.runtime_configs.lock().await.remove(&thread_id);
            }
            // Always reap the namespace, even when native shutdown was refused
            // or unconfirmed. A later resume must prove that thread is closed.
            // Task-scoped social threads must also unload when workspace tools
            // are disabled. Their persisted history remains resumable.
            let stopped = if let Some(native) = &mut native {
                native.shutdown().await
            } else {
                Ok(())
            };
            if let Err(cleanup) = unloaded.and(stopped) {
                return match result {
                    Ok(_) => Err(cleanup.context("social thread cleanup failed")),
                    Err(error) => {
                        Err(error.context(format!("social thread cleanup failed: {cleanup:#}")))
                    }
                };
            }
        }
        result
    }

    pub(crate) async fn compact_thread(
        &self,
        request: CompactRequest,
        cancel_rx: Option<oneshot::Receiver<()>>,
    ) -> Result<()> {
        let client = self.supervisor.client().await?;
        let desired_config = RuntimeConfigSignature::from_compact_request(&request);
        let mut cancel_rx = cancel_rx;
        for attempt in 1..=COMPACT_MAX_ATTEMPTS {
            let thread_id = self
                .reload_thread_for_compact(&client, &request, &desired_config)
                .await
                .context("thread/reload before compact")?;
            let params = ThreadCompactStartParams {
                thread_id: thread_id.clone(),
            };
            let notifications = self.supervisor.subscribe_notifications();
            info!(
                thread_id = %thread_id,
                attempt,
                max_attempts = COMPACT_MAX_ATTEMPTS,
                "sending thread/compact/start"
            );
            client
                .request::<_, ThreadCompactStartResponse>(method::THREAD_COMPACT_START, &params)
                .await
                .context("thread/compact/start")?;
            match wait_for_compaction(&thread_id, notifications, &mut cancel_rx).await {
                Ok(()) => return Ok(()),
                Err(err) if attempt < COMPACT_MAX_ATTEMPTS && is_retryable_compact_error(&err) => {
                    warn!(
                        thread_id = %thread_id,
                        attempt,
                        error = %err,
                        "codex compact failed with retryable stream error; retrying"
                    );
                    sleep(COMPACT_RETRY_DELAY).await;
                }
                Err(err) => return Err(err),
            }
        }
        unreachable!("compact retry loop should return before exhausting attempts")
    }

    async fn reload_thread_for_compact(
        &self,
        client: &Arc<JsonRpcClient>,
        request: &CompactRequest,
        desired_config: &RuntimeConfigSignature,
    ) -> Result<String> {
        if let Err(err) = self
            .unload_thread(client, request.session_id.as_str())
            .await
        {
            if self.supervisor.is_isolated() {
                return Err(err.context("isolated thread must close before compact resume"));
            }
            warn!(
                thread_id = %request.session_id,
                error = %err,
                "failed to unload app-server thread before compact resume"
            );
        }
        self.runtime_configs
            .lock()
            .await
            .remove(&request.session_id);
        let resp: ThreadResumeResponse = client
            .request::<ThreadResumeParams, ThreadResumeResponse>(
                "thread/resume",
                &resume_params(
                    request.session_id.clone(),
                    request.model.clone(),
                    &request.workspace_dir,
                    &request.add_dirs,
                    request.service_tier,
                    None,
                    build_compact_config_overrides(request),
                ),
            )
            .await
            .context("thread/resume before compact")?;
        self.runtime_configs
            .lock()
            .await
            .insert(resp.thread.id.clone(), desired_config.clone());
        Ok(resp.thread.id)
    }

    async fn ensure_thread(
        &self,
        client: &Arc<JsonRpcClient>,
        request: &ExecutionRequest,
        policy: &TurnPolicy,
        tools: &[String],
        native: Option<&super::native_session::NativeSession>,
    ) -> Result<String> {
        let mut desired_config = RuntimeConfigSignature::from_request(request);
        desired_config.native_environment = native.is_some();
        let mut config = build_config_overrides(request);
        if let Some(native) = native {
            native.configure(&mut config);
        } else if self.supervisor.is_isolated() {
            super::native_session::disable(&mut config);
        }
        if let Some(existing) = request.session_state.session_id.clone() {
            if self
                .needs_runtime_reload(existing.as_str(), &desired_config)
                .await
            {
                info!(
                    thread_id = %existing,
                    "app-server runtime config changed; unloading thread before resume"
                );
                if let Err(err) = self.unload_thread(client, existing.as_str()).await {
                    if self.supervisor.is_isolated() {
                        return Err(
                            err.context("native thread must close before environment reload")
                        );
                    }
                    warn!(
                        thread_id = %existing,
                        error = %err,
                        "failed to unload stale app-server thread before resume"
                    );
                }
                self.runtime_configs.lock().await.remove(&existing);
            }
            match client
                .request::<ThreadResumeParams, ThreadResumeResponse>(
                    "thread/resume",
                    &resume_params(
                        existing.clone(),
                        request.model.clone(),
                        &request.workspace_dir,
                        &request.add_dirs,
                        request.service_tier,
                        Some(policy),
                        config.clone(),
                    ),
                )
                .await
            {
                Ok(resp) => {
                    self.runtime_configs
                        .lock()
                        .await
                        .insert(resp.thread.id.clone(), desired_config);
                    return Ok(resp.thread.id);
                }
                Err(err) => {
                    warn!(
                        thread_id = %existing,
                        error = %err,
                        "thread/resume failed; waiting for user recovery choice"
                    );
                    return Err(anyhow!(
                        "codex resume failed for thread `{existing}`: {err:#}"
                    ));
                }
            }
        }
        let start = ThreadStartParams {
            environments: self.supervisor.is_isolated().then(|| {
                native
                    .map(|environment| vec![environment.environment()])
                    .unwrap_or_default()
            }),
            selected_capability_roots: native.map(|environment| environment.capability_roots()),
            dynamic_tools: self
                .supervisor
                .is_isolated()
                .then(|| super::social_tool_policy::definitions(tools)),
            model: request.model.clone(),
            cwd: Some(request.workspace_dir.to_string_lossy().into_owned()),
            approval_policy: policy.approval_policy,
            approvals_reviewer: policy.approvals_reviewer.clone(),
            sandbox: thread_sandbox(policy, &request.add_dirs),
            permissions: None,
            runtime_workspace_roots: build_runtime_workspace_roots(&request.add_dirs),
            service_tier: request.service_tier.map(service_tier_to_wire),
            config,
        };
        info!(
            approval_policy = ?start.approval_policy,
            sandbox_override = policy.sandbox_policy.is_some(),
            plan_mode = policy.plan_mode,
            "sending thread/start"
        );
        let resp: ThreadStartResponse = client
            .request("thread/start", &start)
            .await
            .context("thread/start")?;
        self.runtime_configs
            .lock()
            .await
            .insert(resp.thread.id.clone(), desired_config);
        Ok(resp.thread.id)
    }

    async fn needs_runtime_reload(
        &self,
        thread_id: &str,
        desired_config: &RuntimeConfigSignature,
    ) -> bool {
        self.runtime_configs
            .lock()
            .await
            .get(thread_id)
            .is_some_and(|applied| applied != desired_config)
    }

    async fn unload_thread(&self, client: &Arc<JsonRpcClient>, thread_id: &str) -> Result<()> {
        let mut notifications = self.supervisor.subscribe_notifications();
        let resp: ThreadUnsubscribeResponse = timeout(
            CONTROL_REQUEST_TIMEOUT,
            client.request(
                "thread/unsubscribe",
                &ThreadUnsubscribeParams {
                    thread_id: thread_id.to_string(),
                },
            ),
        )
        .await
        .context("thread/unsubscribe request timed out")?
        .context("thread/unsubscribe")?;
        match resp.status {
            ThreadUnsubscribeStatus::Unsubscribed | ThreadUnsubscribeStatus::NotSubscribed => {
                wait_for_thread_closed(thread_id, &mut notifications).await?;
            }
            ThreadUnsubscribeStatus::NotLoaded => {
                debug!(thread_id, "thread already not loaded before runtime reload");
            }
        }
        Ok(())
    }
}

async fn wait_for_thread_closed(
    thread_id: &str,
    notifications: &mut broadcast::Receiver<Notification>,
) -> Result<()> {
    let wait = async {
        loop {
            match notifications.recv().await {
                Ok(notification) if is_thread_closed(thread_id, &notification) => return Ok(()),
                Ok(notification) if notification.method == method::BACKEND_DISCONNECTED => {
                    return Err(anyhow!(
                        "app-server disconnected before thread close was confirmed"
                    ));
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(anyhow!(
                        "app-server notification stream closed before thread close was confirmed"
                    ));
                }
            }
        }
    };
    timeout(THREAD_UNLOAD_TIMEOUT, wait)
        .await
        .map_err(|_| anyhow!("app-server thread close timed out"))??;
    Ok(())
}

fn is_thread_closed(thread_id: &str, notification: &Notification) -> bool {
    notification.method == method::THREAD_CLOSED
        && notification
            .params
            .get("threadId")
            .and_then(|value| value.as_str())
            == Some(thread_id)
}

/// Build `thread/resume` params. `policy: None` (the compact path) defers
/// approval/sandbox handling entirely to the server's config defaults.
fn resume_params(
    thread_id: String,
    model: Option<String>,
    workspace_dir: &Path,
    add_dirs: &[PathBuf],
    service_tier: Option<ServiceTier>,
    policy: Option<&TurnPolicy>,
    config: HashMap<String, JsonValue>,
) -> ThreadResumeParams {
    ThreadResumeParams {
        thread_id,
        model,
        cwd: Some(workspace_dir.to_string_lossy().into_owned()),
        approval_policy: policy.and_then(|p| p.approval_policy),
        approvals_reviewer: policy.and_then(|p| p.approvals_reviewer.clone()),
        sandbox: policy.and_then(|p| thread_sandbox(p, add_dirs)),
        permissions: None,
        runtime_workspace_roots: build_runtime_workspace_roots(add_dirs),
        service_tier: service_tier.map(service_tier_to_wire),
        config,
    }
}

fn build_turn_input(req: &ExecutionRequest) -> Vec<TurnInputItem> {
    let mut items = Vec::with_capacity(1 + req.image_paths.len());
    items.push(TurnInputItem::Text {
        text: req.prompt.clone(),
    });
    for image in &req.image_paths {
        items.push(TurnInputItem::LocalImage {
            path: image.to_string_lossy().into_owned(),
        });
    }
    items
}

fn build_runtime_workspace_roots(add_dirs: &[PathBuf]) -> Option<Vec<String>> {
    if add_dirs.is_empty() {
        return None;
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut roots = Vec::new();
    for dir in add_dirs {
        let path = dir.to_string_lossy().into_owned();
        if seen.insert(path.clone()) {
            roots.push(path);
        }
    }
    Some(roots)
}

fn thread_sandbox(policy: &TurnPolicy, add_dirs: &[PathBuf]) -> Option<SandboxMode> {
    if !add_dirs.is_empty() {
        return None;
    }
    policy.sandbox_policy.as_ref().map(sandbox_policy_to_mode)
}

fn sandbox_policy_to_mode(policy: &SandboxPolicy) -> SandboxMode {
    match policy {
        SandboxPolicy::ReadOnly { .. } => SandboxMode::ReadOnly,
        SandboxPolicy::WorkspaceWrite { .. } => SandboxMode::WorkspaceWrite,
        SandboxPolicy::DangerFullAccess | SandboxPolicy::ExternalSandbox { .. } => {
            SandboxMode::DangerFullAccess
        }
    }
}

fn build_config_overrides(req: &ExecutionRequest) -> HashMap<String, JsonValue> {
    build_runtime_config_overrides(
        &req.config_overrides,
        req.model.as_deref(),
        req.service_tier,
        req.context_mode,
        req.reasoning_effort.as_str(),
    )
}

fn build_compact_config_overrides(req: &CompactRequest) -> HashMap<String, JsonValue> {
    build_runtime_config_overrides(
        &req.config_overrides,
        req.model.as_deref(),
        req.service_tier,
        req.context_mode,
        req.reasoning_effort.as_str(),
    )
}

fn build_runtime_config_overrides(
    config_overrides: &[String],
    model: Option<&str>,
    service_tier: Option<ServiceTier>,
    context_mode: Option<ContextMode>,
    reasoning_effort: &str,
) -> HashMap<String, JsonValue> {
    let mut out = HashMap::new();
    for override_arg in config_overrides {
        match parse_config_override(override_arg) {
            Some((key, value)) => {
                out.insert(key, value);
            }
            None => {
                warn!(override_arg, "ignoring malformed codex config override");
            }
        }
    }
    if let Some(model) = model {
        out.insert("model".to_string(), JsonValue::String(model.to_string()));
    }
    out.insert(
        "model_reasoning_effort".to_string(),
        JsonValue::String(reasoning_effort.to_string()),
    );
    if let Some(mode) = context_mode {
        out.insert(
            "model_context_window".to_string(),
            match mode {
                ContextMode::Standard => JsonValue::from(272_000u64),
                ContextMode::OneM => JsonValue::from(1_000_000u64),
            },
        );
    }
    if let Some(ServiceTier::Fast) = service_tier {
        out.insert(
            "service_tier".to_string(),
            service_tier_to_config_value(ServiceTier::Fast),
        );
    }
    out
}

pub(crate) fn parse_config_override(raw: &str) -> Option<(String, JsonValue)> {
    let (key, value) = raw.split_once('=')?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    let value = value.trim();
    let parsed = parse_toml_config_value(value)
        .and_then(|value| serde_json::to_value(value).ok())
        .unwrap_or_else(|| JsonValue::String(value.to_string()));
    Some((key.to_string(), parsed))
}

fn parse_toml_config_value(raw: &str) -> Option<toml::Value> {
    let wrapped = format!("value = {raw}");
    let mut table = toml::from_str::<toml::Table>(&wrapped).ok()?;
    table.remove("value")
}

fn service_tier_to_wire(tier: ServiceTier) -> Option<String> {
    match tier {
        ServiceTier::Fast => Some(tier.as_str().to_string()),
        ServiceTier::Flex => None,
    }
}

fn service_tier_to_config_value(tier: ServiceTier) -> JsonValue {
    match service_tier_to_wire(tier) {
        Some(value) => JsonValue::String(value),
        None => JsonValue::Null,
    }
}

async fn receive_notification(
    notifications: &mut broadcast::Receiver<Notification>,
    allow_long_wait: bool,
) -> std::result::Result<
    std::result::Result<Notification, broadcast::error::RecvError>,
    tokio::time::error::Elapsed,
> {
    if allow_long_wait {
        Ok(notifications.recv().await)
    } else {
        timeout(OUTPUT_IDLE_TIMEOUT, notifications.recv()).await
    }
}

async fn wait_for_native_failure(
    receiver: &mut Option<tokio::sync::watch::Receiver<Option<String>>>,
) -> String {
    let Some(receiver) = receiver else {
        return std::future::pending().await;
    };
    loop {
        if let Some(error) = receiver.borrow().clone() {
            return error;
        }
        if receiver.changed().await.is_err() {
            return "native_environment_stopped".into();
        }
    }
}

struct TurnRunner<'a> {
    native: Option<&'a mut super::native_session::NativeSession>,
    tool_cancel: Option<tokio::sync::watch::Sender<bool>>,
    client: Arc<JsonRpcClient>,
    notifications: broadcast::Receiver<Notification>,
    thread_id: String,
    turn_id: String,
    state: TurnState,
    update_tx: Option<mpsc::UnboundedSender<ExecutionUpdate>>,
    cancel_rx: Option<oneshot::Receiver<()>>,
    cancel_requested: bool,
    final_only: bool,
    steer_rx: Option<mpsc::Receiver<super::SocialSteer>>,
    allow_long_wait: bool,
    steer_client_id: Option<String>,
    steer_consumed: bool,
    steer_sequence: u64,
    activity: crate::codex::app_server::activity::ActivityGate,
    model_observer: Option<super::admission::ModelObserver>,
    model_failure: Option<super::admission::ModelFailure>,
}

impl TurnRunner<'_> {
    fn social_response(&self) -> Result<String> {
        if self.steer_client_id.is_some()
            && (!self.steer_consumed || self.state.final_agent_text_parts.is_empty())
        {
            return Err(super::SteerIncomplete.into());
        }
        social_response_text(&self.state)
    }

    async fn drive(&mut self) -> Result<TurnOutcome> {
        let mut final_outcome: Option<TurnOutcome> = None;
        // Pull cancel_rx out of self so its borrow doesn't conflict with
        // &mut self borrows on the other select branch.
        let mut cancel_rx = self.cancel_rx.take();
        let mut steer_rx = self.steer_rx.take();
        let mut native_failure = self.native.as_ref().map(|native| native.failure_receiver());
        while final_outcome.is_none() {
            let has_cancel = cancel_rx.is_some() && !self.cancel_requested;
            tokio::select! {
                biased;
                error = wait_for_native_failure(&mut native_failure) => {
                    self.cancel_requested = true;
                    if let Some(cancel) = &self.tool_cancel { cancel.send_replace(true); }
                    self.send_interrupt().await;
                    self.await_after_interrupt().await?;
                    return Err(anyhow!(error));
                }
                cancel = async {
                    match cancel_rx.as_mut() {
                        Some(rx) => rx.await.ok(),
                        None => std::future::pending::<Option<()>>().await,
                    }
                }, if has_cancel => {
                    if cancel.is_some() {
                        self.cancel_requested = true;
                        if let Some(cancel)=&self.tool_cancel {cancel.send_replace(true);}
                        self.send_interrupt().await;
                        final_outcome = Some(self.await_after_interrupt().await?);
                    } else {
                        cancel_rx = None;
                    }
                }
                event = self.next_event() => {
                    let event = match event {
                        Err(error) if error.is::<NotificationLoss>() => {
                            // A lost event might be the final answer or terminal signal.
                            // Retire this execution before releasing its thread for reuse.
                            self.cancel_requested = true;
                            if let Some(cancel) = &self.tool_cancel { cancel.send_replace(true); }
                            timeout(INTERRUPT_WAIT, async {
                                self.send_interrupt().await;
                                self.await_after_interrupt().await
                            }).await.map_err(|_| super::UnconfirmedInterrupt)??;
                            return Err(error);
                        }
                        other => other?,
                    };
                    match event {
                        Some(notification) => {
                            if let Some(outcome) = self.handle_notification(notification).await {
                                final_outcome = Some(outcome);
                            }
                        }
                        None => {
                            return Err(anyhow!(
                                "codex 后端意外退出，正在重启"
                            ));
                        }
                    }
                }
                steer = async {
                    match steer_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                }, if !self.cancel_requested => {
                    if let Some(steer) = steer {
                        self.steer_sequence += 1;
                        let client_id = format!("{}:steer:{}", self.turn_id, self.steer_sequence);
                        let params = TurnSteerParams {
                            thread_id: self.thread_id.clone(),
                            expected_turn_id: self.turn_id.clone(),
                            input: vec![TurnInputItem::Text { text: steer.text }],
                            client_user_message_id: client_id.clone(),
                        };
                        let response = timeout(Duration::from_secs(5),
                            self.client.request::<_, TurnSteerResponse>("turn/steer", &params)).await;
                        let accepted = matches!(response, Ok(Ok(ref reply)) if reply.turn_id == self.turn_id);
                        if accepted {
                            self.steer_client_id = Some(client_id);
                            self.steer_consumed = false;
                            self.state.final_agent_text_parts.clear();
                        }
                        let _ = steer.acknowledged.send(accepted);
                        if !accepted {
                            // Rejection, disconnect and uncertain ACK all take the same
                            // conservative route: stop, then let the caller start a new
                            // revision. Never resend an uncertain steer to another turn.
                            self.cancel_requested = true;
                            if let Some(cancel) = &self.tool_cancel { cancel.send_replace(true); }
                            self.send_interrupt().await;
                            final_outcome = Some(self.await_after_interrupt().await?);
                        }
                    } else {
                        steer_rx = None;
                    }
                }

            }
        }
        Ok(final_outcome.unwrap_or(TurnOutcome::Completed))
    }

    async fn next_event(&mut self) -> Result<Option<Notification>> {
        loop {
            // Note: the idle timeout resets on ANY broadcast traffic (not just
            // this turn's), which is intentional — a turn can legitimately be
            // paused for a long time awaiting a human approval, and unrelated
            // app-server activity signals the backend is still alive. A turn
            // whose events are lost must be interrupted and confirmed before
            // its thread can be reused, including when no idle deadline applies.
            let event = receive_notification(&mut self.notifications, self.allow_long_wait).await;
            match event {
                // Supervisor-synthesized disconnect: the child exited, so fail
                // fast (fast-restart path) instead of waiting out the timeout.
                Ok(Ok(n)) if n.method == method::BACKEND_DISCONNECTED => return Ok(None),
                Ok(Ok(n)) if is_for_turn(&n, &self.thread_id, &self.turn_id) => {
                    return Ok(Some(n));
                }
                Ok(Ok(n)) => {
                    // A native social task owns this process and its agent tree.
                    // Child failures must pause new root starts without failing
                    // the parent merely because a child reports an error.
                    if self.final_only
                        && self.native.is_some()
                        && n.method == method::ERROR
                        && let Some(failure) =
                            super::admission::ModelFailure::from_native(&n.params["error"])
                        && let Some(observer) = &self.model_observer
                    {
                        if failure.kind == "model_usage_limit" {
                            observer.quota_limited();
                        } else {
                            observer.rate_limited();
                        }
                    }
                    continue;
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => return Ok(None),
                Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                    warn!(lagged = n, "app-server notification stream lagged");
                    return Err(NotificationLoss.into());
                }
                Err(_) => {
                    return Err(anyhow!(
                        "codex 输出超时（{} 秒无新事件）",
                        OUTPUT_IDLE_TIMEOUT.as_secs()
                    ));
                }
            }
        }
    }

    async fn handle_notification(&mut self, n: Notification) -> Option<TurnOutcome> {
        let method = n.method.as_str();
        let params = &n.params;
        if self.final_only
            && self.activity.observe(
                method,
                params,
                &self.thread_id,
                &self.turn_id,
                std::time::Instant::now(),
            )
            && let Some(tx) = self.update_tx.as_ref()
        {
            let _ = tx.send(ExecutionUpdate::Activity);
        }
        match method {
            method::ITEM_STARTED => {
                // The official 0.161 steer test correlates this clientId on
                // UserMessage (it is distinct from the server-generated id).
                // An ACK alone never makes a pre-steer draft the new answer.
                if params.pointer("/item/type").and_then(JsonValue::as_str) == Some("userMessage")
                    && self.steer_client_id.as_deref().is_some_and(|id| {
                        params.pointer("/item/clientId").and_then(JsonValue::as_str) == Some(id)
                    })
                {
                    self.steer_consumed = true;
                    self.state.final_agent_text_parts.clear();
                }
                self.on_translated(params, translator::translate_item_started);
            }
            method::ITEM_UPDATED => {
                self.on_translated(params, translator::translate_item_updated);
            }
            method::ITEM_COMPLETED => {
                self.on_translated(params, translator::translate_item_completed);
            }
            method::TURN_PLAN_UPDATED => {
                self.on_translated(params, translator::translate_turn_plan_updated);
            }
            method::THREAD_TOKEN_USAGE_UPDATED => {
                self.on_translated(params, |state, p: &TokenUsageUpdatedNotification| {
                    translator::translate_token_usage(state, p);
                    tracing::info!(
                        input_tokens = p.token_usage.total.input_tokens,
                        cached_input_tokens = p.token_usage.total.cached_input_tokens,
                        output_tokens = p.token_usage.total.output_tokens,
                        last_input_tokens = p.token_usage.last.input_tokens,
                        usage_scope = "root_thread_cumulative",
                        "social model usage observed"
                    );
                    Vec::new()
                });
            }
            method::THREAD_COMPACTED => {
                self.on_translated(params, |state, p: &CompactedNotification| {
                    vec![translator::translate_compacted(state, p)]
                });
            }
            method::MODEL_REROUTED => {
                self.on_translated(params, |_state, p: &ModelReroutedNotification| {
                    translator::translate_model_rerouted(p)
                        .into_iter()
                        .collect()
                });
            }
            method::ERROR => {
                let error = params.get("error").cloned().unwrap_or(JsonValue::Null);
                let will_retry = params
                    .get("willRetry")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                self.observe_model_error(&error, will_retry);
                let outcome = translator::translate_error_notification(&error, will_retry);
                if matches!(outcome, TurnOutcome::Failed(_)) {
                    return Some(outcome);
                }
            }
            method::TURN_COMPLETED => {
                if let Some(error) = params.pointer("/turn/error") {
                    self.observe_model_error(error, false);
                }
                if let Ok(p) = serde_json::from_value::<TurnCompletedNotification>(params.clone()) {
                    return Some(translator::translate_turn_completed(
                        &mut self.state,
                        &p.turn,
                    ));
                }
            }
            method::TURN_FAILED => {
                if let Some(error) = params.get("error") {
                    self.observe_model_error(error, false);
                }
                let msg = params
                    .get("error")
                    .and_then(|v| v.get("message"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| params.to_string());
                return Some(TurnOutcome::Failed(msg));
            }
            _ => {
                debug!(method = %method, "ignoring notification");
            }
        }
        None
    }

    fn observe_model_error(&mut self, error: &JsonValue, will_retry: bool) {
        let failure = super::admission::ModelFailure::from_native(error);
        if let Some(failure) = &failure {
            tracing::warn!(
                kind = failure.kind,
                http_status = failure.http_status,
                native_code = failure.native_code,
                will_retry,
                retry_after = "unavailable",
                "native model failure observed"
            );
            if let Some(observer) = &self.model_observer {
                if failure.kind == "model_usage_limit" {
                    observer.quota_limited();
                } else {
                    observer.rate_limited();
                }
            }
        }
        if !will_retry {
            self.model_failure = failure;
        }
    }

    async fn send_interrupt(&mut self) {
        if let Some(native) = &self.native {
            native.cancel_browser();
        }
        let params = TurnInterruptParams {
            thread_id: self.thread_id.clone(),
            turn_id: Some(self.turn_id.clone()),
        };
        let response = timeout(
            CONTROL_REQUEST_TIMEOUT,
            self.client
                .request::<_, TurnInterruptResponse>("turn/interrupt", &params),
        )
        .await;
        match response {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => warn!(error = %err, "turn/interrupt failed"),
            Err(_) => warn!("turn/interrupt request timed out"),
        }
    }

    async fn await_after_interrupt(&mut self) -> Result<TurnOutcome> {
        let deadline = tokio::time::Instant::now() + INTERRUPT_WAIT;
        loop {
            let remaining = match deadline.checked_duration_since(tokio::time::Instant::now()) {
                Some(d) => d,
                None => return Err(super::UnconfirmedInterrupt.into()),
            };
            match timeout(remaining, self.notifications.recv()).await {
                Ok(Ok(n))
                    if is_for_turn(&n, &self.thread_id, &self.turn_id)
                        && (n.method == method::TURN_COMPLETED
                            || n.method == method::TURN_FAILED) =>
                {
                    return Ok(TurnOutcome::Interrupted);
                }
                Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => {
                    return Err(super::UnconfirmedInterrupt.into());
                }
            }
        }
    }

    /// Parse `params` as `T` (silently skipping malformed payloads, matching
    /// the per-arm behavior this replaces) and emit whatever the translator
    /// produces.
    fn on_translated<T, F>(&mut self, params: &JsonValue, translate: F)
    where
        T: serde::de::DeserializeOwned,
        F: FnOnce(&mut TurnState, &T) -> Vec<ExecutionUpdate>,
    {
        if let Ok(parsed) = serde_json::from_value::<T>(params.clone()) {
            let updates = translate(&mut self.state, &parsed);
            self.emit_many(updates);
        }
    }

    fn emit_many(&self, updates: Vec<ExecutionUpdate>) {
        if self.final_only {
            return;
        }
        if let Some(tx) = self.update_tx.as_ref() {
            for update in updates {
                let _ = tx.send(update);
            }
        }
    }

    fn build_token_usage_info(&self) -> Option<TokenUsageInfo> {
        let payload = self.state.token_usage.as_ref()?;
        Some(TokenUsageInfo {
            total_token_usage: TokenUsage {
                input_tokens: payload.total.input_tokens,
                cached_input_tokens: payload.total.cached_input_tokens,
                output_tokens: payload.total.output_tokens,
                total_tokens: payload.total.total_tokens,
            },
            last_token_usage: TokenUsage {
                input_tokens: payload.last.input_tokens,
                cached_input_tokens: payload.last.cached_input_tokens,
                output_tokens: payload.last.output_tokens,
                total_tokens: payload.last.total_tokens,
            },
            model_context_window: payload.model_context_window,
        })
    }
}

async fn wait_for_compaction(
    thread_id: &str,
    mut notifications: broadcast::Receiver<Notification>,
    cancel_rx: &mut Option<oneshot::Receiver<()>>,
) -> Result<()> {
    loop {
        let next = async {
            timeout(
                OUTPUT_IDLE_TIMEOUT,
                next_compaction_notification(&mut notifications),
            )
            .await
            .context(format!(
                "codex compact timed out ({} seconds without completion)",
                OUTPUT_IDLE_TIMEOUT.as_secs()
            ))?
        };
        let notification = match cancel_rx.as_mut() {
            Some(cancel) => {
                tokio::select! {
                    _ = cancel => return Err(anyhow!("codex turn aborted by user")),
                    result = next => result?,
                }
            }
            None => next.await?,
        };

        if notification.method.is_empty() {
            anyhow::bail!("app-server notification stream closed during compact");
        }
        if is_compaction_completed(thread_id, &notification) {
            info!(
                thread_id,
                method = %notification.method,
                "codex compaction completed"
            );
            return Ok(());
        }
        if notification.method == method::ERROR {
            let error = notification
                .params
                .get("error")
                .cloned()
                .unwrap_or(JsonValue::Null);
            let will_retry = notification
                .params
                .get("willRetry")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if let TurnOutcome::Failed(msg) =
                translator::translate_error_notification(&error, will_retry)
            {
                anyhow::bail!("codex compact failed: {msg}");
            }
        }
    }
}

fn is_retryable_compact_error(err: &anyhow::Error) -> bool {
    let message = format!("{err:#}").to_ascii_lowercase();
    message.contains("stream disconnected before completion")
        || message.contains("stream closed before response.completed")
}

fn is_compaction_completed(thread_id: &str, notification: &Notification) -> bool {
    match notification.method.as_str() {
        method::THREAD_COMPACTED => {
            notification.params.get("threadId").and_then(|v| v.as_str()) == Some(thread_id)
        }
        method::ITEM_COMPLETED => {
            let Ok(item) = serde_json::from_value::<ItemNotification>(notification.params.clone())
            else {
                return false;
            };
            item.thread_id == thread_id && item.item.item_type == "contextCompaction"
        }
        _ => false,
    }
}

async fn next_compaction_notification(
    notifications: &mut broadcast::Receiver<Notification>,
) -> Result<Notification> {
    loop {
        match notifications.recv().await {
            // Child exited mid-compact: fail fast instead of waiting out the
            // idle timeout (the Sender is program-lifetime, so Closed never
            // fires on a child crash).
            Ok(n) if n.method == method::BACKEND_DISCONNECTED => {
                anyhow::bail!("app-server exited during compact");
            }
            Ok(n) => return Ok(n),
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(lagged = n, "app-server notification stream lagged");
            }
            Err(broadcast::error::RecvError::Closed) => {
                anyhow::bail!("app-server notification stream closed during compact");
            }
        }
    }
}

fn is_for_turn(notification: &Notification, thread_id: &str, turn_id: &str) -> bool {
    let params = &notification.params;
    if notification.method == method::TURN_COMPLETED {
        return params.get("threadId").and_then(JsonValue::as_str) == Some(thread_id)
            && params.pointer("/turn/id").and_then(JsonValue::as_str) == Some(turn_id);
    }
    if notification.method == method::TURN_FAILED {
        return params.get("threadId").and_then(JsonValue::as_str) == Some(thread_id)
            && params.get("turnId").and_then(JsonValue::as_str) == Some(turn_id);
    }
    let matches_thread = params
        .get("threadId")
        .and_then(|v| v.as_str())
        .map(|v| v == thread_id)
        .unwrap_or(false);
    if !matches_thread {
        // Global-ish notifications (configWarning, account/*) have no
        // threadId; allow them so errors can still abort the turn.
        if params.get("threadId").is_none() {
            return true;
        }
        return false;
    }
    match params.get("turnId").and_then(|v| v.as_str()) {
        Some(v) => v == turn_id,
        None => true,
    }
}

fn social_response_text(state: &TurnState) -> Result<String> {
    // A steered turn may finish an earlier answer and then replace it. Each
    // final item is a complete output-schema object; never concatenate them.
    let text = state
        .final_agent_text_parts
        .last()
        .map(String::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    serde_json::from_str::<JsonValue>(&text)
        .map_err(|_| anyhow!("social Codex returned no valid final JSON response"))?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn thread_close_requires_the_matching_thread_notification() {
        let (tx, mut rx) = broadcast::channel(4);
        tx.send(Notification {
            method: method::THREAD_CLOSED.into(),
            params: serde_json::json!({"threadId":"other"}),
        })
        .unwrap();
        let sender = tokio::spawn(async move {
            sleep(Duration::from_secs(1)).await;
            tx.send(Notification {
                method: method::THREAD_CLOSED.into(),
                params: serde_json::json!({"threadId":"current"}),
            })
            .unwrap();
        });
        wait_for_thread_closed("current", &mut rx).await.unwrap();
        sender.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn disconnect_and_closed_notification_channel_never_confirm_thread_unload() {
        let (tx, mut rx) = broadcast::channel(4);
        tx.send(Notification {
            method: method::BACKEND_DISCONNECTED.into(),
            params: JsonValue::Null,
        })
        .unwrap();
        assert!(wait_for_thread_closed("current", &mut rx).await.is_err());
        drop(tx);
        assert!(wait_for_thread_closed("current", &mut rx).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn loaded_thread_without_close_evidence_times_out() {
        let (_tx, mut rx) = broadcast::channel(4);
        let error = wait_for_thread_closed("current", &mut rx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test(start_paused = true)]
    async fn unlimited_foreground_wait_has_no_hidden_idle_deadline_but_observes_disconnect() {
        let (tx, mut rx) = broadcast::channel(4);
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(7200)).await;
            tx.send(Notification {
                method: method::BACKEND_DISCONNECTED.into(),
                params: JsonValue::Null,
            })
            .unwrap();
        });
        let event = receive_notification(&mut rx, true).await.unwrap().unwrap();
        assert_eq!(event.method, method::BACKEND_DISCONNECTED);
        sender.await.unwrap();
        assert!(matches!(
            receive_notification(&mut rx, true).await.unwrap(),
            Err(broadcast::error::RecvError::Closed)
        ));
        let (_tx, mut rx) = broadcast::channel(4);
        assert!(receive_notification(&mut rx, false).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lost_terminal_retires_turn_even_when_notification_sender_stays_alive() {
        use std::os::unix::fs::PermissionsExt;
        for (acknowledges, confirms_terminal) in [(true, true), (true, false), (false, false)] {
            let dir = tempfile::tempdir().unwrap();
            let binary = dir.path().join("fake-server");
            let mut script = String::from(
                "#!/bin/sh\nIFS= read -r request\nprintf '%s\\n' \"$request\" > \"$CODEX_HOME/interrupt.json\"\n",
            );
            if acknowledges {
                script.push_str("printf '%s\\n' '{\"id\":1,\"result\":{}}'\n");
                script.push_str("printf '%s\\n' '{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"thread-current\",\"turn\":{\"id\":\"other-turn\",\"status\":\"completed\"}}}' '{\"method\":\"turn/completed\",\"params\":{\"turn\":{\"id\":\"turn-current\",\"status\":\"completed\"}}}'\n");
            }
            if confirms_terminal {
                script.push_str("printf '%s\\n' '{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"thread-current\",\"turn\":{\"id\":\"turn-current\",\"status\":\"interrupted\"}}}'\n");
            }
            script.push_str("while IFS= read -r next; do :; done\n");
            std::fs::write(&binary, script).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            let transport = Arc::new(
                super::super::transport::StdioTransport::spawn(
                    &binary,
                    dir.path(),
                    dir.path(),
                    None,
                    None,
                )
                .unwrap(),
            );
            let (tx, notifications) = broadcast::channel(2);
            // The terminal event is overwritten; the sender deliberately remains alive.
            tx.send(Notification {
                method: method::TURN_COMPLETED.into(),
                params: JsonValue::Null,
            })
            .unwrap();
            for _ in 0..2 {
                tx.send(Notification {
                    method: "unrelated".into(),
                    params: JsonValue::Null,
                })
                .unwrap();
            }
            let (requests_tx, _requests) = mpsc::channel(4);
            let (client, _exit) =
                JsonRpcClient::start(transport.clone(), tx.clone(), requests_tx, None)
                    .await
                    .unwrap();
            let (tool_cancel, tool_cancelled) = tokio::sync::watch::channel(false);
            let mut runner = TurnRunner {
                tool_cancel: Some(tool_cancel),
                native: None,
                client,
                notifications,
                thread_id: "thread-current".into(),
                turn_id: "turn-current".into(),
                state: TurnState::default(),
                update_tx: None,
                cancel_rx: None,
                cancel_requested: false,
                final_only: true,
                steer_rx: None,
                allow_long_wait: true,
                steer_client_id: None,
                steer_consumed: false,
                steer_sequence: 0,
                activity: crate::codex::app_server::activity::ActivityGate::default(),
                model_observer: None,
                model_failure: None,
            };
            let error = timeout(INTERRUPT_WAIT + Duration::from_secs(2), runner.drive())
                .await
                .unwrap()
                .unwrap_err();
            if confirms_terminal {
                assert!(error.is::<NotificationLoss>());
            } else {
                // Do not release an unconfirmed execution back to the session pool.
                assert!(error.is::<super::super::UnconfirmedInterrupt>());
            }
            assert!(*tool_cancelled.borrow());
            let wire: JsonValue =
                serde_json::from_slice(&std::fs::read(dir.path().join("interrupt.json")).unwrap())
                    .unwrap();
            assert_eq!(wire["method"], "turn/interrupt");
            assert_eq!(wire["params"]["turnId"], "turn-current");
            assert!(tx.receiver_count() > 0);
            runner.client.shutdown("test complete").await;
            if let Some(mut child) = transport.take_child().await {
                child.kill().await.unwrap();
                child.wait().await.unwrap();
            }
        }
    }

    #[test]
    fn social_policy_is_read_only_without_plan_mode_or_approvals() {
        let policy = TurnPolicy::social();
        assert_eq!(policy.approval_policy, Some(ApprovalPolicy::Never));
        assert!(!policy.plan_mode);
        assert!(policy.approvals_reviewer.is_none());
        let wire = serde_json::to_value(policy.sandbox_policy).unwrap();
        assert_eq!(wire["type"], "readOnly");
        assert_eq!(wire["networkAccess"], false);
    }

    #[test]
    fn social_result_only_uses_final_json_and_does_not_fall_back_to_commentary() {
        let mut state = TurnState {
            agent_text_parts: vec!["working commentary".to_owned()],
            ..TurnState::default()
        };
        assert!(social_response_text(&state).is_err());
        state
            .final_agent_text_parts
            .push(r#"{"bubbles":["hello","again"]}"#.to_owned());
        let value: JsonValue =
            serde_json::from_str(&social_response_text(&state).unwrap()).unwrap();
        assert_eq!(value["bubbles"][1], "again");
        state
            .final_agent_text_parts
            .push(r#"{"bubbles":["revised"]}"#.into());
        let value: JsonValue =
            serde_json::from_str(&social_response_text(&state).unwrap()).unwrap();
        assert_eq!(value["bubbles"][0], "revised");
        state
            .final_agent_text_parts
            .push("unexpected second final answer".to_owned());
        assert!(social_response_text(&state).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_steer_uses_exact_turn_and_ack_then_keeps_latest_final() {
        exercise_native_steer(true, true).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejected_native_steer_interrupts_without_marking_revision_accepted() {
        exercise_native_steer(false, false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_steer_consumed_after_sampling_requeues_without_reusing_old_final() {
        exercise_native_steer(true, false).await;
    }

    #[cfg(unix)]
    async fn exercise_native_steer(accepted: bool, produces_final: bool) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("fake-server");
        let script = r##"#!/bin/sh
IFS= read -r request
printf '%s\n' "$request" > "$CODEX_HOME/steer-wire.json"
printf '%s\n' '{"id":1,"result":{"turnId":"turn-current"}}' \
'{"method":"item/completed","params":{"threadId":"thread-current","turnId":"turn-current","item":{"id":"old","type":"agentMessage","phase":"final_answer","text":"{\"bubbles\":[\"old\"]}"}}}' \
'{"method":"item/started","params":{"threadId":"thread-current","turnId":"turn-current","item":{"id":"user-addition","type":"userMessage","clientId":"turn-current:steer:1"}}}' \
'{"method":"item/completed","params":{"threadId":"thread-current","turnId":"turn-current","item":{"id":"new","type":"agentMessage","phase":"final_answer","text":"{\"bubbles\":[\"updated\"]}"}}}' \
'{"method":"turn/completed","params":{"threadId":"thread-current","turn":{"id":"turn-current","status":"completed"}}}'
while IFS= read -r next; do :; done
"##;
        let rejected = r##"#!/bin/sh
IFS= read -r request
printf '%s\n' "$request" > "$CODEX_HOME/steer-wire.json"
printf '%s\n' '{"id":1,"error":{"code":-32600,"message":"no active turn"}}'
IFS= read -r interrupt
printf '%s\n' "$interrupt" > "$CODEX_HOME/interrupt-wire.json"
printf '%s\n' '{"id":2,"result":{}}' '{"method":"turn/completed","params":{"threadId":"thread-current","turn":{"id":"turn-current","status":"interrupted"}}}'
while IFS= read -r next; do :; done
"##;
        let script = if accepted {
            script
                .lines()
                .filter(|line| produces_final || !line.contains("\"id\":\"new\""))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            rejected.to_owned()
        };
        std::fs::write(&binary, script).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let transport = Arc::new(
            super::super::transport::StdioTransport::spawn(
                &binary,
                dir.path(),
                dir.path(),
                None,
                None,
            )
            .unwrap(),
        );
        let (notifications_tx, notifications) = broadcast::channel(32);
        let (requests_tx, _requests) = mpsc::channel(4);
        let (client, _exit) =
            JsonRpcClient::start(transport.clone(), notifications_tx, requests_tx, None)
                .await
                .unwrap();
        let (tx, rx) = mpsc::channel(1);
        let (ack, acknowledged) = oneshot::channel();
        tx.send(super::super::SocialSteer {
            text: "updated requirement".into(),
            acknowledged: ack,
        })
        .await
        .unwrap();
        let mut runner = TurnRunner {
            native: None,
            tool_cancel: None,
            client,
            notifications,
            thread_id: "thread-current".into(),
            turn_id: "turn-current".into(),
            state: TurnState::default(),
            update_tx: None,
            cancel_rx: None,
            cancel_requested: false,
            final_only: true,
            steer_rx: Some(rx),
            allow_long_wait: false,
            steer_client_id: None,
            steer_consumed: false,
            steer_sequence: 0,
            activity: crate::codex::app_server::activity::ActivityGate::default(),
            model_observer: None,
            model_failure: None,
        };
        let result = timeout(Duration::from_secs(3), runner.drive())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(acknowledged.await.unwrap(), accepted);
        if accepted && !produces_final {
            assert!(matches!(result, TurnOutcome::Completed));
            assert!(runner.steer_consumed);
            assert!(
                runner
                    .social_response()
                    .unwrap_err()
                    .is::<super::super::SteerIncomplete>()
            );
        } else if accepted {
            assert!(matches!(result, TurnOutcome::Completed));
            assert!(runner.steer_consumed);
            assert_eq!(
                social_response_text(&runner.state).unwrap(),
                r#"{"bubbles":["updated"]}"#
            );
            runner.steer_consumed = false;
            assert!(
                runner
                    .social_response()
                    .unwrap_err()
                    .is::<super::super::SteerIncomplete>()
            );
            runner.steer_consumed = true;
            runner.state.final_agent_text_parts.clear();
            assert!(
                runner
                    .social_response()
                    .unwrap_err()
                    .is::<super::super::SteerIncomplete>()
            );
        } else {
            assert!(matches!(result, TurnOutcome::Interrupted));
            assert!(runner.steer_client_id.is_none());
            let wire: JsonValue = serde_json::from_slice(
                &std::fs::read(dir.path().join("interrupt-wire.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(wire["method"], "turn/interrupt");
            assert_eq!(wire["params"]["turnId"], "turn-current");
        }
        let wire: JsonValue =
            serde_json::from_slice(&std::fs::read(dir.path().join("steer-wire.json")).unwrap())
                .unwrap();
        assert_eq!(wire["method"], "turn/steer");
        assert_eq!(
            wire["params"],
            serde_json::json!({"threadId":"thread-current","expectedTurnId":"turn-current","clientUserMessageId":"turn-current:steer:1","input":[{"type":"text","text":"updated requirement"}]})
        );
        if let Some(mut child) = transport.take_child().await {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    /// Overrides that a `~/.codex/config.toml` might carry, deliberately
    /// disagreeing with the per-session runtime settings.
    fn file_config_overrides() -> Vec<String> {
        vec![
            "model=\"gpt-config\"".to_string(),
            "model_reasoning_effort=\"low\"".to_string(),
            "model_context_window=128000".to_string(),
            "tool_output_token_limit=2048".to_string(),
        ]
    }

    /// Baseline turn request; each test overrides only the fields it asserts on.
    fn exec_request() -> ExecutionRequest {
        ExecutionRequest {
            prompt: "hi".to_string(),
            workspace_dir: std::path::PathBuf::from("/tmp"),
            codex_home: std::path::PathBuf::from("/tmp/codex-home"),
            config_overrides: Vec::new(),
            add_dirs: Vec::new(),
            session_state: crate::model::settings::SessionState::default(),
            model: Some("gpt-5.5".to_string()),
            service_tier: None,
            context_mode: None,
            reasoning_effort: crate::model::settings::ReasoningEffort::High,
            image_paths: Vec::new(),
        }
    }

    /// Baseline compaction request; same convention as `exec_request`.
    fn compact_request() -> CompactRequest {
        CompactRequest {
            session_id: "thread-1".to_string(),
            workspace_dir: std::path::PathBuf::from("/tmp/work-a"),
            config_overrides: Vec::new(),
            add_dirs: Vec::new(),
            model: Some("gpt-5.5".to_string()),
            service_tier: None,
            context_mode: Some(ContextMode::Standard),
            reasoning_effort: crate::model::settings::ReasoningEffort::High,
        }
    }

    #[test]
    fn is_for_turn_matches_thread_and_turn() {
        let n = Notification {
            method: "turn/started".into(),
            params: serde_json::json!({"threadId":"t","turnId":"u","turn":{"id":"u","status":"inProgress"}}),
        };
        assert!(is_for_turn(&n, "t", "u"));
        assert!(!is_for_turn(&n, "t", "other"));
        assert!(!is_for_turn(&n, "different", "u"));
    }

    #[test]
    fn is_for_turn_allows_unscoped() {
        let n = Notification {
            method: "configWarning".into(),
            params: serde_json::json!({"message":"hi"}),
        };
        assert!(is_for_turn(&n, "t", "u"));
    }

    #[test]
    fn terminal_notifications_require_exact_thread_and_protocol_turn_identity() {
        for (method, params, expected) in [
            (
                method::TURN_COMPLETED,
                serde_json::json!({"threadId":"t","turn":{"id":"u","status":"completed"}}),
                true,
            ),
            (
                method::TURN_COMPLETED,
                serde_json::json!({"threadId":"t","turn":{"id":"other","status":"completed"}}),
                false,
            ),
            (
                method::TURN_COMPLETED,
                serde_json::json!({"turn":{"id":"u","status":"completed"}}),
                false,
            ),
            (
                method::TURN_COMPLETED,
                serde_json::json!({"threadId":"t","turnId":"u"}),
                false,
            ),
            (
                method::TURN_COMPLETED,
                serde_json::json!({"threadId":"other","turn":{"id":"u"}}),
                false,
            ),
            (
                method::TURN_FAILED,
                serde_json::json!({"threadId":"t","turnId":"u"}),
                true,
            ),
            (
                method::TURN_FAILED,
                serde_json::json!({"threadId":"t","turnId":"other"}),
                false,
            ),
            (
                method::TURN_FAILED,
                serde_json::json!({"threadId":"t"}),
                false,
            ),
        ] {
            assert_eq!(
                is_for_turn(
                    &Notification {
                        method: method.into(),
                        params
                    },
                    "t",
                    "u"
                ),
                expected
            );
        }
    }

    #[test]
    fn compact_completion_accepts_legacy_thread_compacted_notification() {
        let n = Notification {
            method: method::THREAD_COMPACTED.into(),
            params: serde_json::json!({"threadId":"thread-1"}),
        };

        assert!(is_compaction_completed("thread-1", &n));
        assert!(!is_compaction_completed("other-thread", &n));
    }

    #[test]
    fn compact_completion_accepts_context_compaction_item() {
        let n = Notification {
            method: method::ITEM_COMPLETED.into(),
            params: serde_json::json!({
                "threadId": "thread-1",
                "item": {
                    "type": "contextCompaction",
                    "id": "item-1"
                }
            }),
        };

        assert!(is_compaction_completed("thread-1", &n));
        assert!(!is_compaction_completed("other-thread", &n));
    }

    #[test]
    fn compact_completion_ignores_regular_items() {
        let n = Notification {
            method: method::ITEM_COMPLETED.into(),
            params: serde_json::json!({
                "threadId": "thread-1",
                "item": {
                    "type": "agentMessage",
                    "id": "item-1"
                }
            }),
        };

        assert!(!is_compaction_completed("thread-1", &n));
    }

    #[test]
    fn compact_retry_detects_stream_disconnect_errors() {
        let err = anyhow!(
            "codex compact failed: stream disconnected before completion: stream closed before response.completed"
        );

        assert!(is_retryable_compact_error(&err));
    }

    #[test]
    fn compact_retry_ignores_non_stream_errors() {
        let err = anyhow!("codex compact failed: context length exceeded");

        assert!(!is_retryable_compact_error(&err));
    }

    #[test]
    fn add_dirs_become_runtime_workspace_roots() {
        let roots = build_runtime_workspace_roots(&[
            std::path::PathBuf::from("/tmp/inbox"),
            std::path::PathBuf::from("/tmp/workspace"),
            std::path::PathBuf::from("/tmp/inbox"),
        ])
        .expect("runtime roots");

        assert_eq!(roots, vec!["/tmp/inbox", "/tmp/workspace"]);
    }

    #[test]
    fn add_dirs_runtime_roots_take_precedence_over_thread_sandbox() {
        let policy = TurnPolicy::plan_mode();

        assert!(thread_sandbox(&policy, &[std::path::PathBuf::from("/tmp/inbox")]).is_none());
        assert!(matches!(
            thread_sandbox(&policy, &[]),
            Some(SandboxMode::ReadOnly)
        ));
    }

    #[test]
    fn flex_service_tier_does_not_write_empty_config_override() {
        let req = ExecutionRequest {
            service_tier: Some(ServiceTier::Flex),
            ..exec_request()
        };

        let overrides = build_config_overrides(&req);
        assert!(!overrides.contains_key("service_tier"));
    }

    #[test]
    fn config_overrides_are_lower_priority_than_session_runtime_settings() {
        let req = ExecutionRequest {
            config_overrides: file_config_overrides(),
            model: Some("gpt-session".to_string()),
            context_mode: Some(ContextMode::OneM),
            ..exec_request()
        };

        let overrides = build_config_overrides(&req);
        assert_eq!(overrides["model"], JsonValue::String("gpt-session".into()));
        assert_eq!(
            overrides["model_reasoning_effort"],
            JsonValue::String("high".into())
        );
        assert_eq!(
            overrides["model_context_window"],
            JsonValue::from(1_000_000u64)
        );
        assert_eq!(overrides["tool_output_token_limit"], JsonValue::from(2048));
    }

    #[test]
    fn compact_config_overrides_match_session_runtime_settings() {
        let req = CompactRequest {
            config_overrides: file_config_overrides(),
            model: Some("gpt-session".to_string()),
            service_tier: Some(ServiceTier::Fast),
            context_mode: Some(ContextMode::OneM),
            ..compact_request()
        };

        let overrides = build_compact_config_overrides(&req);
        assert_eq!(overrides["model"], JsonValue::String("gpt-session".into()));
        assert_eq!(
            overrides["model_reasoning_effort"],
            JsonValue::String("high".into())
        );
        assert_eq!(
            overrides["model_context_window"],
            JsonValue::from(1_000_000u64)
        );
        assert_eq!(overrides["service_tier"], JsonValue::String("fast".into()));
        assert_eq!(overrides["tool_output_token_limit"], JsonValue::from(2048));
    }

    #[test]
    fn runtime_config_signature_tracks_session_runtime_settings() {
        let mut req = ExecutionRequest {
            workspace_dir: std::path::PathBuf::from("/tmp/work-a"),
            context_mode: Some(ContextMode::Standard),
            ..exec_request()
        };

        let original = RuntimeConfigSignature::from_request(&req);
        req.context_mode = Some(ContextMode::OneM);
        assert_ne!(RuntimeConfigSignature::from_request(&req), original);

        req.context_mode = Some(ContextMode::Standard);
        req.reasoning_effort = crate::model::settings::ReasoningEffort::Low;
        assert_ne!(RuntimeConfigSignature::from_request(&req), original);

        req.reasoning_effort = crate::model::settings::ReasoningEffort::High;
        req.model = Some("gpt-5.4".to_string());
        assert_ne!(RuntimeConfigSignature::from_request(&req), original);
    }

    #[test]
    fn compact_runtime_config_signature_tracks_session_runtime_settings() {
        let mut req = compact_request();

        let original = RuntimeConfigSignature::from_compact_request(&req);
        req.context_mode = Some(ContextMode::OneM);
        assert_ne!(RuntimeConfigSignature::from_compact_request(&req), original);

        req.context_mode = Some(ContextMode::Standard);
        req.reasoning_effort = crate::model::settings::ReasoningEffort::Low;
        assert_ne!(RuntimeConfigSignature::from_compact_request(&req), original);

        req.reasoning_effort = crate::model::settings::ReasoningEffort::High;
        req.model = Some("gpt-5.4".to_string());
        assert_ne!(RuntimeConfigSignature::from_compact_request(&req), original);
    }

    #[test]
    fn thread_closed_notification_matches_thread_id() {
        let n = Notification {
            method: method::THREAD_CLOSED.into(),
            params: serde_json::json!({"threadId":"thread-1"}),
        };

        assert!(is_thread_closed("thread-1", &n));
        assert!(!is_thread_closed("other-thread", &n));
    }

    #[test]
    fn malformed_config_override_is_ignored() {
        assert!(parse_config_override("not-a-pair").is_none());
        assert!(parse_config_override(" = 1").is_none());
    }
}
