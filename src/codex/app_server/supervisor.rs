//! Process lifecycle for a long-lived `codex app-server` child.
//!
//! Ensures:
//! - exactly one running child at a time (file lock on the data dir);
//! - automatic respawn with exponential backoff on crash;
//! - `initialize` handshake rerun after each spawn;
//! - atomic client swap so callers always see a live connection.

use std::{
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::{
    sync::{Mutex, RwLock, broadcast, mpsc, oneshot, watch},
    time::{sleep, timeout},
};
use tracing::{error, info, warn};

use super::{
    client::{JsonRpcClient, Notification, ServerRequest},
    isolation::{IsolatedAppServerConfig, SOCIAL_CODEX_HOME, SOCIAL_SQLITE_HOME},
    protocol::{ClientInfo, InitializeCapabilities, InitializeParams, InitializeResponse, method},
    transport::StdioTransport,
};

const CHANNEL_CAP: usize = 1024;
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) struct AppServerSupervisor {
    pub(crate) social_tools: Arc<super::social_tool_policy::Registry>,
    pub(crate) image_auth_authority: RwLock<Option<Arc<AppServerSupervisor>>>,
    pub(crate) image_auth_account: RwLock<Option<String>>,
    image_workers: Mutex<Vec<Weak<AppServerSupervisor>>>,
    initializing_client: Mutex<Option<Arc<JsonRpcClient>>>,
    lifecycle: Mutex<()>,
    stopped: AtomicBool,
    task_worker: bool,
    task_hooks: Option<Arc<super::isolation::HookMounts>>,
    shutdown_notice: watch::Sender<bool>,
    codex_binary: PathBuf,
    codex_home: PathBuf,
    sqlite_home: PathBuf,
    path_env: Option<OsString>,
    client_info: ClientInfo,
    isolation: Option<IsolatedAppServerConfig>,
    notifications_tx: broadcast::Sender<Notification>,
    server_requests_tx: mpsc::Sender<ServerRequest>,
    server_requests_rx: Mutex<Option<mpsc::Receiver<ServerRequest>>>,
    client: RwLock<Option<Arc<JsonRpcClient>>>,
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
}

impl AppServerSupervisor {
    pub(crate) fn validate_tool_skills(&self, paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
        let config = self
            .isolation
            .as_ref()
            .context("social_skills_require_isolation")?;
        let root = config
            .runtime_root
            .parent()
            .context("social_skill_root_required")?
            .join("skills");
        anyhow::ensure!(paths.len() <= 16, "social_skill_limit");
        let mut unique = std::collections::BTreeSet::new();
        for path in paths {
            anyhow::ensure!(
                path.parent() == Some(root.as_path()) && unique.insert(path.clone()),
                "social_skill_scope_invalid"
            );
            crate::util::skill_package::inspect(path)?;
        }
        Ok(paths.to_vec())
    }
    pub(crate) fn validate_tool_workspace(
        &self,
        path: &std::path::Path,
    ) -> Result<super::social_tool_policy::WorkspaceContext> {
        let config = self
            .isolation
            .as_ref()
            .context("social_workspace_requires_isolation")?;
        let root = config
            .runtime_root
            .parent()
            .context("social_workspace_root_required")?
            .join("workspaces");
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("social_workspace_invalid")?;
        anyhow::ensure!(
            path.parent() == Some(root.as_path())
                && name.is_ascii()
                && name.len() == 42
                && matches!(&name[..2], "g_" | "p_")
                && name[2..].bytes().all(|b| b.is_ascii_hexdigit()),
            "social_workspace_invalid"
        );
        super::isolation::validate_workspace_directory(&root)?;
        super::isolation::validate_workspace_directory(path)?;
        Ok(super::social_tool_policy::WorkspaceContext {
            directory: path.to_owned(),
            bubblewrap: config.bubblewrap_binary.clone(),
        })
    }
    pub(crate) fn new(
        codex_binary: PathBuf,
        codex_home: PathBuf,
        sqlite_home: PathBuf,
        path_env: Option<OsString>,
        client_info: ClientInfo,
    ) -> Arc<Self> {
        Self::new_with_isolation(
            codex_binary,
            codex_home,
            sqlite_home,
            path_env,
            client_info,
            None,
        )
    }

