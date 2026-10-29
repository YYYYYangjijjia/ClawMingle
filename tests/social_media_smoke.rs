//! Opt-in real vision and workspace-image test; never connects to QQ or Jev.
//! Run only against the bot's independent profile with the bot stopped.

#![cfg(target_os = "linux")]

use base64::{Engine as _, engine::general_purpose::STANDARD};
use codex_claw::{
    codex::{
        AppServerHandle, ClientInfo, ExecutionRequest, IsolatedAppServerConfig, SOCIAL_CODEX_HOME,
        SOCIAL_WORKSPACE, app_server::SocialTurnInput,
    },
    session::state::{ReasoningEffort, SessionState},
    social::SocialConfig,
};
use fs2::FileExt;
use serde::Deserialize;
use serde_json::json;
use sha1::{Digest, Sha1};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    time::Duration,
};

// Synthetic image fixture. Its meaning is deliberately absent from the prompt.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAIAAAD8GO2jAAAAJklEQVR4nO3NsQkAAAjAsP7/tF7hIASyp6ZbAoFAIBAIBAKB4EuwNof8LkGrxSIAAAAASUVORK5CYII=";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    bubbles: Vec<String>,
    images: Vec<String>,
}

#[tokio::test]
#[ignore = "requires explicit independent bot config, stopped bot, login and real model request"]
async fn vision_understands_pixels_and_copies_an_image_in_its_own_workspace() {
    let config = SocialConfig::load(&PathBuf::from(
        std::env::var_os("CLAWMINGLE_SOCIAL_CONFIG").expect("media_smoke_config_required"),
    ))
    .unwrap_or_else(|_| panic!("media_smoke_config_invalid"));
    assert!(
        config.codex.credentials_file.is_none()
            && config
                .codex
                .provider
                .as_deref()
                .is_none_or(|p| p == "openai"),
        "media_smoke_requires_bot_chatgpt_profile"
    );
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(0o400000 | 0o4000)
        .open(config.data_dir.join("operator.lock"))
        .unwrap();
    let metadata = lock.metadata().unwrap();
    assert!(
        metadata.is_file()
            && metadata.uid() == std::fs::metadata("/proc/self").unwrap().uid()
            && metadata.permissions().mode() & 0o777 == 0o600
            && metadata.nlink() == 1
    );
    lock.try_lock_exclusive()
        .expect("media_smoke_bot_must_be_stopped");
    let root = config.data_dir.join("workspaces");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)
        .unwrap();
    let workspace = root.join(format!(
        "g_{:x}",
        Sha1::digest(format!("media-smoke-{}", ulid::Ulid::new()).as_bytes())
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&workspace)
        .unwrap();
    let bytes = STANDARD.decode(PNG).unwrap();
    std::fs::write(workspace.join("input.png"), &bytes).unwrap();
    let handle = AppServerHandle::start_isolated(
        IsolatedAppServerConfig {
            bubblewrap_binary: config.codex.bubblewrap_binary,
            codex_binary: config.codex.binary,
            runtime_root: config.data_dir.join("runtime"),
            provider_environment: BTreeMap::new(),
            selected_provider: config.codex.provider,
            image_worker: false,
            codex_app_worker: false,
        },
        ClientInfo {
            name: "clawmingle-media-smoke".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: None,
        },
    )
    .await
    .unwrap_or_else(|_| panic!("media_smoke_start_failed"));
    let request=ExecutionRequest {
        prompt:"请看附图，第一条文字气泡只回答图片的主要颜色（中文颜色词）。然后实际调用 clawmingle.workspace_exec 将 /workspace/input.png 原样复制到 /workspace/echo.png，并读回验证。不要画图或联网。最后返回JSON：bubbles为颜色词，images只包含 /workspace/echo.png。若执行失败请如实回答，不声称完成。".into(),
        workspace_dir:SOCIAL_WORKSPACE.into(),codex_home:SOCIAL_CODEX_HOME.into(),
        config_overrides:Vec::new(),add_dirs:Vec::new(),session_state:SessionState::default(),
        model:Some(config.codex.model),service_tier:None,context_mode:None,reasoning_effort:ReasoningEffort::High,image_paths:Vec::new(),
    };
    let schema = json!({"type":"object","properties":{"bubbles":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":2},"images":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":1}},"required":["bubbles","images"],"additionalProperties":false});
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        handle.execute_social_with_input(
            request.clone(),
            schema.clone(),
            SocialTurnInput {
                steer: None,
                messages: None,
                images: vec![format!("data:image/png;base64,{PNG}")],
                tools: vec!["workspace_exec".into()],
                workspace: Some(workspace.clone()),
                ..Default::default()
            },
            None,
            None,
        ),
    )
    .await;
    let followup = if let Ok(Ok(first)) = &result {
        let mut followup = request;
        followup.session_state =
            serde_json::from_value(json!({"session_id":first.session_id})).unwrap();
        followup.prompt = "This application sends text and images according to your JSON reply plan. Only already delivered messages count as sent. To send a shared original PNG/JPEG, copy its application-provided image_ref into images; for a code-created image use its /workspace path. Never guess cache names. Earlier images are not new visual inputs: do not invent a fresh inspection. Do not attach images unsolicited. Application-staged images from this conversation: [{\"image_index\":1,\"image_ref\":\"input:1\",\"path\":\"/workspace/input.png\",\"mime\":\"image/png\",\"current_request\":false,\"expires_in_seconds\":80000}]. Current user message: 刚才我发给你的那张原图，也发我一下。".into();
        let mut followup_schema = schema;
        followup_schema["properties"]["bubbles"]["minItems"] = json!(0);
        followup_schema["properties"]["images"]["minItems"] = json!(0);
        Some(
            tokio::time::timeout(
                Duration::from_secs(120),
                handle.execute_social_with_input(
                    followup,
                    followup_schema,
                    SocialTurnInput {
                        steer: None,
                        messages: None,
                        images: Vec::new(),
                        tools: Vec::new(),
                        workspace: Some(workspace.clone()),
                        ..Default::default()
                    },
                    None,
                    None,
                ),
            )
            .await,
        )
    } else {
        None
    };
    handle.shutdown().await;
    let result = result
        .unwrap_or_else(|_| panic!("media_smoke_timed_out"))
        .unwrap_or_else(|_| panic!("media_smoke_turn_failed"));
    let reply: Reply =
        serde_json::from_str(&result.text).unwrap_or_else(|_| panic!("media_smoke_reply_invalid"));
    assert!(
        reply.bubbles.iter().any(|s| s.contains('绿')),
        "media_smoke_visual_answer_incorrect"
    );
    assert_eq!(reply.images, ["/workspace/echo.png"]);
    assert!(
        result.generated_images.is_empty(),
        "media_smoke_native_generation_must_be_disabled"
    );
    assert_eq!(
        std::fs::read(workspace.join("echo.png")).expect("media_smoke_tool_created_output"),
        bytes
    );
    let followup = followup
        .expect("media_smoke_followup_requires_initial_turn")
        .unwrap_or_else(|_| panic!("media_smoke_followup_timed_out"))
        .unwrap_or_else(|_| panic!("media_smoke_followup_failed"));
    let reply: Reply = serde_json::from_str(&followup.text)
        .unwrap_or_else(|_| panic!("media_smoke_followup_reply_invalid"));
    assert_eq!(
        reply.images,
        ["input:1"],
        "media_smoke_followup_must_reference_shared_original"
    );
    println!(
        "Real vision, isolated image copy and natural-language original-image followup passed."
    );
}
