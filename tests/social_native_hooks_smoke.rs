//! Actual native Hooks engine + task executor, with an anonymous local provider.
//! Ignored by default. No account, external model, QQ, or personal profile is used.
#![cfg(target_os = "linux")]

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use codex_claw::{
    codex::{
        AppServerHandle, ClientInfo, ExecutionRequest, IsolatedAppServerConfig, SOCIAL_CODEX_HOME,
        SOCIAL_WORKSPACE, app_server::SocialTurnInput,
    },
    session::state::{ReasoningEffort, SessionState},
};
use serde_json::{Value, json};
use tokio::{sync::oneshot, time::timeout};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

const MODEL: &str = "gpt-5.5";
const START_CONTEXT: &str = "NATIVE_HOOK_START_CONTEXT";
const POST_CONTEXT: &str = "NATIVE_HOOK_POST_CONTEXT";
const AUTH_CANARY: &str = "CLAWMINGLE_HOOK_FIXTURE_KEY";

#[derive(Default)]
struct Fixture {
    requests: usize,
    saw_start_context: bool,
    saw_post_context: bool,
    errors: Vec<String>,
}

impl Fixture {
    fn respond(&mut self, request: &Request) -> ResponseTemplate {
        let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
            self.errors.push("fixture expected JSON".into());
            return ResponseTemplate::new(400);
        };
        let context = format!("{}{}", body["instructions"], body["input"]);
        self.saw_start_context |= context.contains(START_CONTEXT);
        self.saw_post_context |= context.contains(POST_CONTEXT);
        let step = self.requests;
        self.requests += 1;
        let item = if step.is_multiple_of(2) {
            let found = find_exec(&body["tools"], None);
            if found.is_none() {
                self.errors.push("native exec missing".into());
            }
            let (name, namespace) = found.unwrap_or_else(|| ("exec_command".into(), None));
            let mut item = json!({"type":"function_call","id":format!("hook-item-{step}"),"call_id":format!("hook-call-{step}"),"name":name,"arguments":json!({"cmd":"printf NATIVE_TOOL_OK > /workspace/native-tool.txt; printf NATIVE_TOOL_OK","workdir":"/workspace","max_output_tokens":500}).to_string(),"status":"completed"});
            if let Some(namespace) = namespace {
                item["namespace"] = json!(namespace);
            }
            item
        } else {
            json!({"type":"message","id":format!("hook-final-{step}"),"role":"assistant","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":"{\"ok\":true}"}]})
        };
        let id = format!("hook-response-{step}");
        let events = [
            json!({"type":"response.created","response":{"id":id,"status":"in_progress","output":[]}}),
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
            json!({"type":"response.completed","response":{"id":id,"status":"completed","output":[item],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
        ];
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(
                events
                    .iter()
                    .map(|event| {
                        format!(
                            "event: {}\ndata: {event}\n\n",
                            event["type"].as_str().unwrap()
                        )
                    })
                    .collect::<String>(),
            )
    }
}

fn find_exec(value: &Value, namespace: Option<&str>) -> Option<(String, Option<String>)> {
    for tool in value.as_array()? {
        if tool["type"] == "namespace" {
            if let Some(found) = find_exec(&tool["tools"], tool["name"].as_str()) {
                return Some(found);
            }
        } else if let Some(name) = tool["name"].as_str()
            && name.ends_with("exec_command")
        {
            return Some((name.into(), namespace.map(str::to_owned)));
        }
    }
    None
}

fn private_directory(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn private_file(path: &Path, contents: impl AsRef<[u8]>) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn python_command(script: &str) -> String {
    format!("python3 -c '{}'", script.replace('\'', "'\\''"))
}

fn recording_hook(event: &str, context: Option<&str>) -> Value {
    let context = context.map_or_else(
        || json!({}),
        |text| json!({"hookSpecificOutput":{"hookEventName":event,"additionalContext":text}}),
    );
    let script = format!(
        "import json,os,pathlib,sys; event=json.load(sys.stdin); assert event['hook_event_name']=={event:?}; assert isinstance(event['session_id'],str); assert os.getenv({AUTH_CANARY:?}) is None; assert not pathlib.Path('/state/codex').exists(); p=pathlib.Path('/workspace'); (p/'hook-{event}.json').write_text(json.dumps(event)); print({output:?})",
        output = context.to_string(),
    );
    json!({"type":"command","command":python_command(&script),"timeout":if matches!(event, "SessionEnd" | "Interrupt") {3} else {10}})
}

struct Instance {
    _root: tempfile::TempDir,
    _provider: MockServer,
    runtime: PathBuf,
    workspaces: PathBuf,
    workspace: PathBuf,
    handle: AppServerHandle,
    fixture: Arc<Mutex<Fixture>>,
}

impl Instance {
    async fn new(hooks: Value) -> Self {
        let provider = MockServer::start().await;
        let fixture = Arc::new(Mutex::new(Fixture::default()));
        let responder = fixture.clone();
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(move |request: &Request| responder.lock().unwrap().respond(request))
            .mount(&provider)
            .await;
        let root = tempfile::Builder::new()
            .prefix(".native-hooks-smoke-")
            .tempdir_in(PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local"))
            .unwrap();
        private_directory(root.path());
        let runtime = root.path().join("runtime");
        for name in ["", "codex", "sqlite", "workspace"] {
            private_directory(&runtime.join(name));
        }
        let workspaces = root.path().join("workspaces");
        private_directory(&workspaces);
        let workspace = workspaces.join("g_1111111111111111111111111111111111111111");
        private_directory(&workspace);
        private_file(
            &runtime.join("codex/config.toml"),
            format!(
                "model='{MODEL}'\nmodel_provider='fixture'\nweb_search='disabled'\n[model_providers.fixture]\nname='Offline hooks fixture'\nbase_url='{}/v1'\nwire_api='responses'\nrequires_openai_auth=false\nenv_key='{AUTH_CANARY}'\n",
                provider.uri()
            ),
        );
        private_file(
            &runtime.join("codex/hooks.json"),
            serde_json::to_vec(&hooks).unwrap(),
        );
        let handle = AppServerHandle::start_isolated(
            IsolatedAppServerConfig {
                bubblewrap_binary: std::env::var_os("CLAWMINGLE_NATIVE_BWRAP_BINARY")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| "/usr/bin/bwrap".into()),
                codex_binary: std::env::var_os("CLAWMINGLE_NATIVE_CODEX_BINARY")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| "/opt/codex/current/bin/codex".into()),
                runtime_root: runtime.clone(),
                provider_environment: BTreeMap::from([(
                    AUTH_CANARY.into(),
                    "FAKE_NOT_A_CREDENTIAL".into(),
                )]),
                selected_provider: None,
                image_worker: false,
                codex_app_worker: false,
            },
            ClientInfo {
                name: "clawmingle-offline-hooks".into(),
                version: "1".into(),
                title: None,
            },
        )
        .await
        .expect("isolated anonymous hooks fixture");
        Self {
            _root: root,
            _provider: provider,
            runtime,
            workspaces,
            workspace,
            handle,
            fixture,
        }
    }

    fn input(&self, workspace: &Path) -> SocialTurnInput {
        SocialTurnInput {
            tools: vec!["workspace_exec".into()],
            workspace: Some(workspace.to_owned()),
            ..Default::default()
        }
    }

    fn request(&self) -> ExecutionRequest {
        ExecutionRequest {
            prompt: "Offline hooks fixture.".into(),
            workspace_dir: SOCIAL_WORKSPACE.into(),
            codex_home: SOCIAL_CODEX_HOME.into(),
            config_overrides: vec![],
            add_dirs: vec![],
            session_state: SessionState::default(),
            model: Some(MODEL.into()),
            service_tier: None,
            context_mode: None,
            reasoning_effort: ReasoningEffort::Low,
            image_paths: vec![],
        }
    }
}

fn schema() -> Value {
    json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false})
}

