//! Ignored, anonymous integration test of the actual isolated app-server path.
//! A loopback Responses fixture chooses deterministic tool calls. No external
//! model, QQ, website, account, or existing Codex profile is contacted.
#![cfg(target_os = "linux")]

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use codex_claw::{
    codex::{
        AppServerHandle, ClientInfo, ExecutionRequest, ExecutionUpdate, IsolatedAppServerConfig,
        SOCIAL_CODEX_HOME, SOCIAL_WORKSPACE,
        app_server::{
            SocialTurnInput,
            scoped_integrations::{
                self, BrowserConfig, BrowserRuntime, IntegrationConfig, IntegrationGrant,
            },
        },
    },
    session::state::{ReasoningEffort, SessionState},
};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

// Select bundled metadata with native freeform patch support, ordinary tool
// mode and Responses SSE. Unknown slugs intentionally have no patch metadata.
// This name is never sent to an external model provider in these tests.
const FIXTURE_MODEL: &str = "gpt-5.5";

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAIAAAD8GO2jAAAAJklEQVR4nO3NsQkAAAjAsP7/tF7hIASyp6ZbAoFAIBAIBAKB4EuwNof8LkGrxSIAAAAASUVORK5CYII=";

#[derive(Default)]
struct Fixture {
    step: usize,
    schemas: BTreeMap<String, Value>,
    errors: Vec<String>,
    image_inputs: usize,
    saw_exec_output: bool,
    saw_patch_output: bool,
    saw_resume_read: bool,
    saw_search_output: bool,
    saw_skill_catalog: bool,
    saw_skill_read: bool,
}

// Both processes ignore SIGTERM so successful cancellation must also clean up
// descendants in the executor namespace; cancelling only the provider stream
// or the foreground shell is insufficient. All writes stay in this fixture's
// fresh conversation workspace.
const LONG_NATIVE_PROCESS: &str = r#"python3 -u - <<'PY'
import pathlib, signal, subprocess, sys, time
child = '''import pathlib, signal, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
with pathlib.Path('/workspace/cancel-child.heartbeat').open('ab', buffering=0) as output:
 while True:
  output.write(b'c')
  time.sleep(0.04)
'''
subprocess.Popen([sys.executable, '-u', '-c', child])
signal.signal(signal.SIGTERM, signal.SIG_IGN)
with pathlib.Path('/workspace/cancel-parent.heartbeat').open('ab', buffering=0) as output:
 while True:
  output.write(b'p')
  time.sleep(0.04)
PY"#;

#[derive(Default)]
struct CancellationFixture {
    schemas: BTreeMap<String, Value>,
    errors: Vec<String>,
    step: usize,
    issued: bool,
    recovery: Option<&'static str>,
    confirmed: Vec<String>,
    last_tool_output: String,
}

impl CancellationFixture {
    fn recover(&mut self, marker: &'static str) {
        self.issued = false;
        self.recovery = Some(marker);
    }

