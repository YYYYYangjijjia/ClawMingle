//! One-shot, network-isolated execution in one conversation's workspace.
//! The parent must also restrict `workspace` to its own hashed scope directory.

use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    sync::{Semaphore, watch},
    time::{Instant, sleep_until},
};

use super::workspace_runtime::{RuntimeTools, append_runtime_mounts, validated_native_binary};

static WORKSPACE_SLOT: Semaphore = Semaphore::const_new(1);
const OUTPUT_LIMIT: usize = 16 * 1024;
const JSON_LIMIT: usize = 64 * 1024;
const READY_MARKER: &[u8] = b"\x1eCM_EXEC_READY\x1f\n";
const LAUNCH_SCRIPT: &str = r#"printf '\036CM_EXEC_READY\037\n'; exec "$@""#;

pub const TOOL_DESCRIPTION: &str = "在当前聊天独立工作区运行通用命令、Python、Node 或 Git，读取和处理该工作区中的文件。系统运行环境只读，无网络、登录凭据、个人文件或其他聊天文件。默认 20 秒；实际单次时限受会话授权上限约束，最多 300 秒，输出合计最多 16 KiB；超时或取消后文件可能已部分修改，不会自动重试。";

pub fn tool_definition() -> Value {
    json!({"name": "workspace_exec", "description": TOOL_DESCRIPTION,
        "inputSchema": {"type": "object", "additionalProperties": false,
            "properties": {"command": {"type": "array", "minItems": 1, "maxItems": 32,
                "items": {"type": "string"}, "description": "参数数组，合计最多 8 KiB；例如 python3 -c 或 bash -c。"},
                "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 300, "default": 20}},
            "required": ["command"]}})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    command: Vec<String>,
    #[serde(default = "default_timeout")]
    timeout_seconds: u64,
}

fn default_timeout() -> u64 {
    20
}

#[cfg(test)]
fn parse_arguments(raw: &str) -> Result<Arguments> {
    parse_arguments_with_limit(raw, 30)
}

fn parse_arguments_with_limit(raw: &str, limit: u64) -> Result<Arguments> {
    ensure!(
        (1..=300).contains(&limit),
        "invalid workspace timeout limit"
    );
    ensure!(
        raw.len() <= JSON_LIMIT && raw.trim_start().starts_with('{'),
        "invalid JSON object"
    );
    let value: Value = serde_json::from_str(raw).context("invalid execution arguments")?;
    let explicit = value.get("timeout_seconds").is_some();
    let mut arguments: Arguments =
        serde_json::from_value(value).context("invalid execution arguments")?;
    if !explicit {
        arguments.timeout_seconds = arguments.timeout_seconds.min(limit);
    }
    ensure!(
        (1..=32).contains(&arguments.command.len()),
        "invalid argument count"
    );
    ensure!(
        !arguments.command[0].trim().is_empty(),
        "missing executable"
    );
    ensure!(
        arguments.command.iter().all(|item| !item.contains('\0')),
        "NUL in argument"
    );
    ensure!(
        arguments.command.iter().map(String::len).sum::<usize>() <= 8192,
        "arguments too large"
    );
    ensure!(
        (1..=limit).contains(&arguments.timeout_seconds),
        "invalid timeout"
    );
    Ok(arguments)
}

/// Cancellation is handled here so the child is killed and reaped before the
/// single execution slot is released. The parent must signal the watch channel,
/// rather than dropping this future to cancel a running command.
#[cfg(test)]
pub async fn execute(
    workspace: &Path,
    bubblewrap: &Path,
    arguments: &str,
    cancel: Option<watch::Receiver<bool>>,
) -> Value {
    execute_with_skills(workspace, bubblewrap, arguments, cancel, &[], 30).await
}

pub async fn execute_with_skills(
    workspace: &Path,
    bubblewrap: &Path,
    arguments: &str,
    mut cancel: Option<watch::Receiver<bool>>,
    skills: &[PathBuf],
    timeout_limit: u64,
) -> Value {
    if cancellation_requested(cancel.as_ref()) {
        return early_error("cancelled", "执行已取消，未启动命令。");
    }
    let arguments = match parse_arguments_with_limit(arguments, timeout_limit) {
        Ok(value) => value,
        Err(_) => {
            return early_error(
                "invalid_arguments",
                "命令参数无效：请提供 1 至 32 项参数、合计不超过 8 KiB，以及当前会话授权范围内的超时（最高300秒）。",
            );
        }
    };
    let _slot = match WORKSPACE_SLOT.try_acquire() {
        Ok(permit) => permit,
        Err(_) => return early_error("busy", "另一个工作区命令尚未清理完成，请稍后再试。"),
    };
    if cancellation_requested(cancel.as_ref()) {
        return early_error("cancelled", "执行已取消，未启动命令。");
    }
    let workspace = match validate_workspace(workspace) {
        Ok(path) => path,
        Err(_) => return early_error("invalid_workspace", "当前聊天的独立工作区未通过权限检查。"),
    };
    let bubblewrap = match validated_native_binary(bubblewrap) {
        Ok(path) => path,
        Err(_) => return early_error("runtime_unavailable", "隔离执行器未通过检查。"),
    };
    let (mut command, tools) =
        match build_command_with_skills(&workspace, &bubblewrap, &arguments, skills) {
            Ok(value) => value,
            Err(_) => return early_error("runtime_unavailable", "只读运行环境未通过检查。"),
        };
    if cancellation_requested(cancel.as_ref()) {
        return early_error("cancelled", "执行已取消，未启动命令。");
    }
    let started = Instant::now();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return early_error("spawn_failed", "无法启动隔离命令。"),
    };
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        kill_and_reap(&mut child).await;
        return early_error("spawn_failed", "无法读取隔离命令输出。");
    };
    let deadline = started + Duration::from_secs(arguments.timeout_seconds);
    let mut capture = Capture::default();
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut status = None;
    let mut stdout_buffer = [0; 4096];
    let mut stderr_buffer = [0; 4096];
    while stdout_open || stderr_open || status.is_none() {
        tokio::select! {
            biased;
            _ = wait_for_cancellation(&mut cancel) => {
                kill_and_reap(&mut child).await;
                return completed_error("cancelled", "执行已取消；工作区文件可能已经部分修改，请核对后再继续。", capture, started, tools, false);
            }
            _ = sleep_until(deadline) => {
                kill_and_reap(&mut child).await;
                return completed_error("timeout", "命令执行超时并已停止；工作区文件可能已经部分修改，不会自动重试。", capture, started, tools, false);
            }
            result = stdout.read(&mut stdout_buffer), if stdout_open => {
                match result {
                    Ok(0) => stdout_open = false,
                    Ok(count) => match capture.stdout(&stdout_buffer[..count]) {
                        Ok(true) => {},
                        Ok(false) => {
                            kill_and_reap(&mut child).await;
                            return completed_error("output_limit", "输出超过 16 KiB，命令已停止；工作区文件可能已经部分修改。", capture, started, tools, true);
                        }
                        Err(()) => {
                            kill_and_reap(&mut child).await;
                            return early_error("sandbox_failed", "隔离环境未能启动命令。");
                        }
                    },
                    Err(_) => {
                        kill_and_reap(&mut child).await;
                        return completed_error("output_failed", "无法继续读取命令输出，执行已停止；请检查工作区文件。", capture, started, tools, false);
                    }
                }
            }
            result = stderr.read(&mut stderr_buffer), if stderr_open => {
                match result {
                    Ok(0) => stderr_open = false,
                    Ok(count) => {
                        if !capture.stderr(&stderr_buffer[..count]) {
                            kill_and_reap(&mut child).await;
                            return completed_error("output_limit", "输出超过 16 KiB，命令已停止；工作区文件可能已经部分修改。", capture, started, tools, true);
                        }
                    }
                    Err(_) => {
                        kill_and_reap(&mut child).await;
                        return completed_error("output_failed", "无法继续读取命令输出，执行已停止；请检查工作区文件。", capture, started, tools, false);
                    }
                }
            }
            result = child.wait(), if status.is_none() => {
                match result {
                    Ok(value) => status = Some(value),
                    Err(_) => {
                        kill_and_reap(&mut child).await;
                        return completed_error("wait_failed", "无法确认命令退出状态，已停止执行；请检查工作区文件。", capture, started, tools, false);
                    }
                }
            }
        }
    }
    if !capture.ready {
        // Do not forward bwrap setup diagnostics containing host mount paths.
        return early_error("sandbox_failed", "隔离环境未能启动命令。");
    }
    let Some(status) = status else {
        return early_error("wait_failed", "无法确认命令退出状态。");
    };
    completed(status, capture, started, tools)
}