    pub(crate) fn new_isolated(
        config: IsolatedAppServerConfig,
        client_info: ClientInfo,
    ) -> Arc<Self> {
        Self::new_with_isolation(
            PathBuf::from("/usr/bin/codex"),
            PathBuf::from(SOCIAL_CODEX_HOME),
            PathBuf::from(SOCIAL_SQLITE_HOME),
            None,
            client_info,
            Some(config),
        )
    }

    fn new_with_isolation(
        codex_binary: PathBuf,
        codex_home: PathBuf,
        sqlite_home: PathBuf,
        path_env: Option<OsString>,
        client_info: ClientInfo,
        isolation: Option<IsolatedAppServerConfig>,
    ) -> Arc<Self> {
        let (notifications_tx, _) = broadcast::channel(CHANNEL_CAP);
        let (server_requests_tx, server_requests_rx) = mpsc::channel(64);
        Arc::new(Self {
            social_tools: super::social_tool_policy::Registry::new(),
            image_auth_authority: RwLock::new(None),
            image_auth_account: RwLock::new(None),
            image_workers: Mutex::new(Vec::new()),
            initializing_client: Mutex::new(None),
            lifecycle: Mutex::new(()),
            stopped: AtomicBool::new(false),
            task_worker: false,
            task_hooks: None,
            shutdown_notice: watch::channel(false).0,
            codex_binary,
            codex_home,
            sqlite_home,
            path_env,
            client_info,
            isolation,
            notifications_tx,
            server_requests_tx,
            server_requests_rx: Mutex::new(Some(server_requests_rx)),
            client: RwLock::new(None),
            shutdown_tx: Mutex::new(None),
        })
    }

    pub(crate) fn validate_social_request(
        &self,
        request: &crate::codex::ExecutionRequest,
    ) -> Result<()> {
        self.isolation
            .as_ref()
            .context("social execution requires an isolated app-server")?
            .validate_request(request)
    }

    pub(crate) fn is_isolated(&self) -> bool {
        self.isolation.is_some()
    }

    pub(crate) fn isolation_config(&self) -> Option<IsolatedAppServerConfig> {
        self.isolation.clone()
    }

    pub(crate) fn is_task_worker(&self) -> bool {
        self.task_worker
    }
    pub(crate) fn task_hooks(&self) -> Option<Arc<super::isolation::HookMounts>> {
        self.task_hooks.clone()
    }

    /// A native agent tree gets its own process. Codex 0.161 does not expose its
    /// internal tree-shutdown fence over JSON-RPC; terminating this process must
    /// never affect another conversation. The installation's independent bot
    /// profile is shared as supported by Codex's concurrent CLI sessions.
    pub(crate) async fn task_worker(
        self: &Arc<Self>,
        admission: super::admission::ModelPermit,
    ) -> Result<TaskWorker> {
        let config = self
            .isolation_config()
            .context("native_task_requires_isolation")?;
        anyhow::ensure!(
            !self.task_worker && !config.image_worker && !config.codex_app_worker,
            "native_task_worker_invalid"
        );
        let hooks = super::isolation::HookMounts::prepare(&config).await?;
        let mut supervisor = Self::new_isolated(config, self.client_info.clone());
        let settings = Arc::get_mut(&mut supervisor).expect("new supervisor");
        settings.task_worker = true;
        settings.task_hooks = hooks;
        let worker = TaskWorker {
            supervisor,
            admission: Some(admission),
        };
        self.register_auxiliary_worker(&worker.supervisor).await?;
        worker.supervisor.start().await?;
        Ok(worker)
    }

