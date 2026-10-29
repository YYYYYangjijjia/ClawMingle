//! Actual native V2 agents against an anonymous loopback Responses provider.
//! No personal profile, external model, QQ API, or browser installation is used.
#![cfg(target_os = "linux")]

use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
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
    sync::{mpsc, oneshot},
    time::{sleep, timeout},
};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

// Installed model metadata supplies ordinary Responses SSE and native tools.
// The model name is sent exclusively to this file's local fixture server.
const MODEL: &str = "gpt-5.5";
const HISTORY_MARKER: &str = "UNRELATED_GROUP_HISTORY_NOT_FOR_EMPTY_CHILD";
const PRIVATE_LOG: &str = "CHILD_INTERNAL_DETAIL_NOT_FOR_ROOT";
const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Inheritance,
    Cancel,
    Recover,
}

#[derive(Clone, Copy)]
enum Child {
    Full,
    None,
    Cancel,
}

struct Fixture {
    mode: Mode,
    root_id: Option<String>,
    root_step: usize,
    sequence: usize,
    pending: VecDeque<Child>,
    children: BTreeMap<String, (Child, usize)>,
    full_done: bool,
    none_done: bool,
    recovered: bool,
    errors: Vec<String>,
    last_output: String,
    full_initial_bytes: usize,
    empty_initial_bytes: usize,
    root_saw_handoff: bool,
    material_paths: Vec<String>,
}

