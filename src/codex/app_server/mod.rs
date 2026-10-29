//! Shared `codex app-server` stdio JSON-RPC backend.
//!
//! Replaces the per-turn `codex exec` subprocess with a single long-lived
//! `codex app-server` child process that serves every QQ user's conversations
//! as independent threads. Exposes [`AppServerHandle`] — the façade used by
//! `CodexExecutor` — which the existing call sites already speak to.

mod activity;
pub mod admission;
pub(crate) mod approvals;
pub(crate) mod client;
pub(crate) mod events;
mod isolation;
mod models;
mod native_agent_context;
mod native_agents;
pub(crate) mod native_browser;
mod native_context;
mod native_environment;
mod native_hooks;
mod native_images;
mod native_session;
pub(crate) mod protocol;
pub mod scoped_integrations;
pub(crate) mod session;
pub(crate) mod social_tool_policy;
mod social_tools;
pub(crate) mod supervisor;
mod tool_output;
pub(crate) mod transport;
mod workspace_exec;
pub(crate) mod workspace_runtime;

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::codex::types::{CompactRequest, ExecutionRequest, ExecutionResult, ExecutionUpdate};

pub use isolation::{IsolatedAppServerConfig, SOCIAL_CODEX_HOME, SOCIAL_WORKSPACE};
pub use models::AvailableModel;
pub use native_images::NativeImageRequest;
pub use protocol::ClientInfo;
pub use session::TurnPolicy;

/// Host-owned, conversation-scoped read access; no model-supplied paths or SQL.
pub type MessageQuery = std::sync::Arc<dyn Fn(&str, &str) -> serde_json::Value + Send + Sync>;
/// Separate capability: only this host-owned catalogue reader can return pixels.
/// Ordinary message/tool JSON is never promoted into image content items.
pub struct StickerToolResult {
    pub data: serde_json::Value,
    pub preview_png: Option<Vec<u8>>,
}
pub type StickerQuery = Arc<dyn Fn(&str) -> StickerToolResult + Send + Sync>;
/// Host-owned asynchronous QQ reader, bound to one conversation.
pub type QqQuery = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = serde_json::Value> + Send>>
        + Send
        + Sync,
>;

/// A host-authorized addition to this execution only. The session owns the
/// client/thread/turn IDs; callers cannot steer another conversation.
pub struct SocialSteer {
    pub text: String,
    pub acknowledged: oneshot::Sender<bool>,
}

#[derive(Debug, thiserror::Error)]
#[error("codex_interrupt_unconfirmed")]
pub struct UnconfirmedInterrupt;

#[derive(Debug, thiserror::Error)]
#[error("codex_steer_without_consumed_final")]
pub struct SteerIncomplete;

#[derive(Default)]
pub struct SocialTurnInput {
    pub admission: Option<admission::ModelPermit>,
    pub model_observer: Option<admission::ModelObserver>,
    pub images: Vec<String>,
    pub tools: Vec<String>,
    pub workspace: Option<std::path::PathBuf>,
    pub messages: Option<MessageQuery>,
    pub steer: Option<mpsc::Receiver<SocialSteer>>,
    /// Foreground host enforces ordinary deadlines; opted-in long tasks may wait
    /// for output without a second hidden idle cutoff. Disconnect/cancel still work.
    pub allow_long_wait: bool,
    pub qq_info: Option<QqQuery>,
    pub documents: Option<QqQuery>,
    pub interaction: Option<QqQuery>,
    pub identity: Option<QqQuery>,
    pub group_profile: Option<QqQuery>,
    pub tool_calls: Option<usize>,
    pub workspace_timeout_seconds: Option<u64>,
    pub integrations: Option<scoped_integrations::ScopedIntegrations>,
    pub skill_packages: Vec<std::path::PathBuf>,
    pub stickers: Option<StickerQuery>,
}
impl SocialTurnInput {
    pub const TOOL_NAMES: &'static [&'static str] = social_tool_policy::DEFAULT_NAMES;
    pub fn validate_tools(names: &[String]) -> Result<()> {
        social_tool_policy::validate_names(names)
    }
}

pub(crate) use approvals::{
    ApprovalBroker, ApprovalOutcome, ApprovalRequest, CommandApprovalEvent,
    FileChangeApprovalEvent, PermissionsApprovalEvent,
};
pub(crate) use session::{AppServerSession, RuntimeConfigSignature};
pub(crate) use supervisor::AppServerSupervisor;

