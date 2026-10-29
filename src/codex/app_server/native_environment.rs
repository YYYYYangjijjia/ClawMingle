//! Native Codex tools in a conversation-owned, network-isolated executor.
//!
//! The parent app-server keeps authentication and model orchestration. This
//! module only connects its native environment protocol to a fresh exec-server
//! through an authenticated loopback WebSocket and JSON-lines stdin/stdout.
//! No model tool is implemented here, and no credential enters the executor.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use futures_util::{SinkExt, StreamExt};
use rand::{RngCore, rngs::OsRng};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{
    WebSocketStream, accept_hdr_async_with_config,
    tungstenite::{
        Message,
        handshake::server::{ErrorResponse, Request, Response},
        http::StatusCode,
        protocol::WebSocketConfig,
    },
};

use super::{
    client::JsonRpcClient,
    isolation::IsolatedAppServerConfig,
    workspace_exec::validate_workspace,
    workspace_runtime::{append_runtime_mounts, validated_native_binary},
};

const HOST_RPC_PREFIX: &str = "clawmingle-hook-rpc-";
pub(super) const HOOK_PROCESS_PREFIX: &str = "clawmingle-hook-process-";

/// Host-only access to this executor connection. Never expose it as a model tool.
#[derive(Clone)]
pub(crate) struct NativeEnvironmentRpc {
    requests: mpsc::UnboundedSender<HostRequest>,
}

struct HostRequest {
    method: &'static str,
    params: Value,
    response: oneshot::Sender<Result<Value>>,
}

impl NativeEnvironmentRpc {
    pub(crate) async fn request(&self, method: &'static str, params: Value) -> Result<Value> {
        let (response, result) = oneshot::channel();
        self.requests
            .send(HostRequest {
                method,
                params,
                response,
            })
            .map_err(|_| anyhow::anyhow!("native_hook_executor_unavailable"))?;
        timeout(Duration::from_secs(15), result)
            .await
            .map_err(|_| anyhow::anyhow!("native_hook_rpc_timeout"))?
            .map_err(|_| anyhow::anyhow!("native_hook_executor_unavailable"))?
    }

    /// Drop/cancellation must queue termination even after the waiting future is gone.
    pub(crate) fn terminate(&self, process_id: &str) {
        let (response, _) = oneshot::channel();
        let _ = self.requests.send(HostRequest {
            method: "process/terminate",
            params: json!({"processId":process_id}),
            response,
        });
    }

    #[cfg(test)]
    pub(super) fn mock(callback: impl Fn(&str, Value) -> Result<Value> + Send + 'static) -> Self {
        let (requests, mut receiver) = mpsc::unbounded_channel::<HostRequest>();
        tokio::spawn(async move {
            while let Some(request) = receiver.recv().await {
                let result = callback(request.method, request.params);
                let _ = request.response.send(result);
            }
        });
        Self { requests }
    }
}

const RPC_LIMIT: usize = 32 * 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const REGISTER_TIMEOUT: Duration = Duration::from_secs(15);
const INITIAL_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
const WORKSPACE_CHECK_INTERVAL: Duration = Duration::from_secs(2);
pub(super) const EXECUTION_ENVIRONMENT: &[(&str, &str)] = &[
    ("PATH", "/runtime:/tools:/usr/bin"),
    ("HOME", "/home/worker"),
    ("CODEX_HOME", "/home/worker/.codex"),
    ("TMPDIR", "/tmp"),
    ("LANG", "C.UTF-8"),
    ("TZ", "Asia/Shanghai"),
];
const WORKSPACE_BUDGET: WorkspaceBudget = WorkspaceBudget {
    bytes: 512 * 1024 * 1024,
    entries: 8192,
    depth: 64,
};

#[derive(Clone, Copy)]
struct WorkspaceBudget {
    bytes: u64,
    entries: usize,
    depth: usize,
}

/// A bounded scan, not a disk quota: writes can exceed the budget between checks.
/// Keep an open root and resolve children through directory fds so concurrent
/// symlink replacement cannot make the host inspect another conversation.
struct WorkspaceMonitor {
    root: File,
    budget: WorkspaceBudget,
    interval: Duration,
}

