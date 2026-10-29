//! Two actual isolated native tasks sharing one authority and anonymous profile.
//! Dropping one execution must close its worker/executor without closing the other.
#![cfg(target_os = "linux")]

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, ensure};
use codex_claw::{
    codex::{
        AppServerHandle, ClientInfo, ExecutionRequest, ExecutionUpdate, IsolatedAppServerConfig,
        SOCIAL_CODEX_HOME, SOCIAL_WORKSPACE, app_server::SocialTurnInput,
    },
    session::state::{ReasoningEffort, SessionState},
};
use serde_json::{Value, json};
use tokio::{
    sync::mpsc,
    time::{Instant, sleep, timeout},
};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

const MODEL: &str = "gpt-5.5";
const DROP_PROMPT: &str = "OFFLINE_CONCURRENT_DROP_A";
const KEEP_PROMPT: &str = "OFFLINE_CONCURRENT_KEEP_B";

#[derive(Default)]
struct Fixture {
    sequence: usize,
    dropped_identity: Option<(String, String)>,
    survivor_id: Option<String>,
    turn_steps: BTreeMap<String, usize>,
    recovery_allowed: bool,
    recovered: bool,
    survivor_finished: bool,
    original_requests: usize,
    errors: Vec<String>,
    last_output: String,
}

impl Fixture {
    fn respond(&mut self, request: &Request) -> ResponseTemplate {
        let body: Value = match serde_json::from_slice(&request.body) {
            Ok(body) => body,
            Err(_) => {
                self.errors.push("fixture request is not plain JSON".into());
                return ResponseTemplate::new(400);
            }
        };
        // These are anonymous fixture-generated identities, never real profiles.
        let metadata = &body["client_metadata"];
        let (Some(thread_id), Some(turn_id)) =
            (metadata["thread_id"].as_str(), metadata["turn_id"].as_str())
        else {
            self.errors
                .push("native request lacks thread/turn identity".into());
            return ResponseTemplate::new(400);
        };
        if !metadata["x-codex-parent-thread-id"].is_null() {
            self.errors
                .push("unexpected child request in two-root fixture".into());
        }
        // Initial root prompts are distinct and contain no forked history.
        // All subsequent routing uses native thread/turn metadata.
        if self
            .dropped_identity
            .as_ref()
            .is_none_or(|(id, _)| id != thread_id)
            && self.survivor_id.as_deref() != Some(thread_id)
        {
            let input = body["input"].to_string();
            if input.contains(DROP_PROMPT) && self.dropped_identity.is_none() {
                self.dropped_identity = Some((thread_id.into(), turn_id.into()));
            } else if input.contains(KEEP_PROMPT) && self.survivor_id.is_none() {
                self.survivor_id = Some(thread_id.into());
            } else {
                self.errors
                    .push("unexpected root identity or prompt".into());
                return ResponseTemplate::new(400);
            }
        }
        let outputs = body["input"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == "function_call_output")
            .map(|item| item["output"].to_string())
            .collect::<String>();
        if !outputs.is_empty() {
            self.last_output = outputs.chars().take(2048).collect();
        }
        let mut schemas = BTreeMap::new();
        collect_schemas(&body["tools"], None, &mut schemas);
        let step = *self.turn_steps.entry(turn_id.into()).or_default();
        *self.turn_steps.get_mut(turn_id).unwrap() += 1;
        self.sequence += 1;
        let item = if self.survivor_id.as_deref() == Some(thread_id) {
            if outputs.contains("SURVIVOR_FINISHED") {
                self.survivor_finished = true;
                final_item()
            } else {
                self.exec(&schemas, SURVIVOR_CHUNK)
            }
        } else if self
            .dropped_identity
            .as_ref()
            .is_some_and(|(_, original_turn)| original_turn == turn_id)
        {
            self.original_requests += 1;
            if self.recovered {
                self.errors
                    .push("dropped worker made a request after its replacement completed".into());
            }
            self.exec(
                &schemas,
                if step == 0 {
                    DROPPED_PROCESS
                } else {
                    "sleep 1; echo DROPPED_TASK_STILL_ACTIVE"
                },
            )
        } else {
            if !self.recovery_allowed {
                self.errors
                    .push("dropped root resumed before test requested recovery".into());
            }
            if step == 0 {
                self.exec(&schemas, RECOVERY_COMMAND)
            } else {
                self.recovered = outputs.contains("DROPPED_SCOPE_REOPENED");
                if !self.recovered {
                    self.errors
                        .push("reopened scope returned no native executor marker".into());
                }
                final_item()
            }
        };
        stream(self.sequence, item)
    }