    fn respond(&mut self, request: &Request) -> ResponseTemplate {
        let body: Value = match serde_json::from_slice(&request.body) {
            Ok(body) => body,
            Err(_) => {
                self.errors
                    .push("provider request is not plain JSON".into());
                return ResponseTemplate::new(400);
            }
        };
        let mut current = BTreeMap::new();
        collect_schemas(&body["tools"], &mut current);
        if !current.keys().any(|name| name.ends_with("exec_command")) {
            self.errors
                .push("native exec schema missing without browser grant".into());
        }
        if current
            .keys()
            .any(|name| name.contains("browser_") || name.ends_with("workspace_exec"))
        {
            self.errors
                .push("ungranted browser or legacy exec wrapper exposed".into());
        }
        self.schemas.extend(current);
        // Only fixture-controlled tool output, bounded and escaped when shown;
        // never capture provider headers, environment, prompts, or profiles.
        let outputs = tool_outputs(&body["input"]);
        if !outputs.is_empty() {
            self.last_tool_output = outputs.chars().take(4096).collect();
        }
        self.step += 1;
        let item = if !self.issued {
            self.issued = true;
            let command = match self.recovery {
                None => LONG_NATIVE_PROCESS.to_owned(),
                Some(marker) => format!(
                    "python3 -c \"import pathlib; p=pathlib.Path('/workspace'); assert (p/'cancel-parent.heartbeat').is_file(); assert (p/'cancel-child.heartbeat').is_file(); (p/'{marker}.txt').write_text('{marker}'); print('{marker}')\""
                ),
            };
            let name = self
                .schemas
                .keys()
                .find(|name| name.ends_with("exec_command"))
                .cloned()
                .unwrap_or_else(|| "exec_command".into());
            json!({"type":"function_call","id":format!("cancel-item-{}",self.step),"call_id":format!("cancel-call-{}",self.step),"name":name,"arguments":json!({"cmd":command,"workdir":"/workspace","yield_time_ms":10000,"max_output_tokens":1000}).to_string(),"status":"completed"})
        } else {
            if let Some(marker) = self.recovery {
                let outputs = body["input"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|item| item["type"] == "function_call_output")
                    .map(|item| item["output"].to_string())
                    .collect::<String>();
                if outputs.contains(marker) {
                    self.confirmed.push(marker.into());
                } else {
                    self.errors
                        .push(format!("native recovery output missing: {marker}"));
                }
            } else {
                self.errors
                    .push("long command returned before fixture cancellation".into());
            }
            final_item()
        };
        let id = format!("cancel-response-{}", self.step);
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

fn tool_outputs(input: &Value) -> String {
    input
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| {
            matches!(
                item["type"].as_str(),
                Some("function_call_output" | "custom_tool_call_output")
            )
        })
        .map(|item| item["output"].to_string())
        .collect()
}

fn heartbeat_lengths(workspace: &Path) -> Option<[u64; 2]> {
    Some([
        fs::metadata(workspace.join("cancel-parent.heartbeat"))
            .ok()?
            .len(),
        fs::metadata(workspace.join("cancel-child.heartbeat"))
            .ok()?
            .len(),
    ])
}

#[tokio::test]
#[ignore = "requires native Codex and working unprivileged bubblewrap; anonymous loopback provider, no browser runtime or model"]
async fn cancelling_native_process_reaps_descendants_and_releases_workspace_for_resume() {
    use anyhow::{Context, ensure};
    use tokio::{
        sync::{mpsc, oneshot},
        time::{Instant, sleep, timeout},
    };

    let server = MockServer::start().await;
    let fixture = Arc::new(Mutex::new(CancellationFixture::default()));
    let responder = fixture.clone();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| responder.lock().unwrap().respond(request))
        .mount(&server)
        .await;
    let root = tempfile::Builder::new()
        .prefix(".native-cancel-smoke-")
        .tempdir_in(PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local"))
        .unwrap();
    private_directory(root.path());
    let runtime = root.path().join("runtime");
    for name in ["", "codex", "sqlite", "workspace"] {
        private_directory(&runtime.join(name));
    }
    let workspace_root = root.path().join("workspaces");
    private_directory(&workspace_root);
    let workspace = workspace_root.join("g_1111111111111111111111111111111111111111");
    private_directory(&workspace);
    let configuration = runtime.join("codex/config.toml");
    fs::write(&configuration, format!("model='{FIXTURE_MODEL}'\nmodel_provider='fixture'\nweb_search='disabled'\n[model_providers.fixture]\nname='Offline fixture'\nbase_url='{}/v1'\nwire_api='responses'\nrequires_openai_auth=false\n", server.uri())).unwrap();
    fs::set_permissions(&configuration, fs::Permissions::from_mode(0o600)).unwrap();
    let handle = AppServerHandle::start_isolated(
        IsolatedAppServerConfig {
            bubblewrap_binary: std::env::var_os("CLAWMINGLE_NATIVE_BWRAP_BINARY")
                .map(PathBuf::from)
                .unwrap_or_else(|| "/usr/bin/bwrap".into()),
            codex_binary: std::env::var_os("CLAWMINGLE_NATIVE_CODEX_BINARY")
                .map(PathBuf::from)
                .unwrap_or_else(|| "/opt/codex/current/bin/codex".into()),
            runtime_root: runtime.clone(),
            provider_environment: BTreeMap::new(),
            selected_provider: None,
            image_worker: false,
            codex_app_worker: false,
        },
        ClientInfo {
            name: "clawmingle-offline-native-cancel".into(),
            version: "1".into(),
            title: None,
        },
    )
    .await
    .expect("anonymous isolated app-server");
    // Execution authorization alone must be sufficient; no browser runtime or
    // grant is supplied during cancellation or either recovery turn.
    let input = || SocialTurnInput {
        tools: vec!["workspace_exec".into()],
        workspace: Some(workspace.clone()),
        ..Default::default()
    };
    let schema = json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false});
    let mut request = ExecutionRequest {
        prompt: "Offline native cancellation fixture.".into(),
        workspace_dir: SOCIAL_WORKSPACE.into(),
        codex_home: SOCIAL_CODEX_HOME.into(),
        config_overrides: vec![],
        add_dirs: vec![],
        session_state: SessionState::default(),
        model: Some(FIXTURE_MODEL.into()),
        service_tier: None,
        context_mode: None,
        reasoning_effort: ReasoningEffort::Low,
        image_paths: vec![],
    };

    // Always shut down the private app-server after a failed scenario as well.
    let scenario: anyhow::Result<()> = async {
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let (update_tx, mut updates) = mpsc::unbounded_channel();
        let cancelled_thread = {
            let execution = handle.execute_social_with_input(request.clone(), schema.clone(), input(), Some(cancel_rx), Some(update_tx));
            tokio::pin!(execution);
            let mut thread = None;
            timeout(Duration::from_secs(30), async {
                loop {
                    tokio::select! {
                        result = &mut execution => {
                            let observed = fixture.lock().unwrap();
                            anyhow::bail!("turn ended before parent and descendant ran: {result:?}; native output={:?}; fixture errors={:?}", observed.last_tool_output, observed.errors);
                        },
                        update = updates.recv() => {
                            if let Some(ExecutionUpdate::SessionStarted { session_id }) = update { thread = Some(session_id); }
                        },
                        _ = sleep(Duration::from_millis(30)) => {
                            if thread.is_some() && heartbeat_lengths(&workspace).is_some_and(|lengths| lengths.iter().all(|length| *length >= 3)) {
                                break;
                            }
                        },
                    }
                }
                Ok::<(), anyhow::Error>(())
            }).await.context("native parent/descendant did not start")??;
            let started = Instant::now();
            cancel_tx.send(()).map_err(|_| anyhow::anyhow!("cancel receiver disappeared"))?;
            let result = timeout(Duration::from_secs(12), &mut execution).await
                .context("native cancellation failed to settle and release its lease")?;
            let error = result.err().context("cancelled native turn unexpectedly succeeded")?;
            ensure!(error.to_string().contains("aborted by user"), "native cancellation failed: {error:#}");
            ensure!(started.elapsed() < Duration::from_secs(12), "native cancellation exceeded cleanup deadline");
            thread.context("native thread id was not announced")?
        };
        let stopped = heartbeat_lengths(&workspace).context("native heartbeat files missing after cancellation")?;
        fixture.lock().unwrap().recover("SAME_THREAD_RECOVERED");
        request.session_state = serde_json::from_value(json!({"session_id":cancelled_thread}))?;
        // No sleep/retry before this call: a successful turn proves the previous
        // workspace lease was already released when cancellation returned.
        let resumed = timeout(Duration::from_secs(30), handle.execute_social_with_input(request.clone(), schema.clone(), input(), None, None))
            .await.context("same-thread recovery timed out")??;
        ensure!(resumed.session_id.as_deref() == Some(cancelled_thread.as_str()), "cancelled thread was not resumed");
        ensure!(serde_json::from_str::<Value>(&resumed.text)? == json!({"ok":true}), "same-thread recovery did not finish");
        sleep(Duration::from_millis(500)).await;
        ensure!(heartbeat_lengths(&workspace) == Some(stopped), "cancelled parent or descendant is still writing");

        fixture.lock().unwrap().recover("NEW_THREAD_RECOVERED");
        request.session_state = SessionState::default();
        let fresh = timeout(Duration::from_secs(30), handle.execute_social_with_input(request, schema, input(), None, None))
            .await.context("new-thread recovery timed out")??;
        ensure!(fresh.session_id.is_some() && fresh.session_id != resumed.session_id, "fresh task did not use a new thread");
        ensure!(serde_json::from_str::<Value>(&fresh.text)? == json!({"ok":true}), "new-thread recovery did not finish");
        sleep(Duration::from_millis(300)).await;
        ensure!(heartbeat_lengths(&workspace) == Some(stopped), "cancelled writer survived later executor lifecycles");
        for marker in ["SAME_THREAD_RECOVERED", "NEW_THREAD_RECOVERED"] {
            ensure!(fs::read_to_string(workspace.join(format!("{marker}.txt")))? == marker, "recovery wrote outside the selected workspace");
            ensure!(!runtime.join("workspace").join(format!("{marker}.txt")).exists(), "recovery used authority workspace");
        }
        ensure!(!runtime.join("codex/auth.json").exists(), "fixture imported authentication");
        let observed = fixture.lock().unwrap();
        ensure!(observed.errors.is_empty(), "fixture errors: {:?}", observed.errors);
        ensure!(observed.confirmed == ["SAME_THREAD_RECOVERED", "NEW_THREAD_RECOVERED"], "native recovery outputs did not reach provider");
        Ok(())
    }.await;
    handle.shutdown().await;
    scenario.expect("native cancellation and workspace recovery");
}

