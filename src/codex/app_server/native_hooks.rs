//! Preserve Codex's hook engine while routing approved commands to one executor.
//!
//! The authority sees only a fixed command token and a strict shell-shaped IPC
//! proxy. Original commands and process environments never run on the authority.
//! This is transport and scope binding, not a second hook matching/result engine.

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rand::{RngCore, rngs::OsRng};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::oneshot,
    task::{JoinHandle, JoinSet},
    time::timeout,
};

use super::native_environment::{EXECUTION_ENVIRONMENT, HOOK_PROCESS_PREFIX, NativeEnvironmentRpc};

pub(crate) const HOOK_INPUT_BINARY: &str = "/runtime/clawmingle-hook-input";
pub(crate) const AUTHORITY_SOCKET_DIRECTORY: &str = "/run/clawmingle-hooks";
const TOKEN_PREFIX: &str = "clawmingle-hook:";
const INPUT_LIMIT: usize = 1024 * 1024;
const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
const SOURCE_LIMIT: usize = 256 * 1024;
const MAX_HANDLERS: usize = 32;
const MAX_CONCURRENT: usize = 16;
const REQUEST_MAGIC: &[u8; 8] = b"CMHOOK01";
const RESPONSE_MAGIC: &[u8; 8] = b"CMHOOR01";

// Public native event/config names, not matcher rules or output semantics.
const EVENTS: &[(&str, &str, &str)] = &[
    ("PreToolUse", "preToolUse", "pre_tool_use"),
    (
        "PermissionRequest",
        "permissionRequest",
        "permission_request",
    ),
    ("PostToolUse", "postToolUse", "post_tool_use"),
    ("PreCompact", "preCompact", "pre_compact"),
    ("PostCompact", "postCompact", "post_compact"),
    ("SessionStart", "sessionStart", "session_start"),
    ("SessionEnd", "sessionEnd", "session_end"),
    ("UserPromptSubmit", "userPromptSubmit", "user_prompt_submit"),
    ("SubagentStart", "subagentStart", "subagent_start"),
    ("SubagentStop", "subagentStop", "subagent_stop"),
    ("Stop", "stop", "stop"),
    ("Interrupt", "interrupt", "interrupt"),
];

struct HookBinding {
    command: String,
    event: &'static str,
    event_wire: &'static str,
    key_suffix: String,
}

struct BrokerState {
    bindings: BTreeMap<String, HookBinding>,
    // Bound once, before the task's native hooks can run. Never selected from
    // session_id, cwd, transcript_path, or other proxy-controlled input.
    executor: Mutex<Option<NativeEnvironmentRpc>>,
}