fn cancellation_requested(cancel: Option<&watch::Receiver<bool>>) -> bool {
    cancel.is_some_and(|receiver| *receiver.borrow() || receiver.has_changed().is_err())
}

async fn wait_for_cancellation(cancel: &mut Option<watch::Receiver<bool>>) {
    let Some(receiver) = cancel else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if *receiver.borrow() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

async fn kill_and_reap(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(target_os = "linux")]
pub(super) fn validate_workspace(path: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    ensure!(
        path.is_absolute() && path != Path::new("/"),
        "invalid workspace root"
    );
    ensure!(
        path.components()
            .all(|part| !matches!(part, Component::CurDir | Component::ParentDir)),
        "workspace must be normalized"
    );
    let uid = fs::metadata("/proc/self")?.uid();
    let mut part = PathBuf::new();
    for component in path.components() {
        part.push(component);
        let metadata = fs::symlink_metadata(&part).context("workspace ancestor is unavailable")?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "workspace ancestors cannot be links"
        );
        ensure!(
            (metadata.uid() == 0 || metadata.uid() == uid)
                && metadata.permissions().mode() & 0o022 == 0,
            "workspace ancestor is not trusted"
        );
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.uid() == uid && metadata.permissions().mode() & 0o777 == 0o700,
        "workspace must be privately owned mode 0700"
    );
    Ok(path.to_owned())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn validate_workspace(_path: &Path) -> Result<PathBuf> {
    anyhow::bail!("workspace execution requires Linux")
}

fn build_command_with_skills(
    workspace: &Path,
    bubblewrap: &Path,
    arguments: &Arguments,
    skills: &[PathBuf],
) -> Result<(Command, RuntimeTools)> {
    ensure!(skills.len() <= 16, "social_skill_limit");
    for path in skills {
        crate::util::skill_package::inspect(path)?;
    }
    let mut command = Command::new(bubblewrap);
    command
        .env_clear()
        .current_dir("/")
        .kill_on_drop(true)
        .stdin(Stdio::null())
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
    for path in ["/usr", "/home", "/home/worker", "/workspace"] {
        command.args(["--dir", path]);
    }
    let tools = append_runtime_mounts(&mut command)?;
    command.args(["--dir", "/skills"]);
    let mut names = std::collections::BTreeSet::new();
    for path in skills {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("skill_name_invalid"))?;
        ensure!(
            crate::util::skill_package::valid_name(name) && names.insert(name),
            "skill_name_invalid"
        );
        command
            .arg("--ro-bind")
            .arg(path)
            .arg(format!("/skills/{name}"));
    }
    command.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
    for (key, value) in [
        ("PATH", "/tools:/usr/bin"),
        ("HOME", "/home/worker"),
        ("TMPDIR", "/tmp"),
        ("LANG", "C.UTF-8"),
        ("TZ", "Asia/Shanghai"),
    ] {
        command.env(key, value);
    }
    // Last filesystem operation: never construct nested mountpoints in this
    // persistent writable source tree. It is the only host-writable bind.
    command.arg("--bind").arg(workspace).arg("/workspace");
    command.args([
        "--chdir",
        "/workspace",
        "--",
        "/usr/bin/sh",
        "-c",
        LAUNCH_SCRIPT,
        "workspace-exec",
    ]);
    command.args(&arguments.command);
    apply_resource_limits(&mut command);
    Ok((command, tools))
}