impl WorkspaceMonitor {
    async fn open(path: &Path, budget: WorkspaceBudget, interval: Duration) -> Result<Self> {
        let root = open_path_without_following(path)?;
        ensure!(
            root.metadata()?.is_dir(),
            "native_environment_workspace_invalid"
        );
        let monitor = Self {
            root,
            budget,
            interval,
        };
        monitor.check().await?;
        Ok(monitor)
    }

    async fn check(&self) -> Result<()> {
        let root = self
            .root
            .try_clone()
            .map_err(|_| anyhow::anyhow!("native_environment_workspace_scan_failed"))?;
        let budget = self.budget;
        tokio::task::spawn_blocking(move || {
            let mut bytes = 0;
            let mut entries = 0;
            scan_workspace(&root, budget, 0, &mut bytes, &mut entries)
        })
        .await
        .map_err(|_| anyhow::anyhow!("native_environment_workspace_scan_failed"))?
    }

    async fn until_failure(&self) -> Result<()> {
        loop {
            tokio::time::sleep(self.interval).await;
            self.check().await?;
        }
    }
}

fn open_path_without_following(path: &Path) -> std::io::Result<File> {
    // Linux O_PATH | O_NOFOLLOW: inspect even a symlink/FIFO without opening its
    // target or blocking. The executor itself is Linux/bubblewrap-only.
    OpenOptions::new()
        .read(true)
        .custom_flags(0o10000000 | 0o400000)
        .open(path)
}

fn scan_workspace(
    directory: &File,
    budget: WorkspaceBudget,
    depth: usize,
    bytes: &mut u64,
    entries: &mut usize,
) -> Result<()> {
    ensure!(
        depth <= budget.depth,
        "native_environment_workspace_depth_exceeded"
    );
    let anchored = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let children = std::fs::read_dir(&anchored)
        .map_err(|_| anyhow::anyhow!("native_environment_workspace_scan_failed"))?;
    for child in children {
        let child =
            child.map_err(|_| anyhow::anyhow!("native_environment_workspace_scan_failed"))?;
        *entries += 1;
        ensure!(
            *entries <= budget.entries,
            "native_environment_workspace_entries_exceeded"
        );
        let file = match open_path_without_following(&anchored.join(child.file_name())) {
            Ok(file) => file,
            // A concurrent unlink has already removed this entry's storage.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => anyhow::bail!("native_environment_workspace_scan_failed"),
        };
        let metadata = file
            .metadata()
            .map_err(|_| anyhow::anyhow!("native_environment_workspace_scan_failed"))?;
        if metadata.is_dir() {
            scan_workspace(&file, budget, depth + 1, bytes, entries)?;
        } else if metadata.is_file() {
            *bytes = bytes.saturating_add(metadata.len());
            ensure!(
                *bytes <= budget.bytes,
                "native_environment_workspace_bytes_exceeded"
            );
        }
    }
    Ok(())
}

struct EnvironmentWatch {
    host_requests: mpsc::UnboundedReceiver<HostRequest>,
    workspace: WorkspaceMonitor,
    failure: watch::Sender<Option<String>>,
}

static ACTIVE_WORKSPACES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

struct WorkspaceLease(PathBuf);

impl WorkspaceLease {
    fn acquire(path: &Path) -> Result<Self> {
        ensure!(
            ACTIVE_WORKSPACES
                .get_or_init(Mutex::default)
                .lock()
                .map_err(|_| anyhow::anyhow!("native_environment_lease_unavailable"))?
                .insert(path.to_owned()),
            "native_environment_workspace_busy"
        );
        Ok(Self(path.to_owned()))
    }
}

impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE_WORKSPACES.get_or_init(Mutex::default).lock() {
            active.remove(&self.0);
        }
    }
}

/// Hold until the turn settles. Explicit shutdown waits for the executor and
/// its PID namespace to stop before releasing the per-workspace lease.
///
/// This type intentionally has no Debug implementation: registration includes
/// a bearer capability. A disconnected executor is never replaced by a local
/// shell. The caller starts a new guard before resuming an unloaded thread.
pub(crate) struct NativeEnvironmentGuard {
    environment_id: String,
    url: String,
    token: String,
    skill_names: Vec<String>,
    host_rpc: NativeEnvironmentRpc,
    running: Arc<AtomicBool>,
    failure: watch::Receiver<Option<String>>,
    cancel: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<()>>>,
}