    /// Track every access-only auxiliary worker so authority shutdown reaps it.
    pub(crate) async fn register_auxiliary_worker(&self, worker: &Arc<Self>) -> Result<()> {
        let mut workers = self.image_workers.lock().await;
        anyhow::ensure!(
            !self.stopped.load(Ordering::Acquire),
            "image_authority_stopped"
        );
        workers.retain(|worker| worker.strong_count() > 0);
        workers.push(Arc::downgrade(worker));
        Ok(())
    }

    pub(crate) fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown_notice.subscribe()
    }

    /// Launch the supervisor loop. Spawns the child, performs `initialize`,
    /// and kicks off a background task that respawns on EOF.
    pub(crate) async fn start(self: &Arc<Self>) -> Result<()> {
        let (client, exit_rx) = self.spawn_and_initialize().await?;
        *self.client.write().await = Some(client);
        self.initializing_client.lock().await.take();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        *self.shutdown_tx.lock().await = Some(shutdown_tx);

        let sup = self.clone();
        tokio::spawn(async move {
            sup.run_respawn_loop(exit_rx, shutdown_rx).await;
        });
        Ok(())
    }

    async fn run_respawn_loop(
        self: Arc<Self>,
        mut exit_rx: oneshot::Receiver<()>,
        mut shutdown_rx: oneshot::Receiver<()>,
    ) {
        let mut backoff = BACKOFF_MIN;
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown_rx => {
                    info!("supervisor shutdown requested");
                    let mut slot = self.client.write().await;
                    if let Some(client) = slot.as_ref() {
                        client.shutdown("supervisor stopped").await;
                    }
                    slot.take();
                    return;
                }
                _ = &mut exit_rx => {
                    warn!("app-server stdout closed; respawning");
                    // Notify in-flight turns immediately. The broadcast Sender is
                    // program-lifetime, so receivers never observe Closed on a
                    // child crash; without this synthetic signal each turn would
                    // stall for the full output-idle timeout before failing.
                    let _ = self.notifications_tx.send(Notification {
                        method: method::BACKEND_DISCONNECTED.to_string(),
                        params: serde_json::Value::Null,
                    });
                    let mut slot = self.client.write().await;
                    if let Some(client) = slot.as_ref() {
                        client.drain_pending_with_disconnect("app-server exited").await;
                        // Kill child if still around.
                        client.shutdown("respawn").await;
                    }
                    slot.take();
                    drop(slot);
                    if self.task_worker || self.isolation.as_ref().is_some_and(|config| config.image_worker || config.codex_app_worker) {
                        return;
                    }
                    loop {
                        tokio::select! {
                            biased;
                            _ = &mut shutdown_rx => {
                                info!("supervisor stopped during restart backoff");
                                return;
                            }
                            _ = sleep(backoff) => {}
                        }
                        match self.spawn_and_initialize().await {
                            Ok((client, new_exit_rx)) => {
                                *self.client.write().await = Some(client);
                                self.initializing_client.lock().await.take();
                                exit_rx = new_exit_rx;
                                backoff = BACKOFF_MIN;
                                info!("app-server respawned");
                                break;
                            }
                            Err(err) => {
                                error!(error = %err, "failed to respawn app-server");
                                backoff = (backoff * 2).min(BACKOFF_MAX);
                            }
                        }
                    }
                }
            }
        }
    }

    async fn spawn_and_initialize(
        self: &Arc<Self>,
    ) -> Result<(Arc<JsonRpcClient>, oneshot::Receiver<()>)> {
        self.spawn_and_initialize_with_timeout(INITIALIZE_TIMEOUT)
            .await
    }

    async fn spawn_and_initialize_with_timeout(
        self: &Arc<Self>,
        initialize_timeout: Duration,
    ) -> Result<(Arc<JsonRpcClient>, oneshot::Receiver<()>)> {
        let _lifecycle = self.lifecycle.lock().await;
        anyhow::ensure!(!self.stopped.load(Ordering::Acquire), "app_server_stopped");
        let transport = StdioTransport::spawn_with_hooks(
            &self.codex_binary,
            &self.codex_home,
            &self.sqlite_home,
            self.path_env.as_deref(),
            self.isolation.as_ref(),
            self.task_hooks.as_deref(),
        )
        .context("spawn app-server")?;
        let transport = Arc::new(transport);
        let (client, mut exit_rx) = JsonRpcClient::start(
            transport,
            self.notifications_tx.clone(),
            self.server_requests_tx.clone(),
            Some(self.social_tools.clone()),
        )
        .await
        .context("start JSON-RPC client")?;
        *self.initializing_client.lock().await = Some(client.clone());

        let params = InitializeParams {
            client_info: self.client_info.clone(),
            capabilities: Some(InitializeCapabilities {
                experimental_api: Some(true),
            }),
        };
        let handshake = async {
            let resp: InitializeResponse = client
                .request("initialize", &params)
                .await
                .context("initialize handshake")?;
            if let Some(home) = resp.codex_home.as_deref() {
                let expected = self.codex_home.to_string_lossy();
                if home != expected {
                    anyhow::ensure!(
                        self.isolation.is_none(),
                        "isolated app-server reported an unexpected Codex home"
                    );
                    warn!(
                        server_codex_home = home,
                        expected = %expected,
                        "app-server reported different CODEX_HOME than requested"
                    );
                }
            }
            client
                .notify_empty(method::INITIALIZED)
                .await
                .context("initialized notification")
        };
        let initialized = tokio::select! {
            biased;
            _ = &mut exit_rx => Err(anyhow::anyhow!("app-server exited during initialization")),
            result = timeout(initialize_timeout, handshake) => {
                result.unwrap_or_else(|_| Err(anyhow::anyhow!("app-server initialization timed out")))
            }
        };
        if let Err(error) = initialized {
            client.shutdown("app-server initialization failed").await;
            self.initializing_client.lock().await.take();
            return Err(error);
        }
        if self.stopped.load(Ordering::Acquire) {
            client
                .shutdown("app-server stopped during initialization")
                .await;
            self.initializing_client.lock().await.take();
            anyhow::bail!("app_server_stopped");
        }
        Ok((client, exit_rx))
    }

    pub(crate) async fn client(&self) -> Result<Arc<JsonRpcClient>> {
        self.client
            .read()
            .await
            .clone()
            .context("app-server client not ready")
    }

    pub(crate) fn subscribe_notifications(&self) -> broadcast::Receiver<Notification> {
        self.notifications_tx.subscribe()
    }

    /// Take the single receiver for server-initiated requests. Should only be
    /// called by the `ApprovalBroker` at startup.
    pub(crate) async fn take_server_request_receiver(
        &self,
    ) -> Option<mpsc::Receiver<ServerRequest>> {
        self.server_requests_rx.lock().await.take()
    }

    pub(crate) async fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        self.shutdown_notice.send_replace(true);
        let workers = self
            .image_workers
            .lock()
            .await
            .iter()
            .filter_map(|worker| worker.upgrade())
            .collect::<Vec<_>>();
        for worker in workers {
            Box::pin(worker.shutdown()).await;
        }
        let _lifecycle = self.lifecycle.lock().await;
        let mut initializing = self.initializing_client.lock().await;
        if let Some(client) = initializing.as_ref() {
            client
                .shutdown("supervisor stopped during initialization")
                .await;
        }
        initializing.take();
        drop(initializing);
        if let Some(tx) = self.shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }
        // Also actively drop the client so anyone holding a reference sees None.
        let mut slot = self.client.write().await;
        if let Some(client) = slot.as_ref() {
            client.shutdown("supervisor shutdown").await;
        }
        slot.take();
        drop(slot);
        if let Some(hooks) = &self.task_hooks {
            hooks.prepared.shutdown().await;
        }
        self.image_workers.lock().await.clear();
    }
}