/// One task-owned worker. Do not reuse across scopes or authority processes.
/// The caller reads the bot's owner-only source; this never reads personal state.
/// Intentionally no Debug/Serialize: tokens and original commands stay private.
pub(crate) struct PreparedHooks {
    directory: Arc<tempfile::TempDir>,
    #[cfg(test)]
    document: Value,
    state: Arc<BrokerState>,
    cancel: Mutex<Option<oneshot::Sender<()>>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl PreparedHooks {
    pub(crate) async fn prepare(source: &str) -> Result<Self> {
        let (document, bindings) = prepare_document(source)?;
        let directory = Arc::new(
            tempfile::Builder::new()
                .prefix("clawmingle-hooks-")
                .tempdir()?,
        );
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        let file = directory.path().join("hooks.json");
        // The random owner-only directory is not writable by executor/model code.
        fs::write(&file, serde_json::to_vec(&document)?)?;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;
        let socket = directory.path().join("control.sock");
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        let state = Arc::new(BrokerState {
            bindings,
            executor: Mutex::new(None),
        });
        let (cancel, cancelled) = oneshot::channel();
        let task = tokio::spawn(serve(listener, state.clone(), directory.clone(), cancelled));
        Ok(Self {
            directory,
            #[cfg(test)]
            document,
            state,
            cancel: Mutex::new(Some(cancel)),
            task: Mutex::new(Some(task)),
        })
    }

    pub(crate) fn hooks_file(&self) -> PathBuf {
        self.directory.path().join("hooks.json")
    }

    pub(crate) fn socket_directory(&self) -> &Path {
        self.directory.path()
    }

    pub(crate) fn attach_executor(&self, executor: NativeEnvironmentRpc) -> Result<()> {
        let mut bound = self
            .state
            .executor
            .lock()
            .map_err(|_| anyhow::anyhow!("native_hook_binding_failed"))?;
        ensure!(bound.is_none(), "native_hook_executor_already_bound");
        *bound = Some(executor);
        Ok(())
    }

    /// Consume only the native hooks/list metadata for this exact rewritten file.
    /// The native engine owns normalization and hashing. No hash is reconstructed.
    /// Caller passes the worker-visible mount path, normally /state/codex/hooks.json.
    pub(crate) fn trust_state(&self, catalog: &Value, source_path: &str) -> Result<Value> {
        let data = catalog
            .get("data")
            .and_then(Value::as_array)
            .context("native_hook_catalog_invalid")?;
        ensure!(data.len() == 1, "native_hook_catalog_invalid");
        let row = &data[0];
        for key in ["errors", "warnings"] {
            ensure!(
                row.get(key)
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty),
                "native_hook_catalog_rejected"
            );
        }
        let entries = row
            .get("hooks")
            .and_then(Value::as_array)
            .context("native_hook_catalog_invalid")?;
        ensure!(
            entries.len() == self.state.bindings.len(),
            "native_hook_catalog_mismatch"
        );
        let mut states = Map::new();
        for metadata in entries {
            let command = metadata
                .get("command")
                .and_then(Value::as_str)
                .context("native_hook_catalog_mismatch")?;
            let token = command
                .strip_prefix(TOKEN_PREFIX)
                .context("native_hook_catalog_mismatch")?;
            let binding = self
                .state
                .bindings
                .get(token)
                .context("native_hook_catalog_mismatch")?;
            let key = format!("{source_path}:{}", binding.key_suffix);
            ensure!(
                metadata["handlerType"] == "command"
                    && metadata["eventName"] == binding.event_wire
                    && metadata["sourcePath"] == source_path
                    && metadata["key"] == key
                    && metadata["isManaged"] == false
                    && metadata["pluginId"].is_null(),
                "native_hook_catalog_mismatch"
            );
            let hash = metadata["currentHash"]
                .as_str()
                .context("native_hook_catalog_invalid")?;
            ensure!(
                hash.strip_prefix("sha256:").is_some_and(
                    |hex| hex.len() == 64 && hex.bytes().all(|c| c.is_ascii_hexdigit())
                ),
                "native_hook_catalog_invalid"
            );
            ensure!(
                states
                    .insert(key, json!({"enabled":true,"trusted_hash":hash}))
                    .is_none(),
                "native_hook_catalog_mismatch"
            );
        }
        Ok(Value::Object(states))
    }

    #[cfg(test)]
    pub(crate) fn document(&self) -> &Value {
        &self.document
    }