impl Fixture {
    fn respond(&mut self, request: &Request) -> ResponseTemplate {
        let body: Value = match serde_json::from_slice(&request.body) {
            Ok(body) => body,
            Err(_) => {
                self.errors
                    .push("provider request is not plain JSON".into());
                return ResponseTemplate::new(400);
            }
        };
        collect_schemas(&body["tools"], &mut self.schemas);
        for item in body["input"].as_array().into_iter().flatten() {
            if item["type"] == "tool_search_output" {
                self.saw_search_output |= item["status"] == "completed"
                    && item["tools"]
                        .as_array()
                        .is_some_and(|tools| !tools.is_empty());
                collect_schemas(&item["tools"], &mut self.schemas);
            }
        }
        if self.step == 0 {
            // Inspect the first request before a tool can echo any Skill text.
            // The fixture prompt itself contains no catalog/instruction text.
            let instructions = format!("{}{}", body["instructions"], body["input"]);
            self.saw_skill_catalog = instructions.contains("native-fixture")
                && instructions.contains("NATIVE_SELECTED_SKILL_DISCOVERY");
            if instructions.contains("NATIVE_UNSELECTED_SKILL_DISCOVERY") {
                self.errors
                    .push("unselected Skill appeared in native instructions".into());
            }
        }
        let outputs = tool_outputs(&body["input"]);
        self.saw_skill_read |= outputs.contains("NATIVE_SELECTED_SKILL_READ_OK");
        self.saw_patch_output |=
            outputs.contains("native-patch.txt") && outputs.contains("Success");
        self.saw_exec_output |= outputs.contains("NATIVE_REMOTE_EXEC_OK");
        self.saw_resume_read |= outputs.contains("RESUMED_REMOTE_READ_OK");
        self.image_inputs = self.image_inputs.max(count_images(&body["input"]));
        let step = self.step;
        self.step += 1;
        let item = match step {
            0 => {
                for suffix in ["exec_command", "apply_patch", "view_image", "tool_search"] {
                    if !self.schemas.keys().any(|name| name.ends_with(suffix)) {
                        self.errors.push(format!("missing native schema: {suffix}; names={:?}", self.schemas.keys().collect::<Vec<_>>()));
                    }
                }
                let command = format!("python3 -c \"import base64,pathlib; p=pathlib.Path('/workspace'); (p/'native-marker.txt').write_text('NATIVE_REMOTE_EXEC_OK'); (p/'native-image.png').write_bytes(base64.b64decode('{PNG}')); assert 'NATIVE_SELECTED_SKILL_CONTENT' in pathlib.Path('/skills/native-fixture/SKILL.md').read_text(); assert not pathlib.Path('/skills/unselected-fixture/SKILL.md').exists(); print('NATIVE_SELECTED_SKILL_READ_OK'); print((p/'native-marker.txt').read_text())\"");
                self.call("exec_command", json!({"cmd":command,"workdir":"/workspace","max_output_tokens":1000}))
            }
            1 => {
                let name = self.schemas.keys().find(|name| name.ends_with("apply_patch")).cloned().unwrap_or_else(|| "apply_patch".into());
                // Exercise the native freeform tool protocol as well as the
                // schema: this file must be present after executor rebinding.
                json!({"type":"custom_tool_call","id":format!("patch-item-{}",self.step),"call_id":format!("patch-call-{}",self.step),"name":name,"input":"*** Begin Patch\n*** Add File: /workspace/native-patch.txt\n+NATIVE_PATCH_OK\n*** End Patch","status":"completed"})
            },
            2 => self.call("view_image", json!({"path":"/workspace/native-image.png"})),
            3 => json!({"type":"tool_search_call","id":"fixture-discovery-item","call_id":"fixture-discovery-call","status":"completed","execution":"client","arguments":{"query":"playwright browser_navigate browser_evaluate browser_take_screenshot","limit":32}}),
            4 => {
                for suffix in ["browser_navigate", "browser_evaluate", "browser_take_screenshot"] {
                    if !self.schemas.keys().any(|name| name.ends_with(suffix)) {
                        self.errors.push(format!("native discovery omitted {suffix}; names={:?}", self.schemas.keys().collect::<Vec<_>>()));
                    }
                }
                self.call("browser_navigate", json!({"url":"about:blank"}))
            },
            5 => self.call("browser_evaluate", json!({"function":"() => { document.body.innerHTML='<h1>NATIVE_MCP_SCREENSHOT</h1>'; return document.body.textContent; }"})),
            6 => self.call("browser_take_screenshot", json!({"type":"png"})),
            7 => final_item(),
            8 => self.call("exec_command", json!({"cmd":"python3 -c \"import pathlib; p=pathlib.Path('/workspace'); assert (p/'native-image.png').is_file(); assert (p/'native-marker.txt').read_text() == 'NATIVE_REMOTE_EXEC_OK'; assert (p/'native-patch.txt').read_text().strip() == 'NATIVE_PATCH_OK'; print('RESUMED_REMOTE_READ_OK')\"","workdir":"/workspace","max_output_tokens":1000})),
            _ => final_item(),
        };
        let id = format!("fixture-response-{step}");
        let events = [
            json!({"type":"response.created","response":{"id":id,"status":"in_progress","output":[]}}),
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
            json!({"type":"response.completed","response":{"id":id,"status":"completed","output":[item],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
        ];
        let stream = events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect::<String>();
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(stream)
    }

    fn call(&mut self, suffix: &str, arguments: Value) -> Value {
        let name = self
            .schemas
            .keys()
            .find(|name| name.ends_with(suffix))
            .cloned()
            .unwrap_or_else(|| suffix.into());
        let mut item = json!({"type":"function_call","id":format!("item-{}",self.step),"call_id":format!("call-{}",self.step),"name":name,"arguments":arguments.to_string(),"status":"completed"});
        if let Some(namespace) = self
            .schemas
            .get(&name)
            .and_then(|tool| tool["_fixture_namespace"].as_str())
        {
            item["namespace"] = json!(namespace);
        }
        item
    }
}

fn collect_schemas(value: &Value, schemas: &mut BTreeMap<String, Value>) {
    collect_namespaced_schemas(value, None, schemas);
}

fn collect_namespaced_schemas(
    value: &Value,
    namespace: Option<&str>,
    schemas: &mut BTreeMap<String, Value>,
) {
    if let Some(tools) = value.as_array() {
        for tool in tools {
            if tool["type"] == "namespace" {
                collect_namespaced_schemas(&tool["tools"], tool["name"].as_str(), schemas);
                continue;
            }
            // Native tool_search is identified by its wire type, without name.
            if let Some(name) = tool["name"]
                .as_str()
                .or_else(|| (tool["type"] == "tool_search").then_some("tool_search"))
            {
                let mut schema = tool.clone();
                if let Some(namespace) = namespace {
                    schema["_fixture_namespace"] = json!(namespace);
                }
                schemas.insert(name.into(), schema);
            }
        }
    }
}

fn count_images(value: &Value) -> usize {
    match value {
        Value::Object(object) => {
            usize::from(object.get("type").and_then(Value::as_str) == Some("input_image"))
                + object.values().map(count_images).sum::<usize>()
        }
        Value::Array(items) => items.iter().map(count_images).sum(),
        _ => 0,
    }
}

fn final_item() -> Value {
    json!({"type":"message","id":"fixture-final","role":"assistant","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":"{\"ok\":true}"}]})
}

fn private_directory(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[tokio::test]
#[ignore = "requires existing native Codex/browser runtimes and working unprivileged bubblewrap; local fixture only"]
async fn native_tools_and_browser_images_survive_isolated_turn_and_resume() {
    let browser: BrowserRuntime = serde_json::from_slice(
        &fs::read(
            std::env::var_os("CLAWMINGLE_BROWSER_SMOKE_RUNTIME_JSON")
                .expect("existing browser runtime JSON required"),
        )
        .unwrap(),
    )
    .unwrap();
    let server = MockServer::start().await;
    let fixture = Arc::new(Mutex::new(Fixture::default()));
    let responder = fixture.clone();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| responder.lock().unwrap().respond(request))
        .mount(&server)
        .await;
    let root = tempfile::Builder::new()
        .prefix(".native-smoke-")
        .tempdir_in(PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local"))
        .unwrap();
    private_directory(root.path());
    let runtime = root.path().join("runtime");
    for name in ["", "codex", "sqlite", "workspace"] {
        private_directory(&runtime.join(name));
    }
    let workspace_root = root.path().join("workspaces");
    private_directory(&workspace_root);
    let workspace = workspace_root.join("g_0000000000000000000000000000000000000000");
    private_directory(&workspace);
    let skills = root.path().join("skills");
    private_directory(&skills);
    let selected_skill = skills.join("native-fixture");
    for (name, description, contents) in [
        (
            "native-fixture",
            "NATIVE_SELECTED_SKILL_DISCOVERY",
            "NATIVE_SELECTED_SKILL_CONTENT",
        ),
        (
            "unselected-fixture",
            "NATIVE_UNSELECTED_SKILL_DISCOVERY",
            "NATIVE_UNSELECTED_SKILL_CONTENT",
        ),
    ] {
        let directory = skills.join(name);
        private_directory(&directory);
        let path = directory.join("SKILL.md");
        fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {description}\n---\n{contents}\n"),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let configuration = runtime.join("codex/config.toml");
    fs::write(&configuration, format!("model='{FIXTURE_MODEL}'\nmodel_provider='fixture'\nweb_search='disabled'\n[model_providers.fixture]\nname='Offline fixture'\nbase_url='{}/v1'\nwire_api='responses'\nrequires_openai_auth=false\n",server.uri())).unwrap();
    fs::set_permissions(&configuration, fs::Permissions::from_mode(0o600)).unwrap();
    let handle = AppServerHandle::start_isolated(
        IsolatedAppServerConfig {
            bubblewrap_binary: browser.bubblewrap.clone(),
            codex_binary: std::env::var_os("CLAWMINGLE_NATIVE_CODEX_BINARY")
                .map(PathBuf::from)
                .unwrap_or_else(|| "/opt/codex/current/bin/codex".into()),
            runtime_root: runtime.clone(),
            provider_environment: BTreeMap::new(),
            selected_provider: None,
            image_worker: false,
            codex_app_worker: false,
        },
        ClientInfo {
            name: "clawmingle-offline-native-test".into(),
            version: "1".into(),
            title: None,
        },
    )
    .await
    .expect("anonymous isolated app-server");
    let integration = IntegrationConfig {
        browser: BrowserConfig {
            enabled: true,
            runtime: Some(browser),
        },
        ..Default::default()
    };
    let input = || SocialTurnInput {
        tools: vec!["workspace_exec".into(), "public_browser".into()],
        workspace: Some(workspace.clone()),
        skill_packages: vec![selected_skill.clone()],
        integrations: Some(
            scoped_integrations::bind(
                &integration,
                &IntegrationGrant {
                    browser: true,
                    plugins: vec![],
                },
            )
            .unwrap(),
        ),
        ..Default::default()
    };
    let schema = json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false});
    let mut request = ExecutionRequest {
        prompt: "Offline deterministic tool protocol fixture.".into(),
        workspace_dir: SOCIAL_WORKSPACE.into(),
        codex_home: SOCIAL_CODEX_HOME.into(),
        config_overrides: vec![],
        add_dirs: vec![],
        session_state: SessionState::default(),
        model: Some(FIXTURE_MODEL.into()),
        service_tier: None,
        context_mode: None,
        reasoning_effort: ReasoningEffort::Low,
        image_paths: vec![],
    };
    let first = tokio::time::timeout(
        Duration::from_secs(90),
        handle.execute_social_with_input(request.clone(), schema.clone(), input(), None, None),
    )
    .await;
    let resumed = if let Ok(Ok(first)) = &first {
        request.session_state =
            serde_json::from_value(json!({"session_id":first.session_id})).unwrap();
        Some(
            tokio::time::timeout(
                Duration::from_secs(60),
                handle.execute_social_with_input(request, schema, input(), None, None),
            )
            .await,
        )
    } else {
        None
    };
    handle.shutdown().await;
    let first = first
        .expect("first fixture turn timeout")
        .expect("first fixture turn failed");
    let resumed = resumed
        .expect("resume requested")
        .expect("resume timeout")
        .expect("resume failed");
    assert_eq!(
        first.session_id, resumed.session_id,
        "thread remains the same while its executor is rebound"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&first.text).unwrap(),
        json!({"ok":true})
    );
    assert_eq!(
        serde_json::from_str::<Value>(&resumed.text).unwrap(),
        json!({"ok":true})
    );
    assert_eq!(
        fs::read_to_string(workspace.join("native-marker.txt")).unwrap(),
        "NATIVE_REMOTE_EXEC_OK"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("native-patch.txt")).unwrap(),
        "NATIVE_PATCH_OK\n"
    );
    assert!(
        !runtime.join("workspace/native-patch.txt").exists(),
        "native patch must not write to the authority workspace"
    );
    assert!(
        !runtime.join("workspace/native-marker.txt").exists(),
        "authority workspace must remain untouched"
    );
    assert!(
        !runtime.join("codex/auth.json").exists(),
        "no authentication was imported"
    );
    let fixture = fixture.lock().unwrap();
    assert!(fixture.errors.is_empty(), "{:?}", fixture.errors);
    assert!(
        fixture.saw_search_output,
        "native deferred MCP search returned schemas"
    );
    assert!(
        fixture.saw_skill_catalog && fixture.saw_skill_read,
        "selected Skill was discovered by Codex and read through its executor"
    );
    assert!(
        fixture.saw_exec_output && fixture.saw_patch_output && fixture.saw_resume_read,
        "remote command and freeform patch output reached provider"
    );
    assert!(
        fixture.image_inputs >= 2,
        "native view_image and MCP PNG reached provider as images; got {}",
        fixture.image_inputs
    );
    assert_eq!(
        fixture.step, 10,
        "eight first-turn exchanges including native discovery and two resume exchanges"
    );
    println!(
        "Anonymous native app-server verified: {} schemas, {} image inputs, same-thread executor rebind",
        fixture.schemas.len(),
        fixture.image_inputs
    );
}