    fn exec(&mut self, schemas: &BTreeMap<String, Value>, command: &str) -> Value {
        let name = schemas
            .keys()
            .find(|name| name.ends_with("exec_command"))
            .cloned()
            .unwrap_or_else(|| {
                self.errors
                    .push("native exec_command schema missing".into());
                "exec_command".into()
            });
        let mut item = json!({"type":"function_call", "id":format!("concurrent-item-{}",self.sequence), "call_id":format!("concurrent-call-{}",self.sequence), "name":name, "arguments":json!({"cmd":command,"workdir":"/workspace","yield_time_ms":10000,"max_output_tokens":1000}).to_string(), "status":"completed"});
        if let Some(namespace) = schemas
            .get(&name)
            .and_then(|schema| schema["_namespace"].as_str())
        {
            item["namespace"] = json!(namespace);
        }
        item
    }
}

// A ignores SIGTERM; only ending the HTTP response or dropping a reader cannot
// stop this writer. Executor teardown must terminate its process namespace.
const DROPPED_PROCESS: &str = r#"python3 -u - <<'PY'
import pathlib, signal, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
with pathlib.Path('/workspace/dropped.heartbeat').open('ab', buffering=0) as output:
 while True:
  output.write(b'a')
  time.sleep(0.04)
PY"#;

// Short chunks return within native exec's yield window. The provider keeps
// this task running until the test explicitly releases it after A has reopened.
const SURVIVOR_CHUNK: &str = r#"python3 -u - <<'PY'
import pathlib, time
root = pathlib.Path('/workspace')
deadline = time.monotonic() + 2
with (root/'survivor.heartbeat').open('ab', buffering=0) as output:
 while time.monotonic() < deadline:
  output.write(b'b')
  if (root/'finish-survivor').exists():
   (root/'survivor-finished.txt').write_text('SURVIVOR_FINISHED')
   print('SURVIVOR_FINISHED')
   break
  time.sleep(0.04)
 else:
  print('SURVIVOR_STILL_ACTIVE')
PY"#;

const RECOVERY_COMMAND: &str = "python3 -c \"import pathlib; p=pathlib.Path('/workspace'); assert (p/'dropped.heartbeat').is_file(); assert not (p/'survivor.heartbeat').exists(); (p/'reopened.txt').write_text('DROPPED_SCOPE_REOPENED'); print('DROPPED_SCOPE_REOPENED')\"";

fn collect_schemas(value: &Value, namespace: Option<&str>, schemas: &mut BTreeMap<String, Value>) {
    for tool in value.as_array().into_iter().flatten() {
        if tool["type"] == "namespace" {
            collect_schemas(&tool["tools"], tool["name"].as_str(), schemas);
        } else if let Some(name) = tool["name"].as_str() {
            let mut tool = tool.clone();
            if let Some(namespace) = namespace {
                tool["_namespace"] = json!(namespace);
            }
            schemas.insert(name.into(), tool);
        }
    }
}

fn final_item() -> Value {
    json!({"type":"message", "id":"fixture-final", "role":"assistant", "status":"completed", "phase":"final_answer", "content":[{"type":"output_text", "text":"{\"ok\":true}"}]})
}