    pub(crate) async fn shutdown(&self) {
        let cancel = self.cancel.lock().ok().and_then(|mut value| value.take());
        if let Some(cancel) = cancel {
            let _ = cancel.send(());
        }
        let task = self.task.lock().ok().and_then(|mut value| value.take());
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

impl Drop for PreparedHooks {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.get_mut().ok().and_then(Option::take) {
            let _ = cancel.send(());
        }
    }
}

/// Validate only: no filesystem access, process launch, or entropy consumption.
pub(crate) fn validate_source(source: &str) -> Result<()> {
    let mut next = 0_u64;
    transform_document(source, &mut || {
        next += 1;
        format!("{next:032x}")
    })
    .map(|_| ())
}

fn prepare_document(source: &str) -> Result<(Value, BTreeMap<String, HookBinding>)> {
    transform_document(source, &mut random_token)
}

fn transform_document(
    source: &str,
    token: &mut impl FnMut() -> String,
) -> Result<(Value, BTreeMap<String, HookBinding>)> {
    ensure!(source.len() <= SOURCE_LIMIT, "native_hook_source_too_large");
    let mut document: Value =
        serde_json::from_str(source).map_err(|_| anyhow::anyhow!("native_hook_source_invalid"))?;
    let root = document
        .as_object_mut()
        .context("native_hook_source_invalid")?;
    ensure!(
        root.keys()
            .all(|k| matches!(k.as_str(), "description" | "hooks")),
        "native_hook_source_unsupported_field"
    );
    ensure!(
        root.get("description").is_none_or(Value::is_string),
        "native_hook_source_invalid"
    );
    let events = root
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .context("native_hook_source_invalid")?;
    let mut bindings = BTreeMap::new();
    for (event_name, groups) in events {
        let &(event, event_wire, key_label) = EVENTS
            .iter()
            .find(|(name, _, _)| *name == event_name)
            .context("native_hook_event_unsupported")?;
        let groups = groups
            .as_array_mut()
            .context("native_hook_source_invalid")?;
        for (group_index, group) in groups.iter_mut().enumerate() {
            let group = group
                .as_object_mut()
                .context("native_hook_source_invalid")?;
            ensure!(
                group
                    .keys()
                    .all(|k| matches!(k.as_str(), "matcher" | "hooks"))
                    && group.get("matcher").is_none_or(Value::is_string),
                "native_hook_source_unsupported_field"
            );
            let handlers = group
                .get_mut("hooks")
                .and_then(Value::as_array_mut)
                .context("native_hook_source_invalid")?;
            for (handler_index, handler) in handlers.iter_mut().enumerate() {
                ensure!(bindings.len() < MAX_HANDLERS, "native_hook_handler_limit");
                let handler = handler
                    .as_object_mut()
                    .context("native_hook_source_invalid")?;
                ensure!(
                    handler.get("type").and_then(Value::as_str) == Some("command"),
                    "native_hook_handler_unsupported"
                );
                ensure!(
                    handler.keys().all(|k| matches!(
                        k.as_str(),
                        "type"
                            | "command"
                            | "timeout"
                            | "async"
                            | "statusMessage"
                            | "additionalContextLimit"
                    )),
                    "native_hook_source_unsupported_field"
                );
                let command = handler
                    .get("command")
                    .and_then(Value::as_str)
                    .context("native_hook_source_invalid")?
                    .to_owned();
                ensure!(
                    !command.trim().is_empty()
                        && !command.contains('\0')
                        && command.len() <= 65_536,
                    "native_hook_command_invalid"
                );
                ensure!(
                    handler.get("async").is_none_or(Value::is_boolean)
                        && handler.get("statusMessage").is_none_or(Value::is_string)
                        && handler
                            .get("additionalContextLimit")
                            .is_none_or(|v| v.as_u64().is_some())
                        && handler
                            .get("timeout")
                            .is_none_or(|v| v.as_u64().is_some_and(|v| (1..=600).contains(&v))),
                    "native_hook_source_invalid"
                );
                let token = token();
                handler.insert(
                    "command".into(),
                    Value::String(format!("{TOKEN_PREFIX}{token}")),
                );
                bindings.insert(
                    token,
                    HookBinding {
                        command,
                        event,
                        event_wire,
                        key_suffix: format!("{key_label}:{group_index}:{handler_index}"),
                    },
                );
            }
        }
    }
    ensure!(!bindings.is_empty(), "native_hook_source_empty");
    Ok((document, bindings))
}

fn random_token() -> String {
    let mut token = [0; 16];
    OsRng.fill_bytes(&mut token);
    token.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn serve(
    listener: UnixListener,
    state: Arc<BrokerState>,
    _directory: Arc<tempfile::TempDir>,
    mut cancelled: oneshot::Receiver<()>,
) {
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut cancelled => break,
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break; };
                if tasks.len() >= MAX_CONCURRENT { drop(stream); continue; }
                let state = state.clone();
                tasks.spawn(async move {
                    // Native hook timers remain authoritative; this is only an
                    // outer ceiling for a malformed/non-native IPC peer.
                    let _ = timeout(Duration::from_secs(610), handle(stream, &state)).await;
                });
            },
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

async fn handle(mut stream: UnixStream, state: &BrokerState) -> Result<()> {
    let (token, input) = timeout(Duration::from_secs(5), read_request(&mut stream))
        .await
        .map_err(|_| anyhow::anyhow!("native_hook_ipc_timeout"))??;
    let binding = state
        .bindings
        .get(&token)
        .context("native_hook_binding_unknown")?;
    validate_input(&input, binding.event)?;
    let rpc = state
        .executor
        .lock()
        .map_err(|_| anyhow::anyhow!("native_hook_binding_failed"))?
        .clone()
        .context("native_hook_executor_unavailable")?;
    let operation = execute(&rpc, &binding.command, input);
    tokio::pin!(operation);
    let mut extra = [0];
    let result = tokio::select! {
        result = &mut operation => result,
        _ = stream.read(&mut extra) => Err(anyhow::anyhow!("native_hook_proxy_closed")),
    };
    let (code, stdout, stderr) = match result {
        Ok(output) => output,
        // No native hook decision is fabricated. A transport failure is an
        // ordinary hook failure, interpreted by the native engine itself.
        Err(_) => (
            1,
            Vec::new(),
            b"ClawMingle hook executor unavailable or cancelled".to_vec(),
        ),
    };
    stream.write_all(RESPONSE_MAGIC).await?;
    stream.write_i32(code).await?;
    stream.write_u32(stdout.len() as u32).await?;
    stream.write_u32(stderr.len() as u32).await?;
    stream.write_all(&stdout).await?;
    stream.write_all(&stderr).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn read_request(stream: &mut UnixStream) -> Result<(String, Vec<u8>)> {
    let mut magic = [0; 8];
    stream.read_exact(&mut magic).await?;
    ensure!(&magic == REQUEST_MAGIC, "native_hook_ipc_invalid");
    let mut token = [0; 32];
    stream.read_exact(&mut token).await?;
    ensure!(
        token.iter().all(u8::is_ascii_hexdigit),
        "native_hook_ipc_invalid"
    );
    let size = stream.read_u32().await? as usize;
    ensure!(size <= INPUT_LIMIT, "native_hook_input_too_large");
    let mut input = vec![0; size];
    stream.read_exact(&mut input).await?;
    Ok((String::from_utf8(token.to_vec())?, input))
}

fn validate_input(input: &[u8], event: &str) -> Result<()> {
    let payload: Value =
        serde_json::from_slice(input).map_err(|_| anyhow::anyhow!("native_hook_input_invalid"))?;
    ensure!(
        payload.is_object()
            && payload
                .get("session_id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty() && id.len() <= 128)
            && payload.get("hook_event_name").and_then(Value::as_str) == Some(event),
        "native_hook_input_invalid"
    );
    Ok(())
}

struct ProcessCleanup<'a> {
    rpc: &'a NativeEnvironmentRpc,
    id: String,
}
impl Drop for ProcessCleanup<'_> {
    fn drop(&mut self) {
        self.rpc.terminate(&self.id);
    }
}

async fn execute(
    rpc: &NativeEnvironmentRpc,
    command: &str,
    input: Vec<u8>,
) -> Result<(i32, Vec<u8>, Vec<u8>)> {
    let process = ProcessCleanup {
        rpc,
        id: format!("{HOOK_PROCESS_PREFIX}{}", random_token()),
    };
    let environment: BTreeMap<_, _> = EXECUTION_ENVIRONMENT.iter().copied().collect();
    // The fixed runner consumes a length frame and closes the real hook's stdin.
    // Codex 0.161 process/write cannot itself signal stdin EOF.
    let launched = rpc
        .request(
            "process/start",
            json!({
                "processId":process.id,"argv":[HOOK_INPUT_BINARY],"cwd":"file:///workspace",
                "env":environment,"tty":false,"pipeStdin":true,"arg0":null
            }),
        )
        .await?;
    ensure!(
        launched["processId"] == process.id,
        "native_hook_process_mismatch"
    );
    let mut frame = Vec::with_capacity(8 + command.len() + input.len());
    frame.extend_from_slice(&(command.len() as u32).to_be_bytes());
    frame.extend_from_slice(&(input.len() as u32).to_be_bytes());
    frame.extend_from_slice(command.as_bytes());
    frame.extend_from_slice(&input);
    let written = rpc
        .request(
            "process/write",
            json!({"processId":process.id,"writeId":random_token(),"chunk":STANDARD.encode(frame)}),
        )
        .await?;
    ensure!(written["status"] == "accepted", "native_hook_stdin_failed");
    let mut after = 0_u64;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit = None;
    loop {
        let response = rpc
            .request(
                "process/read",
                json!({"processId":process.id,"afterSeq":after,"maxBytes":65_536,"waitMs":100}),
            )
            .await?;
        ensure!(
            response.get("failure").is_none_or(Value::is_null),
            "native_hook_process_failed"
        );
        let chunks = response["chunks"]
            .as_array()
            .context("native_hook_output_invalid")?;
        for chunk in chunks {
            let bytes = STANDARD.decode(
                chunk["chunk"]
                    .as_str()
                    .context("native_hook_output_invalid")?,
            )?;
            ensure!(
                stdout.len() + stderr.len() + bytes.len() <= OUTPUT_LIMIT,
                "native_hook_output_too_large"
            );
            match chunk["stream"].as_str() {
                Some("stdout") => stdout.extend_from_slice(&bytes),
                Some("stderr") => stderr.extend_from_slice(&bytes),
                _ => anyhow::bail!("native_hook_output_invalid"),
            }
        }
        after = response["nextSeq"]
            .as_u64()
            .context("native_hook_output_invalid")?
            .saturating_sub(1);
        if let Some(code) = response["exitCode"].as_i64() {
            exit = Some(i32::try_from(code)?);
        }
        if response["closed"] == true && chunks.is_empty() {
            return Ok((exit.context("native_hook_exit_missing")?, stdout, stderr));
        }
        // Native process/read can return immediately once exited but descendants
        // still hold pipes. Avoid spinning until closed or the native timeout.
        if response["exited"] == true {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"cat; exit 2","timeout":7,"async":false}]}]}}"#;

    #[test]
    fn only_commands_are_replaced_and_native_rules_are_retained() {
        let (document, bindings) = prepare_document(SOURCE).unwrap();
        let handler = &document["hooks"]["PreToolUse"][0]["hooks"][0];
        assert_eq!(document["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(handler["timeout"], 7);
        assert_eq!(handler["async"], false);
        assert!(
            handler["command"]
                .as_str()
                .unwrap()
                .starts_with(TOKEN_PREFIX)
        );
        assert!(!document.to_string().contains("cat; exit"));
        assert_eq!(bindings.values().next().unwrap().command, "cat; exit 2");
    }

    #[test]
    fn unsupported_or_self_authorizing_sources_fail_closed() {
        for source in [
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"mcp_tool","server":"private","tool":"send"}]}]}}"#,
            r#"{"hooks":{"Unknown":[]}}"#,
            r#"{"hooks":{},"state":{"anything":{"trusted_hash":"sha256:untrusted"}}}"#,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"true","env":{"KEY":"value"}}]}]}}"#,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"true","commandWindows":"evil"}]}]}}"#,
        ] {
            assert!(prepare_document(source).is_err(), "{source}");
        }
    }

    #[test]
    fn input_identifies_event_but_cannot_choose_an_executor() {
        assert!(validate_input(br#"{"session_id":"thread","hook_event_name":"PreToolUse","cwd":"/outside","environment_id":"other"}"#, "PreToolUse").is_ok());
        assert!(
            validate_input(
                br#"{"session_id":"thread","hook_event_name":"Stop"}"#,
                "PreToolUse"
            )
            .is_err()
        );
        assert!(validate_input(br#"{"hook_event_name":"PreToolUse"}"#, "PreToolUse").is_err());
    }

    #[tokio::test]
    async fn native_metadata_trusts_only_exact_generated_file_and_entries() {
        let hooks = PreparedHooks::prepare(SOURCE).await.unwrap();
        let command = hooks.document()["hooks"]["PreToolUse"][0]["hooks"][0]["command"].clone();
        let source = "/state/codex/hooks.json";
        let mut catalog = json!({"data":[{"errors":[],"warnings":[],"hooks":[{
            "handlerType":"command","command":command,"eventName":"preToolUse",
            "sourcePath":source,"key":format!("{source}:pre_tool_use:0:0"),"pluginId":null,
            "isManaged":false,"currentHash":format!("sha256:{}","a".repeat(64))
        }]}]});
        assert!(hooks.trust_state(&catalog, source).is_ok());
        catalog["data"][0]["hooks"][0]["sourcePath"] = json!("/other/hooks.json");
        assert!(hooks.trust_state(&catalog, source).is_err());
        hooks.shutdown().await;
    }

    #[tokio::test]
    async fn private_material_and_oversized_ipc_are_rejected_without_allocating() {
        let hooks = PreparedHooks::prepare(SOURCE).await.unwrap();
        assert_eq!(
            fs::metadata(hooks.socket_directory())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(hooks.hooks_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.write_all(REQUEST_MAGIC).await.unwrap();
        writer
            .write_all(b"0123456789abcdef0123456789abcdef")
            .await
            .unwrap();
        writer.write_u32((INPUT_LIMIT + 1) as u32).await.unwrap();
        assert!(read_request(&mut reader).await.is_err());
        hooks.shutdown().await;
    }

    #[tokio::test]
    async fn executor_frame_is_exact_and_closed_output_is_drained_without_skipping_chunks() {
        let input = br#"{"session_id":"thread","hook_event_name":"PreToolUse"}"#.to_vec();
        let expected = input.clone();
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = reads.clone();
        let rpc = NativeEnvironmentRpc::mock(move |method, params| match method {
            "process/start" => {
                assert_eq!(params["argv"], json!([HOOK_INPUT_BINARY]));
                assert_eq!(params["cwd"], "file:///workspace");
                assert_eq!(params["env"]["HOME"], "/home/worker");
                assert_eq!(
                    params["env"].as_object().unwrap().len(),
                    EXECUTION_ENVIRONMENT.len()
                );
                Ok(json!({"processId":params["processId"]}))
            }
            "process/write" => {
                let frame = STANDARD.decode(params["chunk"].as_str().unwrap()).unwrap();
                assert_eq!(u32::from_be_bytes(frame[..4].try_into().unwrap()), 3);
                assert_eq!(&frame[8..11], b"cat");
                assert_eq!(&frame[11..], expected);
                Ok(json!({"status":"accepted"}))
            }
            "process/read" => {
                let index = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert_eq!(params["afterSeq"], index as u64);
                let chunks = match index {
                    0 => json!([{"seq":1,"stream":"stdout","chunk":STANDARD.encode("first")}]),
                    1 => json!([{"seq":2,"stream":"stderr","chunk":STANDARD.encode("second")}]),
                    _ => json!([]),
                };
                Ok(
                    json!({"chunks":chunks,"nextSeq":index+2,"closed":true,"exited":true,"exitCode":2,"failure":null}),
                )
            }
            "process/terminate" => Ok(json!({"running":false})),
            _ => panic!("unexpected method"),
        });
        let (code, stdout, stderr) = execute(&rpc, "cat", input).await.unwrap();
        assert_eq!(
            (code, stdout, stderr),
            (2, b"first".to_vec(), b"second".to_vec())
        );
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn cancelling_an_inflight_hook_queues_native_process_termination() {
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let rpc = NativeEnvironmentRpc::mock(move |method, params| {
            let _ = events.send(method.to_owned());
            Ok(match method {
                "process/start" => json!({"processId":params["processId"]}),
                "process/write" => json!({"status":"accepted"}),
                "process/read" => {
                    json!({"chunks":[],"nextSeq":1,"closed":false,"exited":true,"exitCode":0})
                }
                "process/terminate" => json!({"running":true}),
                _ => panic!("unexpected method"),
            })
        });
        let task = tokio::spawn(async move { execute(&rpc, "sleep 60", b"{}".to_vec()).await });
        while receiver.recv().await.unwrap() != "process/read" {}
        task.abort();
        let _ = task.await;
        let terminated = timeout(Duration::from_secs(1), async {
            while receiver.recv().await.unwrap() != "process/terminate" {}
        })
        .await;
        assert!(terminated.is_ok());
    }
}