impl NativeEnvironmentGuard {
    /// `append_mounts` is a trusted host callback, never model-provided data.
    /// It can install reviewed browser runtime/socket mounts before the single
    /// persistent writable workspace bind. It must not mount authority state.
    pub(crate) async fn start(
        config: &IsolatedAppServerConfig,
        workspace: &Path,
        skills: &[PathBuf],
        append_mounts: impl FnOnce(&mut Command) -> Result<()>,
    ) -> Result<Self> {
        let workspace = validate_workspace(workspace)?;
        let lease = WorkspaceLease::acquire(&workspace)?;
        let (command, skill_names) = build_command(config, &workspace, skills, append_mounts)?;
        Self::start_command(command, &workspace, skill_names, lease).await
    }

    async fn start_command(
        mut command: Command,
        workspace: &Path,
        skill_names: Vec<String>,
        lease: WorkspaceLease,
    ) -> Result<Self> {
        let monitor =
            WorkspaceMonitor::open(workspace, WORKSPACE_BUDGET, WORKSPACE_CHECK_INTERVAL).await?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .context("native_environment_listener_unavailable")?;
        let url = format!("ws://{}", listener.local_addr()?);
        let mut random = [0_u8; 32];
        OsRng.fill_bytes(&mut random);
        let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let child = command.spawn().context("native_environment_spawn_failed")?;
        let environment_id = format!(
            "clawmingle-{:x}",
            md5::compute(workspace.as_os_str().as_bytes())
        );
        let running = Arc::new(AtomicBool::new(true));
        let (cancel, cancelled) = oneshot::channel();
        let (host_requests, host_receiver) = mpsc::unbounded_channel();
        let (failure_sender, failure) = watch::channel(None);
        let task = tokio::spawn(run_environment(
            child,
            listener,
            token.clone(),
            cancelled,
            running.clone(),
            lease,
            EnvironmentWatch {
                host_requests: host_receiver,
                workspace: monitor,
                failure: failure_sender,
            },
        ));
        Ok(Self {
            environment_id,
            url,
            token,
            skill_names,
            host_rpc: NativeEnvironmentRpc {
                requests: host_requests,
            },
            running,
            failure,
            cancel: Some(cancel),
            task: Some(task),
        })
    }

    pub(crate) fn host_rpc(&self) -> NativeEnvironmentRpc {
        self.host_rpc.clone()
    }

    pub(crate) fn environment_id(&self) -> &str {
        &self.environment_id
    }

    /// The turn owner must interrupt native model work when this becomes Some.
    /// Disconnecting exec RPC alone does not stop the model from continuing.
    pub(crate) fn failure_receiver(&self) -> watch::Receiver<Option<String>> {
        self.failure.clone()
    }

    pub(crate) fn environment_params(&self) -> Value {
        json!({"environmentId":self.environment_id,"cwd":"/workspace","runtimeWorkspaceRoots":["/workspace"]})
    }

    pub(crate) fn selected_capability_roots(&self) -> Vec<Value> {
        self.skill_names
            .iter()
            .map(|name| {
                json!({"id":format!("clawmingle-skill-{name}"),"location":{"type":"environment","environmentId":self.environment_id,"path":format!("/skills/{name}")}})
            })
            .collect()
    }