impl Fixture {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            root_id: None,
            root_step: 0,
            sequence: 0,
            pending: VecDeque::new(),
            children: BTreeMap::new(),
            full_done: false,
            none_done: false,
            recovered: false,
            errors: Vec::new(),
            last_output: String::new(),
            full_initial_bytes: 0,
            empty_initial_bytes: 0,
            root_saw_handoff: false,
            material_paths: Vec::new(),
        }
    }

    fn respond(&mut self, request: &Request) -> ResponseTemplate {
        let body: Value = match serde_json::from_slice(&request.body) {
            Ok(value) => value,
            Err(_) => {
                self.errors
                    .push("provider payload is not plain JSON".into());
                return ResponseTemplate::new(400);
            }
        };
        // This public native request metadata identifies concurrent sessions;
        // never route by a prompt substring inherited into a fork's history.
        let metadata = &body["client_metadata"];
        let Some(thread) = metadata["thread_id"].as_str() else {
            self.errors
                .push("native request lacks client_metadata.thread_id".into());
            return ResponseTemplate::new(400);
        };
        let parent = metadata["x-codex-parent-thread-id"].as_str();
        let mut schemas = BTreeMap::new();
        collect_schemas(&body["tools"], None, &mut schemas);
        let outputs = tool_outputs(&body["input"]);
        if !outputs.is_empty() {
            self.last_output = outputs.chars().take(2048).collect();
        }
        self.sequence += 1;
        let item = if let Some(parent) = parent {
            if self.mode == Mode::Recover {
                self.errors
                    .push("old child made a model request after task worker shutdown".into());
            }
            if self.root_id.as_deref() != Some(parent) {
                self.errors
                    .push("child metadata points outside fixture root".into());
                final_item("UNEXPECTED_PARENT")
            } else {
                if !self.children.contains_key(thread) {
                    match self.pending.pop_front() {
                        Some(kind) => {
                            self.children.insert(thread.into(), (kind, 0));
                        }
                        None => {
                            self.errors
                                .push("unexpected child request after task closure".into());
                            return stream(self.sequence, final_item("UNEXPECTED_CHILD"));
                        }
                    }
                }
                let (kind, step) = *self.children.get(thread).unwrap();
                self.children.get_mut(thread).unwrap().1 += 1;
                self.child_response(kind, step, &body, &schemas, &outputs)
            }
        } else {
            if self.root_step == 0 {
                let manifest = material_manifest(&body);
                if let Some(path) = manifest["initial_image_files_in_attachment_order"][0].as_str()
                {
                    self.material_paths.push(path.to_owned());
                } else {
                    self.errors
                        .push("root lacks current image material path".into());
                }
            }
            if self.mode == Mode::Inheritance {
                let root_input = body["input"].to_string();
                if root_input.contains(PRIVATE_LOG) {
                    self.errors
                        .push("child tool detail leaked into root model input".into());
                }
                self.root_saw_handoff |= root_input.contains("EMPTY_CHILD_DONE");
                if !schemas.iter().any(|(name, schema)| {
                    name.ends_with("spawn_agent")
                        && schema.to_string().contains("ClawMingle context delegation")
                }) {
                    self.errors
                        .push("native spawn tool lacks context delegation policy".into());
                }
            }
            if let Some(root) = &self.root_id {
                if root != thread {
                    self.errors
                        .push("same-root resume used a different thread".into());
                }
            } else {
                self.root_id = Some(thread.into());
            }
            self.root_response(&schemas, &outputs)
        };
        stream(self.sequence, item)
    }

    fn root_response(&mut self, schemas: &BTreeMap<String, Value>, outputs: &str) -> Value {
        match self.mode {
            Mode::Inheritance => match self.root_step {
                0 => {
                    self.root_step = 1;
                    self.pending.push_back(Child::Full);
                    self.call(schemas, "spawn_agent", json!({"task_name":"full", "message":"Offline full fork fixture.", "fork_turns":"all"}))
                }
                1 if self.full_done => {
                    self.root_step = 2;
                    self.pending.push_back(Child::None);
                    self.call(schemas, "spawn_agent", json!({"task_name":"empty", "message":"Offline empty fork fixture. Goal: verify the selected image with native view_image and selected Skill file. Constraints: read only task materials. Save evidence to /workspace/empty-child.txt. Return EMPTY_CHILD_DONE and the artifact path; keep raw logs here.", "fork_turns":"none"}))
                }
                2 if self.none_done && self.root_saw_handoff => final_item("{\"ok\":true}"),
                _ => self.call(schemas, "wait_agent", json!({"timeout_ms":10000})),
            },
            Mode::Cancel => match self.root_step {
                0 => {
                    self.root_step = 1;
                    self.pending.push_back(Child::Cancel);
                    self.call(schemas, "spawn_agent", json!({"task_name":"cancel_worker", "message":"Offline child cancellation fixture.", "fork_turns":"all"}))
                }
                1 => {
                    self.root_step = 2;
                    self.call(schemas, "exec_command", json!({"cmd":heartbeat("agent-parent"), "workdir":"/workspace", "yield_time_ms":10000, "max_output_tokens":1000}))
                }
                _ => self.call(schemas, "wait_agent", json!({"timeout_ms":10000})),
            },
            Mode::Recover => {
                if self.root_step == 0 {
                    self.root_step = 1;
                    let old = self.material_paths.first().cloned().unwrap_or_default();
                    let new = self.material_paths.last().cloned().unwrap_or_default();
                    let command = format!(
                        "python3 - <<'PY'\nimport pathlib\np=pathlib.Path('/workspace')\nassert (p/'agent-parent.heartbeat').is_file()\nassert (p/'agent-child.heartbeat').is_file()\nassert not pathlib.Path({old:?}).exists()\nassert pathlib.Path({new:?}).read_bytes().startswith(bytes([137,80,78,71]))\n(p/'agents-recovered.txt').write_text('NATIVE_AGENTS_RESUMED')\nprint('NATIVE_AGENTS_RESUMED')\nPY"
                    );
                    self.call(
                        schemas,
                        "exec_command",
                        json!({"cmd":command, "workdir":"/workspace", "max_output_tokens":1000}),
                    )
                } else {
                    self.recovered = outputs.contains("NATIVE_AGENTS_RESUMED");
                    final_item("{\"ok\":true}")
                }
            }
        }
    }

    fn child_response(
        &mut self,
        kind: Child,
        step: usize,
        body: &Value,
        schemas: &BTreeMap<String, Value>,
        outputs: &str,
    ) -> Value {
        if step == 0 {
            let catalog = format!("{}{}", body["instructions"], body["input"]);
            let has_dynamic = schemas.keys().any(|name| name.ends_with("message_lookup"));
            let has_exec = schemas.keys().any(|name| name.ends_with("exec_command"));
            let has_skill = catalog.contains("NATIVE_AGENT_SKILL_DISCOVERY");
            if !has_exec {
                self.errors.push("child lacks native exec".into());
            }
            if matches!(kind, Child::Full) && (!has_dynamic || !has_skill) {
                self.errors.push(format!(
                    "full fork inheritance missing: dynamic={has_dynamic}, skill={has_skill}"
                ));
            }
            if matches!(kind, Child::None) && (has_dynamic || has_skill) {
                self.errors.push("empty fork unexpectedly inherited thread dynamic tools/Skill discovery; recheck upstream contract".into());
            }
            if self.mode == Mode::Inheritance {
                let manifest = material_manifest(body);
                if !catalog.contains("You are a ClawMingle task worker")
                    || manifest["selected_skill_files"] != json!(["/skills/native-agent/SKILL.md"])
                {
                    self.errors
                        .push("worker protocol/explicit selected Skill manifest missing".into());
                }
                match kind {
                    Child::Full => {
                        self.full_initial_bytes = body["input"].to_string().len();
                        if !catalog.contains(HISTORY_MARKER) {
                            self.errors
                                .push("full fork fixture lost baseline history".into());
                        }
                    }
                    Child::None => {
                        self.empty_initial_bytes = body["input"].to_string().len();
                        if catalog.contains(HISTORY_MARKER)
                            || !catalog.contains("Goal: verify the selected image")
                        {
                            self.errors
                                .push("empty fork copied history or lost task brief".into());
                        }
                    }
                    Child::Cancel => {}
                }
            }
            if catalog.contains("NATIVE_UNSELECTED_AGENT_SKILL") {
                self.errors
                    .push("unselected Skill appeared in child instructions".into());
            }
            let command = if matches!(kind, Child::Cancel) {
                heartbeat("agent-child")
            } else {
                let marker = if matches!(kind, Child::Full) {
                    "full-child"
                } else {
                    "empty-child"
                };
                let image = material_manifest(body)["initial_image_files_in_attachment_order"][0]
                    .as_str()
                    .unwrap_or("/missing-fixture-image")
                    .to_owned();
                format!(
                    "python3 - <<'PY'\nimport pathlib\np=pathlib.Path('/workspace')\nassert not pathlib.Path('/state/codex/auth.json').exists()\nassert not pathlib.Path('/state/codex/authority-canary.txt').exists()\nassert 'NATIVE_AGENT_SKILL_CONTENT' in pathlib.Path('/skills/native-agent/SKILL.md').read_text()\nassert not pathlib.Path('/skills/unselected-agent/SKILL.md').exists()\nimage=pathlib.Path({image:?})\nassert image.read_bytes().startswith(bytes([137,80,78,71]))\ntry:\n image.write_bytes(b'overwrite')\nexcept OSError:\n pass\nelse:\n raise AssertionError('task image is writable')\n(p/'{marker}.txt').write_text('NATIVE_CHILD_ENV_OK')\nprint('NATIVE_CHILD_ENV_OK')\nprint('{PRIVATE_LOG}'*120)\nPY"
                )
            };
            return self.call(schemas, "exec_command", json!({"cmd":command, "workdir":"/workspace", "yield_time_ms":10000, "max_output_tokens":1000}));
        }
        match kind {
            Child::Full if step == 1 => {
                if !outputs.contains("NATIVE_CHILD_ENV_OK") {
                    self.errors
                        .push("full child executor output missing".into());
                }
                self.call(
                    schemas,
                    "message_lookup",
                    json!({"message_id":"fixture-message"}),
                )
            }
            Child::Full => {
                self.full_done = outputs.contains("NATIVE_CHILD_SCOPE_READ");
                if !self.full_done {
                    self.errors
                        .push("child dynamic request was not authorized in its root scope".into());
                }
                final_item("FULL_CHILD_DONE")
            }
            Child::None if step == 1 => {
                self.none_done = outputs.contains("NATIVE_CHILD_ENV_OK");
                if !self.none_done {
                    self.errors
                        .push("empty child executor output missing".into());
                }
                let manifest = material_manifest(body);
                self.call(
                    schemas,
                    "view_image",
                    json!({"path":manifest["initial_image_files_in_attachment_order"][0]}),
                )
            }
            Child::None => {
                if !body["input"].to_string().contains("data:image/") {
                    self.errors
                        .push("empty child did not receive native image pixels".into());
                }
                final_item("EMPTY_CHILD_DONE; verified image and Skill; /workspace/empty-child.txt")
            }
            Child::Cancel => self.call(schemas, "wait_agent", json!({"timeout_ms":10000})),
        }
    }

    fn call(&mut self, schemas: &BTreeMap<String, Value>, suffix: &str, arguments: Value) -> Value {
        let name = schemas
            .keys()
            .find(|name| name.ends_with(suffix))
            .cloned()
            .unwrap_or_else(|| {
                self.errors.push(format!(
                    "native schema missing: {suffix}; names={:?}",
                    schemas.keys().collect::<Vec<_>>()
                ));
                suffix.into()
            });
        let mut item = json!({"type":"function_call", "id":format!("item-{}",self.sequence), "call_id":format!("call-{}",self.sequence), "name":name, "arguments":arguments.to_string(), "status":"completed"});
        if let Some(namespace) = schemas
            .get(&name)
            .and_then(|schema| schema["_namespace"].as_str())
        {
            item["namespace"] = json!(namespace);
        }
        item
    }
}