/// Façade the rest of the project uses: holds the supervisor + broker and
/// exposes a single `execute` method mirroring the legacy executor contract.
#[derive(Clone)]
pub struct AppServerHandle {
    pub(crate) supervisor: Arc<AppServerSupervisor>,
    pub(crate) approvals: Arc<ApprovalBroker>,
    runtime_configs: Arc<Mutex<HashMap<String, RuntimeConfigSignature>>>,
    social_admission: Arc<admission::Admission>,
}

impl AppServerHandle {
    /// Start the social bot in a required bubblewrap filesystem view. This
    /// never imports or falls back to the operator's Codex profile.
    pub async fn start_isolated(
        config: IsolatedAppServerConfig,
        client_info: ClientInfo,
    ) -> Result<Self> {
        config.validate()?;
        let supervisor = AppServerSupervisor::new_isolated(config, client_info);
        supervisor.start().await?;
        let approvals = ApprovalBroker::new(supervisor.clone());
        approvals.start().await?;
        Ok(Self {
            supervisor,
            approvals,
            runtime_configs: Arc::new(Mutex::new(HashMap::new())),
            social_admission: Arc::new(admission::Admission::default()),
        })
    }

    pub async fn start(
        codex_binary: std::path::PathBuf,
        codex_home: std::path::PathBuf,
        sqlite_home: std::path::PathBuf,
        path_env: Option<std::ffi::OsString>,
        client_info: ClientInfo,
    ) -> Result<Self> {
        let supervisor =
            AppServerSupervisor::new(codex_binary, codex_home, sqlite_home, path_env, client_info);
        supervisor.start().await?;
        let approvals = ApprovalBroker::new(supervisor.clone());
        approvals.start().await?;
        Ok(Self {
            supervisor,
            approvals,
            runtime_configs: Arc::new(Mutex::new(HashMap::new())),
            social_admission: Arc::new(admission::Admission::default()),
        })
    }

    pub async fn execute(
        &self,
        request: ExecutionRequest,
        policy: TurnPolicy,
        cancel_rx: Option<oneshot::Receiver<()>>,
        update_tx: Option<mpsc::UnboundedSender<ExecutionUpdate>>,
    ) -> Result<ExecutionResult> {
        anyhow::ensure!(
            !self.supervisor.is_isolated(),
            "isolated social app-server requires execute_social"
        );
        AppServerSession::new(self.supervisor.clone(), self.runtime_configs.clone())
            .execute(request, policy, cancel_rx, update_tx)
            .await
    }

    /// Run a text/social turn with fixed read-only/never permissions and a
    /// structured final response. Only thread/turn starts are forwarded; user-visible task updates use a separately scoped typed tool.
    /// commentary and tool output are never delivery candidates. The caller
    /// validates its application-specific schema before sending QQ bubbles.
    pub async fn execute_social(
        &self,
        request: ExecutionRequest,
        output_schema: serde_json::Value,
        cancel_rx: Option<oneshot::Receiver<()>>,
        update_tx: Option<mpsc::UnboundedSender<ExecutionUpdate>>,
    ) -> Result<ExecutionResult> {
        self.execute_social_with_input(
            request,
            output_schema,
            SocialTurnInput::default(),
            cancel_rx,
            update_tx,
        )
        .await
    }

    /// Image input is application-fetched data only, never arbitrary URLs or
    /// filesystem paths supplied by a chat member.
    pub async fn execute_social_with_input(
        &self,
        request: ExecutionRequest,
        output_schema: serde_json::Value,
        mut input: SocialTurnInput,
        cancel_rx: Option<oneshot::Receiver<()>>,
        update_tx: Option<mpsc::UnboundedSender<ExecutionUpdate>>,
    ) -> Result<ExecutionResult> {
        self.supervisor.validate_social_request(&request)?;
        validate_image_inputs(&input.images)?;
        social_tool_policy::validate_names(&input.tools)?;
        if let Some(workspace) = &input.workspace {
            self.supervisor.validate_tool_workspace(workspace)?;
        }
        anyhow::ensure!(
            output_schema.is_object(),
            "social output schema must be an object"
        );
        let permit = input
            .admission
            .take()
            .or_else(|| self.try_social_admission())
            .ok_or(admission::AdmissionDeferred)?;
        anyhow::ensure!(
            permit.belongs_to(&self.social_admission),
            "foreign_model_admission"
        );
        if !permit.valid() {
            return Err(admission::AdmissionDeferred.into());
        }
        tracing::info!(
            prompt_bytes = request.prompt.len(),
            tool_definitions_bytes =
                serde_json::to_vec(&social_tool_policy::definitions(&input.tools))?.len(),
            output_schema_bytes = serde_json::to_vec(&output_schema)?.len(),
            image_inputs = input.images.len(),
            resumed_thread = request.session_state.session_id.is_some(),
            "social model input components"
        );
        input.model_observer = Some(permit.observer());
        let worker = self.supervisor.task_worker(permit).await?;
        let approvals = ApprovalBroker::new(worker.supervisor.clone());
        approvals.start().await?;
        let result = AppServerSession::new(
            worker.supervisor.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        )
        .execute_social(request, output_schema, input, cancel_rx, update_tx)
        .await;
        worker.supervisor.shutdown().await;
        worker
            .admission
            .as_ref()
            .expect("worker admission")
            .finish(result.is_ok());
        result
    }