#[cfg(target_os = "linux")]
fn apply_resource_limits(command: &mut Command) {
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
    // Only stack values and libc syscalls in the post-fork hook. No AS limit:
    // Node/V8 reserves a large virtual address range even for tiny programs.
    unsafe {
        command.as_std_mut().pre_exec(|| {
            for (resource, maximum) in [(0, 10), (1, 8 * 1024 * 1024), (4, 0)] {
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

#[cfg(not(target_os = "linux"))]
fn apply_resource_limits(_command: &mut Command) {}

#[derive(Default)]
struct Capture {
    ready: bool,
    marker: Vec<u8>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Capture {
    fn stdout(&mut self, mut bytes: &[u8]) -> std::result::Result<bool, ()> {
        if !self.ready {
            let count = (READY_MARKER.len() - self.marker.len()).min(bytes.len());
            self.marker.extend_from_slice(&bytes[..count]);
            if !READY_MARKER.starts_with(&self.marker) {
                return Err(());
            }
            bytes = &bytes[count..];
            if self.marker.len() != READY_MARKER.len() {
                return Ok(true);
            }
            self.ready = true;
        }
        Ok(self.append(bytes, true))
    }

    fn stderr(&mut self, bytes: &[u8]) -> bool {
        self.append(bytes, false)
    }

    fn append(&mut self, bytes: &[u8], stdout: bool) -> bool {
        let remaining = OUTPUT_LIMIT - self.stdout.len() - self.stderr.len();
        let buffer = if stdout {
            &mut self.stdout
        } else {
            &mut self.stderr
        };
        buffer.extend_from_slice(&bytes[..remaining.min(bytes.len())]);
        bytes.len() <= remaining
    }

    fn output(&self) -> (String, String) {
        if !self.ready {
            return (String::new(), String::new());
        }
        let stdout = decoded_prefix(&self.stdout, OUTPUT_LIMIT);
        let stderr = decoded_prefix(&self.stderr, OUTPUT_LIMIT - stdout.len());
        (stdout, stderr)
    }
}

fn decoded_prefix(bytes: &[u8], limit: usize) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn early_error(code: &str, message: &str) -> Value {
    json!({"ok": false, "error": {"code": code, "message": message}})
}

fn completed_error(
    code: &str,
    message: &str,
    capture: Capture,
    started: Instant,
    tools: RuntimeTools,
    truncated: bool,
) -> Value {
    if !capture.ready && code != "cancelled" && code != "timeout" {
        return early_error("sandbox_failed", "隔离环境未能启动命令。");
    }
    let (stdout, stderr) = capture.output();
    json!({"ok": false, "error": {"code": code, "message": message}, "stdout": stdout,
        "stderr": stderr, "truncated": truncated, "elapsed_ms": started.elapsed().as_millis(),
        "runtime_tools": {"node": tools.node, "rg": tools.rg}, "automatic_retry": false})
}

fn completed(status: ExitStatus, capture: Capture, started: Instant, tools: RuntimeTools) -> Value {
    let (stdout, stderr) = capture.output();
    let mut value = json!({"ok": status.success(), "exit_code": status.code(), "stdout": stdout,
        "stderr": stderr, "truncated": false, "elapsed_ms": started.elapsed().as_millis(),
        "runtime_tools": {"node": tools.node, "rg": tools.rg}, "automatic_retry": false});
    if !status.success() {
        value["error"] = json!({"code": "command_failed", "message": "命令以非成功状态退出；工作区文件可能已经部分修改，请核对后再继续。"});
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_timeout_ceiling_is_not_model_expandable() {
        let raw = r#"{"command":["true"],"timeout_seconds":120}"#;
        assert!(parse_arguments_with_limit(raw, 120).is_ok());
        assert!(parse_arguments_with_limit(raw, 30).is_err());
        assert!(parse_arguments_with_limit(raw, 301).is_err());
        assert_eq!(
            parse_arguments_with_limit(r#"{"command":["true"]}"#, 1)
                .unwrap()
                .timeout_seconds,
            1
        );
    }

    #[test]
    fn strict_arguments_allow_general_code_and_preserve_argv() {
        let arguments =
            parse_arguments(r#"{"command":["python3","-c","print('hello')",""]}"#).unwrap();
        assert_eq!(arguments.command, ["python3", "-c", "print('hello')", ""]);
        assert_eq!(arguments.timeout_seconds, 20);
        assert!(
            parse_arguments(r#"{"command":["bash","-c","echo ok"],"timeout_seconds":30}"#).is_ok()
        );
    }

    #[test]
    fn arguments_reject_extra_authority_and_out_of_bounds_values() {
        for value in [
            "[]",
            "null",
            r#"{"command":[]}"#,
            r#"{"command":[""]}"#,
            r#"{"command":["true"],"cwd":"/"}"#,
            r#"{"command":["true"],"env":{}}"#,
            r#"{"command":["true"],"timeout_seconds":0}"#,
            r#"{"command":["true"],"timeout_seconds":31}"#,
            r#"{"command":["true"],"timeout_seconds":-1}"#,
            r#"{"command":["true\u0000"]}"#,
        ] {
            assert!(parse_arguments(value).is_err());
        }
        assert!(parse_arguments(&json!({"command":vec!["x";33]}).to_string()).is_err());
        assert!(parse_arguments(&json!({"command":["x".repeat(8193)]}).to_string()).is_err());
        assert!(parse_arguments(&" ".repeat(JSON_LIMIT + 1)).is_err());
    }

    #[test]
    fn readiness_is_removed_even_across_arbitrary_chunks() {
        let mut capture = Capture::default();
        for byte in READY_MARKER {
            assert_eq!(capture.stdout(&[*byte]), Ok(true));
        }
        assert_eq!(capture.stdout(b"hello"), Ok(true));
        assert!(capture.stderr(b"warning"));
        assert_eq!(capture.output(), ("hello".into(), "warning".into()));
    }

    #[test]
    fn startup_failures_never_return_host_diagnostics() {
        let mut capture = Capture::default();
        assert!(capture.stderr(b"fixture sandbox setup details"));
        assert_eq!(capture.output(), (String::new(), String::new()));
        assert_eq!(capture.stdout(b"unexpected"), Err(()));
    }

    #[test]
    fn combined_output_limit_is_shared_and_exact() {
        let mut capture = Capture::default();
        capture.stdout(READY_MARKER).unwrap();
        assert!(capture.stderr(&vec![b'e'; OUTPUT_LIMIT - 3]));
        assert_eq!(capture.stdout(b"out"), Ok(true));
        assert_eq!(capture.stdout(b"extra"), Ok(false));
        let (stdout, stderr) = capture.output();
        assert_eq!(stdout.len() + stderr.len(), OUTPUT_LIMIT);
    }

    #[test]
    fn binary_output_remains_valid_utf8_within_the_text_budget() {
        let mut capture = Capture::default();
        capture.stdout(READY_MARKER).unwrap();
        capture.stdout(&vec![0xff; OUTPUT_LIMIT]).unwrap();
        let (stdout, stderr) = capture.output();
        assert!(stdout.len() + stderr.len() <= OUTPUT_LIMIT);
        assert!(!stdout.is_empty());
    }

    #[tokio::test]
    async fn cancelled_or_closed_control_channels_prevent_starting() {
        let (_sender, receiver) = watch::channel(true);
        let result = execute(
            Path::new("relative"),
            Path::new("missing"),
            "{}",
            Some(receiver),
        )
        .await;
        assert_eq!(result["error"]["code"], "cancelled");
        let (sender, receiver) = watch::channel(false);
        drop(sender);
        let result = execute(
            Path::new("relative"),
            Path::new("missing"),
            "{}",
            Some(receiver),
        )
        .await;
        assert_eq!(result["error"]["code"], "cancelled");
    }

    #[tokio::test]
    async fn cancellation_signal_wakes_the_waiter_without_spawning() {
        let (sender, receiver) = watch::channel(false);
        let mut receiver = Some(receiver);
        let waited = async {
            wait_for_cancellation(&mut receiver).await;
        };
        let signalled = async {
            sender.send(true).unwrap();
        };
        tokio::join!(waited, signalled);
        assert!(cancellation_requested(receiver.as_ref()));
    }

    #[tokio::test]
    async fn occupied_slot_returns_busy_before_path_inspection() {
        let _permit = WORKSPACE_SLOT.acquire().await.unwrap();
        let result = execute(
            Path::new("relative"),
            Path::new("missing"),
            r#"{"command":["true"]}"#,
            None,
        )
        .await;
        assert_eq!(result["error"]["code"], "busy");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn workspace_rejects_shared_roots_and_relative_paths() {
        for path in ["/", "/tmp", "relative", "/usr/bin/../lib"] {
            assert!(validate_workspace(Path::new(path)).is_err());
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "manual real-OS bwrap test; never part of routine checks"]
    async fn real_os_workspace_python_smoke() {
        use std::os::unix::fs::PermissionsExt;
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let directory = tempfile::Builder::new()
            .prefix(".clawmingle-exec-test-")
            .tempdir_in(home)
            .unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let result = execute(directory.path(), Path::new("/usr/bin/bwrap"), r#"{"command":["python3","-c","from pathlib import Path; Path('result.txt').write_text('fixture'); print('workspace-ok')"]}"#, None).await;
        assert_eq!(result["ok"], true);
        assert_eq!(result["stdout"], "workspace-ok\n");
        assert_eq!(
            fs::read_to_string(directory.path().join("result.txt")).unwrap(),
            "fixture"
        );
        if result["runtime_tools"]["node"] == true {
            let node = execute(
                directory.path(),
                Path::new("/usr/bin/bwrap"),
                r#"{"command":["node","-e","console.log(6*7)"]}"#,
                None,
            )
            .await;
            assert_eq!(node["ok"], true, "node_runtime_failed");
            assert_eq!(node["stdout"], "42\n");
        }
        let git = execute(
            directory.path(),
            Path::new("/usr/bin/bwrap"),
            r#"{"command":["git","--version"]}"#,
            None,
        )
        .await;
        assert_eq!(git["ok"], true, "git_runtime_failed");
        assert!(git["stdout"].as_str().unwrap().starts_with("git version "));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "manual OS isolation/cancellation verification, no model or credentials"]
    async fn real_os_scope_and_cancellation_boundaries() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let temporary = tempfile::Builder::new()
            .prefix(".clawmingle-boundary-test-")
            .tempdir_in(home)
            .unwrap();
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let workspace = temporary.path().join("scope");
        fs::create_dir(&workspace).unwrap();
        fs::set_permissions(&workspace, fs::Permissions::from_mode(0o700)).unwrap();
        let outside = temporary.path().join("synthetic-other-scope");
        fs::write(&outside, "private-fixture").unwrap();
        symlink(&outside, workspace.join("outside-link")).unwrap();
        let arguments=json!({"command":["python3","-c","from pathlib import Path; import os,sys; assert not Path(sys.argv[1]).exists(); assert not Path('/workspace/outside-link').exists(); assert not Path('/state').exists(); assert not Path('/home/codex').exists(); assert not any(x in os.environ for x in ('OPENAI_API_KEY','TYPESAFE_API_KEY','QQ_APP_SECRET')); Path('inside.txt').write_text('allowed'); print('isolation-ok')",outside.to_str().unwrap()]}).to_string();
        let result = execute(&workspace, Path::new("/usr/bin/bwrap"), &arguments, None).await;
        assert_eq!(result["ok"], true, "isolated_execution_failed");
        assert_eq!(result["stdout"], "isolation-ok\n");
        assert_eq!(fs::read_to_string(&outside).unwrap(), "private-fixture");
        let (cancel, rx) = watch::channel(false);
        let scope = workspace.clone();
        let arguments=json!({"command":["python3","-c","from pathlib import Path; import time; Path('started').write_text('yes'); time.sleep(4); Path('must-not-exist').write_text('late')"]}).to_string();
        let task = tokio::spawn(async move {
            execute(&scope, Path::new("/usr/bin/bwrap"), &arguments, Some(rx)).await
        });
        for _ in 0..100 {
            if workspace.join("started").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            workspace.join("started").exists(),
            "execution_did_not_start"
        );
        cancel.send_replace(true);
        let result = task.await.unwrap();
        assert_eq!(result["error"]["code"], "cancelled");
        assert!(!workspace.join("must-not-exist").exists());
        let result = execute(
            &workspace,
            Path::new("/usr/bin/bwrap"),
            r#"{"command":["python3","-c","print('after-cancel')"]}"#,
            None,
        )
        .await;
        assert_eq!(result["ok"], true);
        assert_eq!(result["stdout"], "after-cancel\n");
    }
    #[test]
    fn skill_mounts_are_read_only_turn_selected_and_never_native_home() {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let root = tempfile::tempdir().unwrap();
        let skill = root.path().join("example");
        fs::DirBuilder::new().mode(0o700).create(&skill).unwrap();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(skill.join("SKILL.md"))
            .unwrap();
        file.write_all(b"Instruction only").unwrap();
        let args =
            parse_arguments(r#"{"command":["python3","/skills/example/scripts/run.py"]}"#).unwrap();
        let (command, _) = build_command_with_skills(
            root.path(),
            Path::new("/usr/bin/bwrap"),
            &args,
            std::slice::from_ref(&skill),
        )
        .unwrap();
        let argv = command
            .as_std()
            .get_args()
            .map(|v| v.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            argv.windows(3)
                .any(|w| w == ["--ro-bind", skill.to_str().unwrap(), "/skills/example"])
        );
        assert!(
            argv.windows(3)
                .any(|w| w == ["--bind", root.path().to_str().unwrap(), "/workspace"])
        );
        assert!(!argv.iter().any(|v| v.contains(".codex")));
        assert!(argv.iter().any(|v| v == "--unshare-all"));
        let (disabled, _) =
            build_command_with_skills(root.path(), Path::new("/usr/bin/bwrap"), &args, &[])
                .unwrap();
        assert!(
            !disabled
                .as_std()
                .get_args()
                .any(|arg| arg == "/skills/example")
        );
        assert!(
            build_command_with_skills(
                root.path(),
                Path::new("/usr/bin/bwrap"),
                &args,
                &[skill.clone(), skill]
            )
            .is_err()
        );
    }
}