fn material_manifest(value: &Value) -> Value {
    match value {
        Value::String(text) => text
            .lines()
            .find_map(|line| {
                line.strip_prefix("Current execution material manifest: ")
                    .and_then(|json| serde_json::from_str(json).ok())
            })
            .unwrap_or(Value::Null),
        Value::Array(values) => values
            .iter()
            .map(material_manifest)
            .find(|value| !value.is_null())
            .unwrap_or(Value::Null),
        Value::Object(values) => values
            .values()
            .map(material_manifest)
            .find(|value| !value.is_null())
            .unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn collect_schemas(value: &Value, namespace: Option<&str>, schemas: &mut BTreeMap<String, Value>) {
    for tool in value.as_array().into_iter().flatten() {
        if tool["type"] == "namespace" {
            collect_schemas(&tool["tools"], tool["name"].as_str(), schemas);
        } else if let Some(name) = tool["name"].as_str() {
            let mut schema = tool.clone();
            if let Some(namespace) = namespace {
                schema["_namespace"] = json!(namespace);
            }
            schemas.insert(name.into(), schema);
        }
    }
}

fn tool_outputs(input: &Value) -> String {
    input
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "function_call_output")
        .map(|item| item["output"].to_string())
        .collect()
}

fn final_item(text: &str) -> Value {
    json!({"type":"message", "id":"fixture-final", "role":"assistant", "status":"completed", "phase":"final_answer", "content":[{"type":"output_text", "text":text}]})
}

fn stream(sequence: usize, item: Value) -> ResponseTemplate {
    let id = format!("fixture-response-{sequence}");
    let events = [
        json!({"type":"response.created", "response":{"id":id, "status":"in_progress", "output":[]}}),
        json!({"type":"response.output_item.done", "output_index":0, "item":item}),
        json!({"type":"response.completed", "response":{"id":id, "status":"completed", "output":[item], "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
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

fn heartbeat(name: &str) -> String {
    format!(
        "python3 -u - <<'PY'\nimport pathlib,signal,time\nsignal.signal(signal.SIGTERM,signal.SIG_IGN)\nwith pathlib.Path('/workspace/{name}.heartbeat').open('ab',buffering=0) as f:\n while True:\n  f.write(b'x')\n  time.sleep(0.04)\nPY"
    )
}

fn heartbeat_lengths(workspace: &Path) -> Option<[u64; 2]> {
    Some([
        fs::metadata(workspace.join("agent-parent.heartbeat"))
            .ok()?
            .len(),
        fs::metadata(workspace.join("agent-child.heartbeat"))
            .ok()?
            .len(),
    ])
}

fn private_directory(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

struct Harness {
    _root: tempfile::TempDir,
    _server: MockServer,
    handle: AppServerHandle,
    fixture: Arc<Mutex<Fixture>>,
    workspace: PathBuf,
    runtime: PathBuf,
    skill: PathBuf,
    reads: Arc<AtomicUsize>,
}

impl Harness {
    async fn start(mode: Mode) -> Self {
        let server = MockServer::start().await;
        let fixture = Arc::new(Mutex::new(Fixture::new(mode)));
        let responder = fixture.clone();
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(move |request: &Request| responder.lock().unwrap().respond(request))
            .mount(&server)
            .await;
        let root = tempfile::Builder::new()
            .prefix(".native-agents-smoke-")
            .tempdir_in(PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local"))
            .unwrap();
        private_directory(root.path());
        let runtime = root.path().join("runtime");
        for name in ["", "codex", "sqlite", "workspace"] {
            private_directory(&runtime.join(name));
        }
        fs::write(
            runtime.join("codex/authority-canary.txt"),
            "AUTHORITY_ONLY_FIXTURE",
        )
        .unwrap();
        private_directory(&root.path().join("workspaces"));
        private_directory(&root.path().join("skills"));
        let workspace = root
            .path()
            .join("workspaces/g_2222222222222222222222222222222222222222");
        private_directory(&workspace);
        let skill = root.path().join("skills/native-agent");
        for (name, description) in [
            ("native-agent", "NATIVE_AGENT_SKILL_DISCOVERY"),
            ("unselected-agent", "NATIVE_UNSELECTED_AGENT_SKILL"),
        ] {
            let directory = root.path().join("skills").join(name);
            private_directory(&directory);
            let path = directory.join("SKILL.md");
            fs::write(&path, format!("---\nname: {name}\ndescription: {description}\n---\nNATIVE_AGENT_SKILL_CONTENT\n")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let config = runtime.join("codex/config.toml");
        fs::write(&config, format!("model='{MODEL}'\nmodel_provider='fixture'\nweb_search='disabled'\n[model_providers.fixture]\nname='Offline native agent fixture'\nbase_url='{}/v1'\nwire_api='responses'\nrequires_openai_auth=false\n", server.uri())).unwrap();
        fs::set_permissions(config, fs::Permissions::from_mode(0o600)).unwrap();
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
                name: "clawmingle-offline-native-agents".into(),
                version: "1".into(),
                title: None,
            },
        )
        .await
        .expect("anonymous app-server");
        Self {
            _root: root,
            _server: server,
            handle,
            fixture,
            workspace,
            runtime,
            skill,
            reads: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn input(&self) -> SocialTurnInput {
        let reads = self.reads.clone();
        SocialTurnInput {
            images: vec![format!("data:image/png;base64,{PNG}")],
            tools: vec!["workspace_exec".into(), "message_lookup".into()],
            workspace: Some(self.workspace.clone()),
            skill_packages: vec![self.skill.clone()],
            messages: Some(Arc::new(move |name, arguments| {
                let value: Value = serde_json::from_str(arguments).unwrap();
                assert_eq!(name, "message_lookup");
                assert_eq!(value["message_id"], "fixture-message");
                reads.fetch_add(1, Ordering::SeqCst);
                json!({"ok":true, "marker":"NATIVE_CHILD_SCOPE_READ"})
            })),
            ..Default::default()
        }
    }

    fn request(&self) -> ExecutionRequest {
        ExecutionRequest {
            prompt: if self.fixture.lock().unwrap().mode == Mode::Inheritance {
                format!(
                    "Anonymous native subagent protocol fixture.\n{}",
                    HISTORY_MARKER.repeat(1600)
                )
            } else {
                "Anonymous native subagent protocol fixture.".into()
            },
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

    fn check(&self) -> anyhow::Result<()> {
        let fixture = self.fixture.lock().unwrap();
        ensure!(
            fixture.errors.is_empty(),
            "fixture errors={:?}; last tool output={:?}",
            fixture.errors,
            fixture.last_output
        );
        ensure!(
            !self.runtime.join("codex/auth.json").exists(),
            "anonymous fixture imported auth"
        );
        Ok(())
    }
}

fn schema() -> Value {
    json!({"type":"object", "properties":{"ok":{"type":"boolean"}}, "required":["ok"], "additionalProperties":false})
}

#[tokio::test]
#[ignore = "requires native Codex 0.161 and unprivileged bubblewrap; anonymous offline provider only"]
async fn native_agents_inherit_scope_tools_and_selected_skills_with_fork_boundary() {
    let harness = Harness::start(Mode::Inheritance).await;
    let result = timeout(
        Duration::from_secs(75),
        harness.handle.execute_social_with_input(
            harness.request(),
            schema(),
            harness.input(),
            None,
            None,
        ),
    )
    .await;
    harness.handle.shutdown().await;
    harness.check().unwrap();
    let result = result
        .expect("native agent inheritance deadline")
        .expect("native agent task");
    assert_eq!(
        serde_json::from_str::<Value>(&result.text).unwrap(),
        json!({"ok":true})
    );
    assert_eq!(
        harness.reads.load(Ordering::SeqCst),
        1,
        "only full child may invoke the scoped message reader"
    );
    for name in ["full-child.txt", "empty-child.txt"] {
        assert_eq!(
            fs::read_to_string(harness.workspace.join(name)).unwrap(),
            "NATIVE_CHILD_ENV_OK"
        );
        assert!(
            !harness.runtime.join("workspace").join(name).exists(),
            "child used authority executor"
        );
    }
    let fixture = harness.fixture.lock().unwrap();
    assert!(fixture.full_done && fixture.none_done);
    assert!(fixture.root_saw_handoff);
    assert!(fixture.full_initial_bytes > fixture.empty_initial_bytes + HISTORY_MARKER.len() * 1500);
    eprintln!(
        "Offline fixture serialized input bytes: full={}, empty={}; not token usage",
        fixture.full_initial_bytes, fixture.empty_initial_bytes
    );
    assert_eq!(
        fixture.children.len(),
        2,
        "fixture must actually create two native child threads"
    );
}

#[tokio::test]
#[ignore = "requires native Codex 0.161 and unprivileged bubblewrap; anonymous offline provider only"]
async fn cancelling_native_agent_tree_stops_parent_and_child_and_allows_same_root_resume() {
    let harness = Harness::start(Mode::Cancel).await;
    let scenario: anyhow::Result<()> = async {
        let (cancel_tx,cancel_rx) = oneshot::channel();
        let (updates_tx,mut updates) = mpsc::unbounded_channel();
        let mut request = harness.request();
        let root_id = {
            let execution = harness.handle.execute_social_with_input(request.clone(),schema(),harness.input(),Some(cancel_rx),Some(updates_tx));
            tokio::pin!(execution);
            let mut root_id = None;
            timeout(Duration::from_secs(35),async {
                loop {
                    tokio::select! {
                        result = &mut execution => {
                            let fixture = harness.fixture.lock().unwrap();
                            anyhow::bail!("task ended before both native agents ran: {result:?}; errors={:?}; output={:?}",fixture.errors,fixture.last_output);
                        },
                        update = updates.recv() => {
                            if let Some(ExecutionUpdate::SessionStarted{session_id}) = update { root_id = Some(session_id); }
                        },
                        _ = sleep(Duration::from_millis(30)) => {
                            if root_id.is_some() && heartbeat_lengths(&harness.workspace).is_some_and(|lengths|lengths.iter().all(|length|*length>=3)) { break; }
                        }
                    }
                }
                Ok::<(),anyhow::Error>(())
            }).await.context("native agents did not both start")??;
            cancel_tx.send(()).map_err(|_|anyhow::anyhow!("cancel receiver disappeared"))?;
            let result = timeout(Duration::from_secs(15),&mut execution).await.context("tree cancellation did not settle")?;
            let error = result.err().context("cancelled task unexpectedly succeeded")?;
            ensure!(error.to_string().contains("aborted by user"),"wrong cancellation failure: {error:#}");
            root_id.context("root session not announced")?
        };
        let stopped = heartbeat_lengths(&harness.workspace).context("missing heartbeat files")?;
        {
            let mut fixture = harness.fixture.lock().unwrap();
            fixture.mode = Mode::Recover;
            fixture.root_step = 0;
        }
        request.session_state = serde_json::from_value(json!({"session_id":root_id}))?;
        // No retry or pause: cancellation returning must release the scope lease.
        let resumed = timeout(Duration::from_secs(35),harness.handle.execute_social_with_input(request,schema(),harness.input(),None,None)).await.context("same-root resume deadline")??;
        ensure!(resumed.session_id.as_deref()==Some(root_id.as_str()),"root identity changed on resume");
        ensure!(serde_json::from_str::<Value>(&resumed.text)?==json!({"ok":true}),"resume did not finish");
        sleep(Duration::from_millis(600)).await;
        ensure!(heartbeat_lengths(&harness.workspace)==Some(stopped),"cancelled native parent/child continues writing");
        ensure!(harness.fixture.lock().unwrap().recovered,"resumed executor did not return its marker");
        ensure!(fs::read_to_string(harness.workspace.join("agents-recovered.txt"))?=="NATIVE_AGENTS_RESUMED","resume used wrong workspace");
        harness.check()?;
        Ok(())
    }.await;
    harness.handle.shutdown().await;
    scenario.expect("native agent tree cancellation and root recovery");
}