    pub fn configure_social_admission(
        &self,
        config: admission::ModelAdmissionConfig,
    ) -> Result<()> {
        self.social_admission.configure(config)
    }

    pub fn social_admission_available(&self) -> bool {
        self.social_admission.available()
    }

    pub fn social_admission_epoch(&self) -> u64 {
        self.social_admission.epoch()
    }

    pub fn try_social_admission(&self) -> Option<admission::ModelPermit> {
        self.social_admission.try_acquire()
    }

    pub(crate) async fn compact_thread(
        &self,
        request: CompactRequest,
        cancel_rx: Option<oneshot::Receiver<()>>,
    ) -> Result<()> {
        AppServerSession::new(self.supervisor.clone(), self.runtime_configs.clone())
            .compact_thread(request, cancel_rx)
            .await
    }

    /// Verify exact persisted inputs without starting/resuming a model turn.
    /// Display history alone does not reflect all compaction/rollback records.
    pub(crate) async fn social_context_contains_inputs(
        &self,
        thread_id: &str,
        fingerprints: &[String],
    ) -> Result<bool> {
        let config = self
            .supervisor
            .isolation_config()
            .ok_or_else(|| anyhow::anyhow!("social_runtime_required"))?;
        let root = config.runtime_root.join("codex");
        let client = self.supervisor.client().await?;
        let history: serde_json::Value = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.request(
                "thread/read",
                &serde_json::json!({"threadId":thread_id,"includeTurns":false}),
            ),
        )
        .await
        .map_err(|_| anyhow::anyhow!("context_history_timeout"))??;
        let path = history
            .pointer("/thread/path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("context_history_path_missing"))?
            .to_owned();
        let thread_id = thread_id.to_owned();
        let fingerprints = fingerprints.to_vec();
        tokio::task::spawn_blocking(move || {
            native_context::contains_inputs(&root, &path, &thread_id, &fingerprints)
        })
        .await
        .map_err(|_| anyhow::anyhow!("context_history_read_failed"))?
    }

    /// Discover visible model choices and their advertised reasoning efforts.
    /// No conversation, turn or model completion is created by this request.
    pub async fn available_models(&self) -> Result<Vec<AvailableModel>> {
        models::available_models(
            self.supervisor
                .client()
                .await
                .map_err(|_| anyhow::anyhow!("model_list_client_unavailable"))?,
        )
        .await
    }

    pub async fn shutdown(&self) {
        self.supervisor.shutdown().await;
    }
}

fn validate_image_inputs(images: &[String]) -> Result<()> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    anyhow::ensure!(images.len() <= 3, "social_images_limit");
    let mut total = 0;
    for image in images {
        let (prefix, data) = image
            .split_once(',')
            .ok_or_else(|| anyhow::anyhow!("social_image_invalid"))?;
        anyhow::ensure!(
            matches!(
                prefix,
                "data:image/png;base64"
                    | "data:image/jpeg;base64"
                    | "data:image/gif;base64"
                    | "data:image/webp;base64"
            ) && data.len() <= 2_796_204,
            "social_image_invalid"
        );
        let bytes = STANDARD
            .decode(data)
            .map_err(|_| anyhow::anyhow!("social_image_invalid"))?;
        anyhow::ensure!(
            !bytes.is_empty() && bytes.len() <= 2 * 1024 * 1024,
            "social_image_invalid"
        );
        total += bytes.len();
    }
    anyhow::ensure!(total <= 6 * 1024 * 1024, "social_images_limit");
    Ok(())
}

#[cfg(test)]
mod image_tests {
    use super::*;
    #[test]
    fn social_images_accept_data_and_refuse_paths_remote_urls_and_malformed_content() {
        assert!(validate_image_inputs(&["data:image/png;base64,aGVsbG8=".into()]).is_ok());
        for input in [
            "file:///etc/passwd",
            "https://example.com/image.png",
            "data:image/svg+xml;base64,aA==",
            "data:image/png;base64,%",
            "data:image/png;base64,",
        ] {
            assert!(validate_image_inputs(&[input.into()]).is_err());
        }
        assert!(validate_image_inputs(&vec!["data:image/png;base64,aA==".into(); 4]).is_err());
    }
}