fn stream(sequence: usize, item: Value) -> ResponseTemplate {
    let id = format!("concurrent-response-{sequence}");
    let events = [
        json!({"type":"response.created", "response":{"id":id,"status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
        json!({"type":"response.completed", "response":{"id":id,"status":"completed","output":[item],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
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

fn private_directory(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn heartbeat(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|metadata| metadata.len())
}

fn request(prompt: &str) -> ExecutionRequest {
    ExecutionRequest {
        prompt: prompt.into(),
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

fn input(workspace: &Path) -> SocialTurnInput {
    SocialTurnInput {
        tools: vec!["workspace_exec".into()],
        workspace: Some(workspace.into()),
        ..Default::default()
    }
}

fn schema() -> Value {
    json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false})
}

#[tokio::test]
#[ignore = "requires native Codex and unprivileged bubblewrap; anonymous loopback provider only"]
async fn dropping_one_native_task_preserves_another_and_releases_its_own_scope() {
    let server = MockServer::start().await;
    let fixture = Arc::new(Mutex::new(Fixture::default()));
    let responder = fixture.clone();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |request: &Request| responder.lock().unwrap().respond(request))
        .mount(&server)
        .await;
    let root = tempfile::Builder::new()
        .prefix(".native-concurrent-smoke-")
        .tempdir_in(PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local"))
        .unwrap();
    private_directory(root.path());
    let runtime = root.path().join("runtime");
    for name in ["", "codex", "sqlite", "workspace"] {
        private_directory(&runtime.join(name));
    }
    let workspaces = root.path().join("workspaces");
    private_directory(&workspaces);
    let workspace_a = workspaces.join("g_3333333333333333333333333333333333333333");
    let workspace_b = workspaces.join("g_4444444444444444444444444444444444444444");
    private_directory(&workspace_a);
    private_directory(&workspace_b);
    let configuration = runtime.join("codex/config.toml");
    fs::write(&configuration,format!("model='{MODEL}'\nmodel_provider='fixture'\nweb_search='disabled'\n[model_providers.fixture]\nname='Offline concurrency fixture'\nbase_url='{}/v1'\nwire_api='responses'\nrequires_openai_auth=false\n",server.uri())).unwrap();
    fs::set_permissions(configuration, fs::Permissions::from_mode(0o600)).unwrap();
    // Exactly one authority handle and bot profile for both concurrent tasks.
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
            name: "clawmingle-offline-native-concurrency".into(),
            version: "1".into(),
            title: None,
        },
    )
    .await
    .expect("anonymous isolated authority");
    let scenario: anyhow::Result<()> = async {
        let (updates_tx,mut updates) = mpsc::unbounded_channel();
        let mut dropped = Box::pin(handle.execute_social_with_input(request(DROP_PROMPT),schema(),input(&workspace_a),None,Some(updates_tx)));
        let mut survivor = Box::pin(handle.execute_social_with_input(request(KEEP_PROMPT),schema(),input(&workspace_b),None,None));
        let heartbeat_a = workspace_a.join("dropped.heartbeat");
        let heartbeat_b = workspace_b.join("survivor.heartbeat");
        let mut root_a = None;
        timeout(Duration::from_secs(40),async {
            loop {
                tokio::select! {
                    result = &mut dropped => anyhow::bail!("A ended before drop: {result:?}"),
                    result = &mut survivor => anyhow::bail!("B ended before release: {result:?}"),
                    Some(update) = updates.recv() => {
                        if let ExecutionUpdate::SessionStarted{session_id} = update { root_a = Some(session_id); }
                    },
                    _ = sleep(Duration::from_millis(30)) => {
                        if root_a.is_some() && heartbeat(&heartbeat_a).is_some_and(|size|size>=3)
                            && heartbeat(&heartbeat_b).is_some_and(|size|size>=3) { break; }
                    }
                }
            }
            Ok::<(),anyhow::Error>(())
        }).await.context("both concurrent native tasks did not start")??;
        let root_a = root_a.context("A root thread was not announced")?;
        let before_drop_b = heartbeat(&heartbeat_b).unwrap();
        // Drop the owned future, not merely a Pin<&mut _>. This exercises the
        // detached NativeSession cleanup path without sending turn/interrupt.
        drop(dropped);
        fixture.lock().unwrap().recovery_allowed = true;
        let mut reopen_request = request("OFFLINE_CONCURRENT_REOPEN_A");
        reopen_request.session_state = serde_json::from_value(json!({"session_id":root_a}))?;
        // Background Drop cleanup may hold the executor lease briefly. Only
        // that exact busy error is retried; all retries and recovery together
        // have a 15-second bound. B remains polled throughout.
        let recovered = timeout(Duration::from_secs(15),async {
            loop {
                let mut reopening = Box::pin(handle.execute_social_with_input(reopen_request.clone(),schema(),input(&workspace_a),None,None));
                let result = tokio::select! {
                    result = &mut survivor => anyhow::bail!("dropping A terminated B: {result:?}"),
                    result = &mut reopening => result,
                };
                match result {
                    Ok(result) => break Ok::<_,anyhow::Error>(result),
                    Err(error) if format!("{error:#}").contains("native_environment_workspace_busy") => {
                        tokio::select! {
                            result = &mut survivor => anyhow::bail!("B terminated while A cleanup finished: {result:?}"),
                            _ = sleep(Duration::from_millis(100)) => {},
                        }
                    },
                    Err(error) => return Err(error.context("same-scope recovery failed")),
                }
            }
        }).await.context("Drop cleanup/reopening exceeded 15 seconds")??;
        ensure!(recovered.session_id.as_deref()==Some(root_a.as_str()),"recovery did not reuse A root");
        ensure!(serde_json::from_str::<Value>(&recovered.text)?==json!({"ok":true}),"A recovery did not finish");
        let stopped_a = heartbeat(&heartbeat_a).context("A heartbeat missing after Drop cleanup")?;
        let after_recovery_b = heartbeat(&heartbeat_b).context("B heartbeat disappeared")?;
        ensure!(after_recovery_b>before_drop_b,"B stopped running when A was dropped");
        let original_requests = fixture.lock().unwrap().original_requests;
        let stability_started = Instant::now();
        timeout(Duration::from_secs(3),async {
            loop {
                tokio::select! {
                    result = &mut survivor => anyhow::bail!("B ended before explicit release: {result:?}"),
                    _ = sleep(Duration::from_millis(50)) => {},
                }
                ensure!(heartbeat(&heartbeat_a)==Some(stopped_a),"dropped executor continues writing after recovery");
                if stability_started.elapsed()>=Duration::from_millis(600)
                    && heartbeat(&heartbeat_b).is_some_and(|size|size>after_recovery_b) { break; }
            }
            Ok::<(),anyhow::Error>(())
        }).await.context("B stopped making progress after A recovery")??;
        ensure!(heartbeat(&heartbeat_a)==Some(stopped_a),"dropped executor continues writing after recovery");
        ensure!(heartbeat(&heartbeat_b).is_some_and(|size|size>after_recovery_b),"surviving executor stopped after A recovery");
        fs::write(workspace_b.join("finish-survivor"),"release fixture task")?;
        let survived = timeout(Duration::from_secs(15),&mut survivor).await.context("B did not finish after release")??;
        ensure!(serde_json::from_str::<Value>(&survived.text)?==json!({"ok":true}),"B did not complete normally");
        ensure!(survived.session_id.as_deref()!=Some(root_a.as_str()),"concurrent tasks shared a root thread");
        ensure!(fs::read_to_string(workspace_a.join("reopened.txt"))?=="DROPPED_SCOPE_REOPENED","A recovered outside its scope");
        ensure!(fs::read_to_string(workspace_b.join("survivor-finished.txt"))?=="SURVIVOR_FINISHED","B completed outside its scope");
        ensure!(!workspace_b.join("reopened.txt").exists() && !workspace_a.join("survivor.heartbeat").exists(),"concurrent workspace scopes leaked");
        ensure!(heartbeat(&heartbeat_a)==Some(stopped_a),"A writer survived B completion");
        ensure!(!runtime.join("workspace/reopened.txt").exists() && !runtime.join("codex/auth.json").exists(),"fixture used authority workspace or imported auth");
        let observed = fixture.lock().unwrap();
        ensure!(observed.errors.is_empty(),"fixture errors={:?}; last output={:?}",observed.errors,observed.last_output);
        ensure!(observed.recovered && observed.survivor_finished,"provider did not observe both successful completions");
        ensure!(observed.original_requests==original_requests,"dropped worker still issues model requests");
        ensure!(survived.session_id.as_deref()==observed.survivor_id.as_deref(),"B root changed during A replacement");
        Ok(())
    }.await;
    handle.shutdown().await;
    if let Err(error) = scenario {
        let observed = fixture.lock().unwrap();
        panic!(
            "native concurrency/Drop fixture failed: {error:#}; errors={:?}; last output={:?}",
            observed.errors, observed.last_output
        );
    }
}