#[tokio::test]
#[ignore = "requires Codex 0.161+, bwrap and both hook binaries; fake loopback provider only"]
async fn native_hooks_preserve_stdin_eof_context_and_scope_without_authority_credentials() {
    let instance = Instance::new(json!({"hooks":{
        "SessionStart":[{"hooks":[recording_hook("SessionStart",Some(START_CONTEXT))]}],
        "PostToolUse":[{"matcher":".*","hooks":[recording_hook("PostToolUse",Some(POST_CONTEXT))]}],
        "Stop":[{"hooks":[recording_hook("Stop",None)]}],
        "SessionEnd":[{"hooks":[recording_hook("SessionEnd",None)]}]
    }}))
    .await;
    let scenario: Result<()> = async {
        let other = instance
            .workspaces
            .join("g_2222222222222222222222222222222222222222");
        private_directory(&other);
        for workspace in [&instance.workspace, &other] {
            let result = timeout(
                Duration::from_secs(35),
                instance.handle.execute_social_with_input(
                    instance.request(),
                    schema(),
                    instance.input(workspace),
                    None,
                    None,
                ),
            )
            .await
            .context("native Hooks task timed out")??;
            ensure!(
                serde_json::from_str::<Value>(&result.text)? == json!({"ok":true}),
                "fixture completion missing"
            );
            for event in ["SessionStart", "PostToolUse", "Stop", "SessionEnd"] {
                let payload: Value = serde_json::from_slice(
                    &fs::read(workspace.join(format!("hook-{event}.json"))).with_context(|| {
                        format!("native {event} did not run in selected workspace")
                    })?,
                )?;
                ensure!(payload["hook_event_name"] == event, "native event mismatch");
            }
            ensure!(
                fs::read_to_string(workspace.join("native-tool.txt"))? == "NATIVE_TOOL_OK",
                "native tool failed after Hook"
            );
        }
        let first: Value = serde_json::from_slice(&fs::read(
            instance.workspace.join("hook-SessionStart.json"),
        )?)?;
        let second: Value =
            serde_json::from_slice(&fs::read(other.join("hook-SessionStart.json"))?)?;
        ensure!(
            first["session_id"] != second["session_id"],
            "different scopes shared a native thread"
        );
        ensure!(
            fs::read_dir(instance.runtime.join("workspace"))?
                .next()
                .is_none(),
            "hook ran in authority workspace"
        );
        ensure!(
            !instance.runtime.join("codex/auth.json").exists(),
            "fixture copied authentication"
        );
        let observed = instance.fixture.lock().unwrap();
        ensure!(
            observed.saw_start_context && observed.saw_post_context,
            "native Hook additionalContext did not reach model fixture"
        );
        ensure!(
            observed.errors.is_empty(),
            "fixture errors: {:?}",
            observed.errors
        );
        Ok(())
    }
    .await;
    instance.handle.shutdown().await;
    scenario.expect("native Hooks scope/EOF/context integration");
}

