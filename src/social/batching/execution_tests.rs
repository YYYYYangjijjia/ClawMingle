use super::*;
use crate::model::social::{
    Bubble, ChatMessage, DecisionStatus, DeliveryStatus, ImageAttachment, RequestRecord,
};

fn members() -> Vec<Member> {
    vec![Member { id: 1, revision: 1 }, Member { id: 2, revision: 1 }]
}

#[test]
fn shared_sources_are_unique_and_silent_images_and_voice_remain_context() {
    let (_dir, config, history, mut state) = crate::social::batching::tests::fixture();
    let seq = state
        .ingest(
            ChatMessage {
                id: "silent-picture".into(),
                author_id: "member-one".into(),
                text: "look at this".into(),
                images: vec![ImageAttachment {
                    url: "https://example.invalid/picture.png".into(),
                    content_type: "image/png".into(),
                }],
                ..Default::default()
            },
            100,
        )
        .unwrap()
        .unwrap();
    state.messages.last_mut().unwrap().status = DecisionStatus::Decided;
    let batch = Batch::prepare(&state, members(), &config, &history, false)
        .unwrap()
        .unwrap();
    let input: Value = serde_json::from_str(batch.prompt.rsplit_once('\n').unwrap().1).unwrap();
    let sources = input["current_request"]["messages"].as_array().unwrap();
    assert!(
        input["input_budget"]["notice"]
            .as_str()
            .unwrap()
            .contains("return defer")
    );
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0]["text"], "question-1");
    assert_eq!(sources[1]["text"], "question-2");
    assert!(
        input["group_context"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["seq"] == seq)
    );
    assert!(
        image_inputs::identity(&image_inputs::collect(&batch.view, 1, &config).unwrap())
            .to_string()
            .contains("context")
    );
    assert_eq!(batch.view.requests[&1].source_seqs, vec![1, 2]);
    assert_eq!(state.requests[&1].source_seqs, vec![1]);
}

#[test]
fn recent_deliveries_are_chronological_and_required_context_is_never_truncated_to_fit() {
    let (_dir, config, history, mut state) = crate::social::batching::tests::fixture();
    state.requests.insert(
        9,
        RequestRecord {
            id: 9,
            status: RequestStatus::Completed,
            bubbles: (1..=8)
                .map(|time| Bubble {
                    text: format!("reply-{time}"),
                    sent_at_ms: Some(time),
                    status: DeliveryStatus::Sent,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        },
    );
    let batch = Batch::prepare(&state, members(), &config, &history, false)
        .unwrap()
        .unwrap();
    let input: Value = serde_json::from_str(batch.prompt.rsplit_once('\n').unwrap().1).unwrap();
    let deliveries = input["already_delivered_bot_messages"].as_array().unwrap();
    assert_eq!(deliveries.first().unwrap()["text"], "reply-3");
    assert_eq!(deliveries.last().unwrap()["text"], "reply-8");
    state.messages[0].message.text = "不可截断的当前要求".repeat(2000);
    assert!(
        Batch::prepare(&state, members(), &config, &history, false)
            .unwrap()
            .is_none()
    );
    assert!(state.messages[0].message.text.len() > 12_000);
}

#[tokio::test]
#[ignore = "actual native Codex with an anonymous loopback fixture; no account or QQ"]
async fn native_batch_submits_two_requests_once_without_operating_tools_or_shared_continuation() {
    use crate::codex::{ClientInfo, IsolatedAppServerConfig};
    use std::{fs, os::unix::fs::PermissionsExt, sync::Mutex};
    use wiremock::{
        Mock, MockServer, Request, ResponseTemplate,
        matchers::{method, path},
    };
    let server = MockServer::start().await;
    let observed: Arc<Mutex<Vec<Value>>> = Arc::default();
    let responses = observed.clone();
    let text=json!({"results":([1,2].map(|id|json!({"request_id":id,"revision":1,"outcome":"answer","bubbles":[format!("answer-{id}")],"quote_seq":id,"mentions":[]})))}).to_string();
    Mock::given(method("POST")).and(path("/v1/responses")).respond_with(move|request:&Request| {
        let body:Value=serde_json::from_slice(&request.body).unwrap();responses.lock().unwrap().push(body);
        let item=json!({"type":"message","id":"batch-final","role":"assistant","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":text}]});
        let events=[json!({"type":"response.created","response":{"id":"batch-response","status":"in_progress","output":[]}}),
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
            json!({"type":"response.completed","response":{"id":"batch-response","status":"completed","output":[item],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}})];
        ResponseTemplate::new(200).insert_header("content-type","text/event-stream").set_body_string(events.into_iter().map(|e|format!("data: {e}\n\n")).collect::<String>())
    }).mount(&server).await;
    let root = tempfile::Builder::new()
        .prefix(".reply-batch-fixture-")
        .tempdir_in(PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local"))
        .unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = root.path().join("runtime");
    for name in ["", "codex", "sqlite", "workspace"] {
        let p = runtime.join(name);
        fs::create_dir_all(&p).unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let file = runtime.join("codex/config.toml");
    // Start with live search enabled: the batch must explicitly disable it.
    fs::write(&file,format!("model='gpt-5.5'\nmodel_provider='fixture'\nweb_search='live'\n[model_providers.fixture]\nname='Offline batching fixture'\nbase_url='{}/v1'\nwire_api='responses'\nrequires_openai_auth=false\n",server.uri())).unwrap();
    fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
    let handle = Arc::new(
        AppServerHandle::start_isolated(
            IsolatedAppServerConfig {
                bubblewrap_binary: "/usr/bin/bwrap".into(),
                codex_binary: "/opt/codex/current/bin/codex".into(),
                runtime_root: runtime,
                provider_environment: BTreeMap::new(),
                selected_provider: None,
                image_worker: false,
                codex_app_worker: false,
            },
            ClientInfo {
                name: "offline-batch-check".into(),
                version: "1".into(),
                title: None,
            },
        )
        .await
        .unwrap(),
    );
    let (_dir, mut config, history, mut state) = crate::social::batching::tests::fixture();
    config.codex.model = "gpt-5.5".into();
    config.codex.reasoning_effort = "low".into();
    let media = Arc::new(
        MediaStore::new(root.path(), "g_3333333333333333333333333333333333333333").unwrap(),
    );
    let batch = Batch::prepare(&state, members(), &config, &history, false)
        .unwrap()
        .unwrap();
    batch.begin(&mut state).unwrap();
    let (_cancel, rx) = oneshot::channel();
    let (updates, _) = mpsc::unbounded_channel();
    let result = execute(
        handle.clone(),
        batch.clone(),
        media,
        root.path().join("images"),
        handle.try_social_admission().unwrap(),
        rx,
        updates,
    )
    .await;
    handle.shutdown().await;
    batch.complete(&mut state, &result.unwrap(), false).unwrap();
    let requests = observed.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]["tools"].as_array().is_none_or(|tools| tools
            .iter()
            .all(|tool| tool["type"] == "function" && tool["name"] == "request_user_input")),
        "batch unexpectedly exposed tools: {:?}",
        requests[0]["tools"].as_array().map(|tools| tools
            .iter()
            .map(|tool| (&tool["type"], &tool["name"]))
            .collect::<Vec<_>>())
    );
    let input = requests[0]["input"].to_string();
    assert!(input.contains("question-1") && input.contains("question-2"));
    assert!(
        state
            .requests
            .values()
            .all(|r| r.status == RequestStatus::Ready && r.codex_thread_id.is_none())
    );
}