    /// Call before thread start/resume, not after enabling native tools. A new
    /// guard replaces the stable scope id with its fresh executor connection.
    pub(crate) async fn register(&self, client: &JsonRpcClient) -> Result<()> {
        ensure!(
            self.running.load(Ordering::Acquire),
            "native_environment_stopped"
        );
        let registration = json!({"environmentId":self.environment_id,"execServerUrl":self.url,"authBearerToken":self.token});
        let scope = json!({"environmentId":self.environment_id});
        let operation = async {
            let _: Value = client.request("environment/add", &registration).await?;
            // `add` is lazy. Resolve the environment before a turn can use it.
            let info: Value = client.request("environment/info", &scope).await?;
            ensure!(
                info.get("cwd").and_then(Value::as_str) == Some("file:///workspace"),
                "native_environment_cwd_mismatch"
            );
            let status: Value = client.request("environment/status", &scope).await?;
            ensure!(
                status.get("status").and_then(Value::as_str) == Some("ready")
                    && self.running.load(Ordering::Acquire),
                "native_environment_not_ready"
            );
            Ok(())
        };
        timeout(REGISTER_TIMEOUT, operation)
            .await
            .map_err(|_| anyhow::anyhow!("native_environment_registration_timeout"))?
    }

    pub(crate) async fn shutdown(&mut self) -> Result<()> {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(task) = self.task.take() {
            task.await
                .map_err(|_| anyhow::anyhow!("native_environment_cleanup_failed"))??;
        }
        Ok(())
    }
}