/// Covers cancellation before NativeSession has taken ownership of the
/// executor. NativeSession also owns the worker while an executor exists.
pub(crate) struct TaskWorker {
    pub(crate) supervisor: Arc<AppServerSupervisor>,
    pub(crate) admission: Option<super::admission::ModelPermit>,
}
impl Drop for TaskWorker {
    fn drop(&mut self) {
        let supervisor = self.supervisor.clone();
        let admission = self.admission.take();
        tokio::spawn(async move {
            supervisor.shutdown().await;
            drop(admission);
        });
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::{TempDir, tempdir};

    fn fake_server(body: &str) -> (TempDir, Arc<AppServerSupervisor>) {
        let dir = tempdir().unwrap();
        let binary = dir.path().join("fake-app-server");
        std::fs::write(&binary, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let supervisor = AppServerSupervisor::new(
            binary,
            dir.path().to_owned(),
            dir.path().join("sqlite"),
            None,
            ClientInfo {
                name: "fake-startup-test".into(),
                version: "test".into(),
                title: None,
            },
        );
        (dir, supervisor)
    }

    #[tokio::test]
    async fn cancelled_initialization_remains_owned_until_shutdown() {
        let (_directory, supervisor) = fake_server("read line\nwhile read next; do :; done");
        let starting = {
            let supervisor = supervisor.clone();
            tokio::spawn(async move { supervisor.start().await })
        };
        timeout(Duration::from_secs(2), async {
            loop {
                if supervisor.initializing_client.lock().await.is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("initial client should be tracked");
        starting.abort();
        let _ = starting.await;
        timeout(Duration::from_secs(2), supervisor.shutdown())
            .await
            .expect("shutdown should reap the pending child");
        assert!(supervisor.initializing_client.lock().await.is_none());
        assert!(supervisor.client().await.is_err());
    }

    #[tokio::test]
    async fn initialize_fails_promptly_on_stdout_eof() {
        let (_dir, supervisor) = fake_server("IFS= read -r request\nexit 0");
        let result = timeout(Duration::from_secs(2), supervisor.start()).await;
        assert!(
            result.is_ok(),
            "startup waited forever after the child exited"
        );
        assert!(result.unwrap().is_err());
        assert!(supervisor.client().await.is_err());
    }

    #[tokio::test]
    async fn initialize_timeout_shuts_down_the_unresponsive_child() {
        let (dir, supervisor) = fake_server(
            "printf '%s' \"$$\" > \"$CODEX_HOME/child.pid\"\nIFS= read -r request\nIFS= read -r never_sent",
        );
        let result = timeout(
            Duration::from_secs(2),
            supervisor.spawn_and_initialize_with_timeout(Duration::from_millis(100)),
        )
        .await;
        assert!(result.is_ok(), "initialize deadline did not return");
        assert!(result.unwrap().is_err());
        let pid = std::fs::read_to_string(dir.path().join("child.pid")).unwrap();
        #[cfg(target_os = "linux")]
        assert!(
            !std::path::Path::new("/proc").join(pid.trim()).exists(),
            "failed initialization left the child alive"
        );
        assert!(supervisor.client().await.is_err());
    }

    #[tokio::test]
    async fn shutdown_during_restart_backoff_does_not_spawn_again() {
        let (dir, supervisor) = fake_server(
            "count=0\nif [ -f \"$CODEX_HOME/launches\" ]; then IFS= read -r count < \"$CODEX_HOME/launches\"; fi\ncount=$((count+1))\nprintf '%s\\n' \"$count\" > \"$CODEX_HOME/launches\"\nIFS= read -r request\nprintf '%s\\n' '{\"id\":1,\"result\":{}}'\nIFS= read -r initialized\nIFS= read -r exit_now\nexit 0",
        );
        let mut notifications = supervisor.subscribe_notifications();
        supervisor.start().await.unwrap();
        supervisor
            .client()
            .await
            .unwrap()
            .notify_empty("test/exit")
            .await
            .unwrap();
        timeout(Duration::from_secs(2), async {
            loop {
                if notifications.recv().await.unwrap().method == method::BACKEND_DISCONNECTED {
                    break;
                }
            }
        })
        .await
        .unwrap();
        supervisor.shutdown().await;
        sleep(BACKOFF_MIN + Duration::from_millis(150)).await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("launches"))
                .unwrap()
                .trim(),
            "1"
        );
        assert!(supervisor.client().await.is_err());
    }
}