const LONG_HOOK: &str = r#"import json,pathlib,signal,subprocess,sys,time
event=json.load(sys.stdin)
assert event['hook_event_name']=='PreToolUse'
child="import pathlib,signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); f=pathlib.Path('/workspace/hook-child.heartbeat').open('ab',buffering=0)\nwhile True: f.write(b'c'); time.sleep(0.04)"
subprocess.Popen([sys.executable,'-u','-c',child])
signal.signal(signal.SIGTERM,signal.SIG_IGN)
with pathlib.Path('/workspace/hook-parent.heartbeat').open('ab',buffering=0) as output:
 while True:
  output.write(b'p')
  time.sleep(0.04)
"#;

fn heartbeat_lengths(workspace: &Path) -> Option<[u64; 2]> {
    Some([
        fs::metadata(workspace.join("hook-parent.heartbeat"))
            .ok()?
            .len(),
        fs::metadata(workspace.join("hook-child.heartbeat"))
            .ok()?
            .len(),
    ])
}

#[tokio::test]
#[ignore = "requires Codex 0.161+, bwrap and both hook binaries; fake loopback provider only"]
async fn cancelling_native_hook_stops_descendants_and_does_not_run_the_waiting_tool() {
    let instance = Instance::new(json!({"hooks":{
        "PreToolUse":[{"matcher":".*","hooks":[{"type":"command","command":python_command(LONG_HOOK),"timeout":60}]}],
        "Interrupt":[{"hooks":[recording_hook("Interrupt",None)]}]
    }})).await;
    let scenario: Result<()> = async {
        let (cancel, cancelled) = oneshot::channel();
        let execution = instance.handle.execute_social_with_input(instance.request(),schema(),instance.input(&instance.workspace),Some(cancelled),None);
        tokio::pin!(execution);
        timeout(Duration::from_secs(30), async {
            loop {
                tokio::select! {
                    result = &mut execution => anyhow::bail!("turn ended before Hook cancellation: {result:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(30)) => {
                        if heartbeat_lengths(&instance.workspace).is_some_and(|lengths| lengths.iter().all(|length| *length >= 3)) { return Ok::<_,anyhow::Error>(()); }
                    }
                }
            }
        }).await.context("native Hook did not receive EOF/start descendants")??;
        cancel.send(()).map_err(|_| anyhow::anyhow!("native cancellation receiver disappeared"))?;
        let result = timeout(Duration::from_secs(12), &mut execution).await.context("native Hook cancellation did not settle")?;
        let error = result.err().context("cancelled Hook task unexpectedly succeeded")?;
        ensure!(error.to_string().contains("aborted by user"), "cancellation error: {error:#}");
        let stopped = heartbeat_lengths(&instance.workspace).context("Hook heartbeat missing")?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        ensure!(heartbeat_lengths(&instance.workspace) == Some(stopped), "cancelled Hook descendant survived the task fence");
        ensure!(!instance.workspace.join("native-tool.txt").exists(), "tool ran after its PreToolUse Hook was cancelled");
        ensure!(instance.workspace.join("hook-Interrupt.json").is_file(), "native Interrupt Hook did not run");
        Ok(())
    }.await;
    instance.handle.shutdown().await;
    scenario.expect("native Hook cancellation and descendant fence");
}