impl Drop for NativeEnvironmentGuard {
    fn drop(&mut self) {
        // The task owns the process and lease; dropping this guard must not
        // release the lease while its executor is still being killed/reaped.
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

fn build_command(
    config: &IsolatedAppServerConfig,
    workspace: &Path,
    skills: &[PathBuf],
    append_mounts: impl FnOnce(&mut Command) -> Result<()>,
) -> Result<(Command, Vec<String>)> {
    ensure!(skills.len() <= 16, "social_skill_limit");
    let codex = validated_native_binary(&config.codex_binary)?;
    let bubblewrap = validated_native_binary(&config.bubblewrap_binary)?;
    let mut names = BTreeSet::new();
    for path in skills {
        crate::util::skill_package::inspect(path)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("skill_name_invalid"))?;
        ensure!(
            crate::util::skill_package::valid_name(name) && names.insert(name.to_owned()),
            "skill_name_invalid"
        );
    }
    let mut command = Command::new(bubblewrap);
    command
        .env_clear()
        .current_dir("/")
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.args([
        "--unshare-all",
        "--unshare-user",
        "--cap-drop",
        "ALL",
        "--new-session",
        "--die-with-parent",
    ]);
    for directory in [
        "/usr",
        "/etc",
        "/home",
        "/home/worker",
        "/home/worker/.codex",
        "/runtime",
        "/run",
        "/skills",
        "/workspace",
    ] {
        command.args(["--dir", directory]);
    }
    append_runtime_mounts(&mut command)?;
    command.arg("--ro-bind").arg(codex).arg("/runtime/codex");
    command.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
    append_mounts(&mut command)?;
    for path in skills {
        let name = path.file_name().expect("validated skill name");
        command
            .arg("--ro-bind")
            .arg(path)
            .arg(Path::new("/skills").join(name));
    }
    for &(key, value) in EXECUTION_ENVIRONMENT {
        command.env(key, value);
    }
    // No authority, app-server profile, state, tokens or host network mount.
    // Construct all mountpoints before this attacker-writable persistent bind.
    command.arg("--bind").arg(workspace).arg("/workspace");
    command.args([
        "--chdir",
        "/workspace",
        "--",
        "/runtime/codex",
        "exec-server",
        "--listen",
        "stdio",
    ]);
    apply_resource_limits(&mut command, 128 * 1024 * 1024);
    Ok((command, names.into_iter().collect()))
}

fn apply_resource_limits(command: &mut Command, file_bytes: u64) {
    use std::os::{
        raw::{c_int, c_ulong},
        unix::process::CommandExt,
    };
    #[repr(C)]
    struct Limit {
        soft: c_ulong,
        hard: c_ulong,
    }
    unsafe extern "C" {
        fn setrlimit(resource: c_int, limit: *const Limit) -> c_int;
    }
    // These hard limits are inherited by every executor descendant and cannot
    // be raised without CAP_SYS_RESOURCE (dropped by bwrap). CPU is per-process
    // accumulated CPU time, not turn wall time or model thinking time. The
    // service's MemoryMax/TasksMax remain the aggregate memory/process boundary;
    // an address-space cap would prevent ordinary V8/Chromium reservations.
    unsafe {
        command.as_std_mut().pre_exec(move || {
            for (resource, maximum) in [(0, 300), (1, file_bytes as c_ulong), (4, 0)] {
                let limit = Limit {
                    soft: maximum,
                    hard: maximum,
                };
                if setrlimit(resource, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

async fn run_environment(
    mut child: Child,
    listener: TcpListener,
    token: String,
    mut cancelled: oneshot::Receiver<()>,
    running: Arc<AtomicBool>,
    _lease: WorkspaceLease,
    monitor: EnvironmentWatch,
) -> Result<()> {
    let EnvironmentWatch {
        workspace,
        failure,
        host_requests,
    } = monitor;
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        if let Some(mut stderr) = stderr {
            let mut buffer = [0_u8; 4096];
            while matches!(stderr.read(&mut buffer).await, Ok(n) if n > 0) {}
        }
    });
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let operation = async {
        let stdin = stdin.context("native_environment_stdin_missing")?;
        let stdout = stdout.context("native_environment_stdout_missing")?;
        let socket = timeout(
            INITIAL_CONNECTION_TIMEOUT,
            accept_authorized(&listener, &token),
        )
        .await
        .map_err(|_| anyhow::anyhow!("native_environment_connection_timeout"))??;
        // Only one app-server connection can own this executor. Reconnect by
        // creating a fresh guard and resuming the thread with the same scope id.
        drop(listener);
        forward(socket, stdin, BufReader::new(stdout), host_requests).await
    };
    tokio::pin!(operation);
    let result = tokio::select! {
        _ = &mut cancelled => Ok(()),
        result = &mut operation => result,
        result = workspace.until_failure() => result,
        _ = child.wait() => Err(anyhow::anyhow!("native_environment_exited")),
    };
    running.store(false, Ordering::Release);
    if let Err(error) = &result {
        // Publish before reaping so the caller can promptly interrupt the turn
        // and revoke its browser bridge. Only fixed monitor codes are exposed.
        let message = error.to_string();
        let code = if message.starts_with("native_environment_workspace_") {
            message
        } else {
            "native_environment_stopped".to_owned()
        };
        failure.send_replace(Some(code));
    }
    // SIGKILL of the bwrap namespace owner also terminates descendants; do not
    // release the lease until wait confirms the owner has exited.
    let _ = child.start_kill();
    child
        .wait()
        .await
        .context("native_environment_reap_failed")?;
    stderr_task.abort();
    let _ = stderr_task.await;
    result
}

async fn accept_authorized(
    listener: &TcpListener,
    token: &str,
) -> Result<WebSocketStream<TcpStream>> {
    loop {
        let (stream, peer) = listener.accept().await?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let expected = format!("Bearer {token}");
        // Tungstenite's handshake callback requires its concrete HTTP error type.
        #[allow(clippy::result_large_err)]
        let callback = move |request: &Request, response: Response| {
            let actual = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            if request.uri().path() == "/" && token_matches(actual, &expected) {
                Ok(response)
            } else {
                let mut denied = ErrorResponse::new(Some("unauthorized".into()));
                *denied.status_mut() = StatusCode::UNAUTHORIZED;
                Err(denied)
            }
        };
        let config = WebSocketConfig {
            max_message_size: Some(RPC_LIMIT),
            max_frame_size: Some(RPC_LIMIT),
            ..Default::default()
        };
        if let Ok(Ok(socket)) = timeout(
            HANDSHAKE_TIMEOUT,
            accept_hdr_async_with_config(stream, callback, Some(config)),
        )
        .await
        {
            return Ok(socket);
        }
    }
}

fn token_matches(actual: &str, expected: &str) -> bool {
    actual.len() == expected.len()
        && actual
            .bytes()
            .zip(expected.bytes())
            .fold(0_u8, |different, (a, b)| different | (a ^ b))
            == 0
}

async fn forward<W, R>(
    socket: WebSocketStream<TcpStream>,
    mut stdin: W,
    mut stdout: R,
    mut host_requests: mpsc::UnboundedReceiver<HostRequest>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
{
    let (mut output, mut input) = socket.split();
    let mut line = Vec::new();
    let mut host_pending = BTreeMap::new();
    let mut next_host_id = 0_u64;
    let mut host_open = true;
    loop {
        tokio::select! {
            request = host_requests.recv(), if host_open => {
                let Some(request) = request else { host_open = false; continue; };
                ensure!(host_pending.len() < 64, "native_hook_rpc_limit");
                next_host_id = next_host_id.checked_add(1).context("native_hook_rpc_limit")?;
                let id = format!("{HOST_RPC_PREFIX}{next_host_id}");
                let data = serde_json::to_vec(&json!({"id":id,"method":request.method,"params":request.params}))?;
                ensure!(data.len() <= RPC_LIMIT, "native_hook_rpc_too_large");
                host_pending.insert(id, request.response);
                stdin.write_all(&data).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await?;
            },
            frame = input.next() => {
                let frame = frame.context("native_environment_connection_closed")??;
                let data = match frame {
                    Message::Text(text) => text.into_bytes(),
                    Message::Binary(data) => data,
                    Message::Ping(data) => {output.send(Message::Pong(data)).await?;continue;},
                    Message::Pong(_) => continue,
                    Message::Close(_) => anyhow::bail!("native_environment_connection_closed"),
                    Message::Frame(_) => continue,
                };
                ensure!(data.len() <= RPC_LIMIT, "native_environment_message_too_large");
                let value: Value = serde_json::from_slice(&data)
                    .map_err(|_| anyhow::anyhow!("native_environment_invalid_message"))?;
                ensure!(value.is_object(), "native_environment_invalid_message");
                ensure!(!value.get("id").and_then(Value::as_str).is_some_and(|id| id.starts_with(HOST_RPC_PREFIX))
                    && !value.pointer("/params/processId").and_then(Value::as_str).is_some_and(|id| id.starts_with(HOOK_PROCESS_PREFIX)),
                    "native_environment_reserved_rpc");
                let mut data = serde_json::to_vec(&value)?;
                data.push(b'\n');
                stdin.write_all(&data).await?;
                stdin.flush().await?;
            },
            result = read_line(&mut stdout, &mut line) => {
                result?;
                let value: Value = serde_json::from_slice(&line)
                    .map_err(|_| anyhow::anyhow!("native_environment_invalid_output"))?;
                if let Some(id) = value.get("id").and_then(Value::as_str)
                    && id.starts_with(HOST_RPC_PREFIX)
                {
                    if let Some(response) = host_pending.remove(id) {
                        let result = value.get("result").cloned()
                            .ok_or_else(|| anyhow::anyhow!("native_hook_rpc_failed"));
                        let _ = response.send(result);
                    }
                    line.clear();
                    continue;
                }
                // Hook output belongs only to its native hook invocation. It must
                // not be mistaken for a model shell session or sent twice.
                if value.get("method").and_then(Value::as_str).is_some_and(|method| matches!(method, "process/output" | "process/exited" | "process/closed"))
                    && value.pointer("/params/processId").and_then(Value::as_str).is_some_and(|id| id.starts_with(HOOK_PROCESS_PREFIX))
                {
                    line.clear();
                    continue;
                }
                let text = std::str::from_utf8(&line)
                    .map_err(|_| anyhow::anyhow!("native_environment_invalid_output"))?;
                output.send(Message::Text(text.trim_end_matches(['\r','\n']).to_owned())).await?;
                line.clear();
            },
        }
    }
}

/// Cancellation-safe: retain partially read JSON while another select branch
/// forwards input. Bound allocation before appending to the buffer.
async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R, line: &mut Vec<u8>) -> Result<()> {
    loop {
        let bytes = reader.fill_buf().await?;
        ensure!(!bytes.is_empty(), "native_environment_stdout_closed");
        let newline = bytes.iter().position(|byte| *byte == b'\n');
        let length = newline.map_or(bytes.len(), |position| position + 1);
        ensure!(
            line.len() + length <= RPC_LIMIT,
            "native_environment_message_too_large"
        );
        line.extend_from_slice(&bytes[..length]);
        reader.consume(length);
        if newline.is_some() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL_BUDGET: WorkspaceBudget = WorkspaceBudget {
        bytes: 8,
        entries: 4,
        depth: 2,
    };

    #[tokio::test]
    async fn host_rpc_replies_and_process_output_are_separate_from_authority_traffic() {
        use tokio_tungstenite::{accept_async, connect_async};
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let client =
            tokio::spawn(async move { connect_async(format!("ws://{address}")).await.unwrap().0 });
        let (stream, _) = listener.accept().await.unwrap();
        let server = accept_async(stream).await.unwrap();
        let mut authority = client.await.unwrap();
        let (stdin, fake_stdin) = tokio::io::duplex(8192);
        let (mut fake_stdout, stdout) = tokio::io::duplex(8192);
        let (requests, receiver) = mpsc::unbounded_channel();
        let rpc = NativeEnvironmentRpc { requests };
        let bridge = tokio::spawn(forward(server, stdin, BufReader::new(stdout), receiver));
        let request = tokio::spawn(async move {
            rpc.request(
                "process/start",
                json!({"processId":format!("{HOOK_PROCESS_PREFIX}fixture")}),
            )
            .await
        });
        let mut fake_stdin = BufReader::new(fake_stdin);
        let mut line = String::new();
        fake_stdin.read_line(&mut line).await.unwrap();
        let host: Value = serde_json::from_str(&line).unwrap();
        assert!(host["id"].as_str().unwrap().starts_with(HOST_RPC_PREFIX));
        for message in [
            json!({"id":host["id"],"result":{"private":true}}),
            json!({"method":"process/output","params":{"processId":format!("{HOOK_PROCESS_PREFIX}fixture"),"chunk":"private"}}),
            json!({"method":"environment/ready","params":{}}),
        ] {
            fake_stdout
                .write_all(format!("{message}\n").as_bytes())
                .await
                .unwrap();
        }
        assert_eq!(request.await.unwrap().unwrap(), json!({"private":true}));
        let visible = authority
            .next()
            .await
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&visible).unwrap()["method"],
            "environment/ready"
        );
        authority
            .send(Message::Text(
                json!({"id":7,"method":"environment/info","params":{}}).to_string(),
            ))
            .await
            .unwrap();
        line.clear();
        fake_stdin.read_line(&mut line).await.unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["id"], 7);
        authority.send(Message::Text(json!({"id":8,"method":"process/terminate","params":{"processId":format!("{HOOK_PROCESS_PREFIX}fixture")}}).to_string())).await.unwrap();
        assert!(bridge.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn workspace_budget_checks_initial_bytes_entries_and_depth_without_following_links() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("outside"), b"too large for this budget").unwrap();
        symlink(outside.path(), directory.path().join("link")).unwrap();
        symlink("missing", directory.path().join("dangling")).unwrap();
        let monitor =
            WorkspaceMonitor::open(directory.path(), SMALL_BUDGET, Duration::from_millis(10))
                .await
                .unwrap();
        std::fs::write(directory.path().join("file"), b"123456789").unwrap();
        assert_eq!(
            monitor.check().await.unwrap_err().to_string(),
            "native_environment_workspace_bytes_exceeded"
        );
        assert!(
            WorkspaceMonitor::open(directory.path(), SMALL_BUDGET, Duration::from_millis(10))
                .await
                .is_err()
        );
        std::fs::remove_file(directory.path().join("file")).unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(directory.path().join(name), b"").unwrap();
        }
        assert_eq!(
            monitor.check().await.unwrap_err().to_string(),
            "native_environment_workspace_entries_exceeded"
        );

        let deep = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(deep.path().join("a/b/c")).unwrap();
        assert_eq!(
            WorkspaceMonitor::open(deep.path(), SMALL_BUDGET, Duration::from_millis(10))
                .await
                .err()
                .unwrap()
                .to_string(),
            "native_environment_workspace_depth_exceeded"
        );
    }

    #[tokio::test]
    async fn child_inherits_cpu_file_and_core_limits_and_cannot_extend_a_file() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "cat /proc/self/limits"]);
        apply_resource_limits(&mut command, 128 * 1024 * 1024);
        let output = command.output().await.unwrap();
        assert!(output.status.success());
        let limits = String::from_utf8(output.stdout).unwrap();
        for expected in [
            ["Max", "cpu", "time", "300", "300", "seconds"],
            ["Max", "file", "size", "134217728", "134217728", "bytes"],
            ["Max", "core", "file", "size", "0", "0"],
        ] {
            assert!(
                limits
                    .lines()
                    .any(|line| line.split_whitespace().take(6).eq(expected)),
                "{limits}"
            );
        }
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("bounded");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "trap '' XFSZ; printf 1234567890123456 > \"$1\"",
                "fixture",
            ])
            .arg(&target);
        apply_resource_limits(&mut command, 8);
        let output = command.output().await.unwrap();
        assert!(!output.status.success());
        assert!(std::fs::metadata(&target).unwrap().len() <= 8);
    }

    #[tokio::test]
    async fn workspace_budget_failure_disconnects_and_reaps_executor_before_releasing_lease() {
        use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};
        let directory = tempfile::tempdir().unwrap();
        let workspace =
            WorkspaceMonitor::open(directory.path(), SMALL_BUDGET, Duration::from_millis(10))
                .await
                .unwrap();
        let lease = WorkspaceLease::acquire(directory.path()).unwrap();
        let mut command = Command::new("/bin/cat");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().unwrap();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let url = format!("ws://{}/", listener.local_addr().unwrap());
        let (_cancel, cancelled) = oneshot::channel();
        let (failure_sender, mut failure) = watch::channel(None);
        let running = Arc::new(AtomicBool::new(true));
        let task = tokio::spawn(run_environment(
            child,
            listener,
            "fixture".into(),
            cancelled,
            running.clone(),
            lease,
            EnvironmentWatch {
                host_requests: mpsc::unbounded_channel().1,
                workspace,
                failure: failure_sender,
            },
        ));
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("authorization", "Bearer fixture".parse().unwrap());
        let (mut socket, _) = connect_async(request).await.unwrap();
        std::fs::write(directory.path().join("exceeded"), b"123456789").unwrap();
        timeout(Duration::from_secs(2), failure.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            failure.borrow().as_deref(),
            Some("native_environment_workspace_bytes_exceeded")
        );
        let result = timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            result.unwrap_err().to_string(),
            "native_environment_workspace_bytes_exceeded"
        );
        assert!(!running.load(Ordering::Acquire));
        assert!(WorkspaceLease::acquire(directory.path()).is_ok());
        assert!(matches!(
            timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap(),
            None | Some(Err(_)) | Some(Ok(Message::Close(_)))
        ));
    }

    #[test]
    fn lease_blocks_replacement_until_the_executor_owner_releases_it() {
        let directory = tempfile::tempdir().unwrap();
        let first = WorkspaceLease::acquire(directory.path()).unwrap();
        assert!(WorkspaceLease::acquire(directory.path()).is_err());
        drop(first);
        assert!(WorkspaceLease::acquire(directory.path()).is_ok());
    }

    #[tokio::test]
    async fn partial_json_is_preserved_across_read_cancellation() {
        let (mut writer, reader) = tokio::io::duplex(256);
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        writer.write_all(b"{\"id\":").await.unwrap();
        assert!(
            timeout(Duration::from_millis(10), read_line(&mut reader, &mut line))
                .await
                .is_err()
        );
        assert_eq!(line, b"{\"id\":");
        writer.write_all(b"1}\n{\"id\":2}\n").await.unwrap();
        read_line(&mut reader, &mut line).await.unwrap();
        assert_eq!(line, b"{\"id\":1}\n");
        line.clear();
        read_line(&mut reader, &mut line).await.unwrap();
        assert_eq!(line, b"{\"id\":2}\n");
    }

    #[test]
    fn bearer_comparison_does_not_accept_a_prefix_or_suffix() {
        assert!(token_matches("Bearer abc", "Bearer abc"));
        assert!(!token_matches("Bearer ab", "Bearer abc"));
        assert!(!token_matches("Bearer abcd", "Bearer abc"));
        assert!(!token_matches("Bearer abd", "Bearer abc"));
    }
}
