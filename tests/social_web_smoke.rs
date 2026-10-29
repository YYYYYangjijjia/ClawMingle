//! Explicitly opted-in real model/web smoke test; never connects to QQ or Jev.
//!
//! Stop the bot first and set CLAWMINGLE_SOCIAL_CONFIG to its private config.
//! This uses only that bot's existing independent runtime profile and holds its
//! operator lock until the app-server shuts down. No personal profile is read.
//! A URL in the final answer does not prove a search occurred: separately inspect
//! the new independent rollout for a completed web-search tool invocation.

#![cfg(target_os = "linux")]

use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use codex_claw::{
    codex::{
        AppServerHandle, ClientInfo, ExecutionRequest, IsolatedAppServerConfig, SOCIAL_CODEX_HOME,
        SOCIAL_WORKSPACE,
    },
    session::state::{ReasoningEffort, SessionState},
    social::SocialConfig,
};
use fs2::FileExt;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bubbles {
    bubbles: Vec<String>,
}

fn private_directory(path: &Path) {
    let metadata = std::fs::symlink_metadata(path)
        .unwrap_or_else(|_| panic!("web_smoke_private_directory_unavailable"));
    let owner = std::fs::metadata("/proc/self")
        .unwrap_or_else(|_| panic!("web_smoke_owner_unavailable"))
        .uid();
    assert!(
        metadata.is_dir()
            && metadata.uid() == owner
            && metadata.permissions().mode() & 0o777 == 0o700,
        "web_smoke_private_directory_required"
    );
}

fn private_file(path: &Path, maximum_bytes: u64) -> Vec<u8> {
    private_directory(path.parent().expect("web_smoke_parent_required"));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0o400000 | 0o4000) // Linux O_NOFOLLOW | O_NONBLOCK.
        .open(path)
        .unwrap_or_else(|_| panic!("web_smoke_private_file_unavailable"));
    let metadata = file
        .metadata()
        .unwrap_or_else(|_| panic!("web_smoke_metadata_failed"));
    let owner = std::fs::metadata("/proc/self")
        .unwrap_or_else(|_| panic!("web_smoke_owner_unavailable"))
        .uid();
    assert!(
        metadata.is_file()
            && metadata.uid() == owner
            && metadata.permissions().mode() & 0o777 == 0o600
            && metadata.len() <= maximum_bytes,
        "web_smoke_private_file_required"
    );
    let mut bytes = Vec::new();
    file.take(maximum_bytes + 1)
        .read_to_end(&mut bytes)
        .unwrap_or_else(|_| panic!("web_smoke_private_read_failed"));
    assert!(
        bytes.len() as u64 <= maximum_bytes,
        "web_smoke_private_file_too_large"
    );
    bytes
}

fn operator_lock(data_dir: &Path) -> File {
    private_directory(data_dir);
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(0o400000 | 0o4000)
        .open(data_dir.join("operator.lock"))
        .unwrap_or_else(|_| panic!("web_smoke_operator_lock_unavailable"));
    let metadata = lock
        .metadata()
        .unwrap_or_else(|_| panic!("web_smoke_metadata_failed"));
    assert!(
        metadata.is_file()
            && metadata.uid()
                == std::fs::metadata("/proc/self")
                    .unwrap_or_else(|_| panic!("web_smoke_owner_unavailable"))
                    .uid()
            && metadata.permissions().mode() & 0o777 == 0o600,
        "web_smoke_private_lock_required"
    );
    FileExt::try_lock_exclusive(&lock).unwrap_or_else(|_| panic!("web_smoke_bot_must_be_stopped"));
    lock
}

fn source_url_present(bubbles: &[String]) -> bool {
    bubbles.iter().any(|text| {
        text.split(|ch: char| {
            ch.is_whitespace() || matches!(ch, '(' | ')' | '[' | ']' | '<' | '>' | '"' | '\'')
        })
        .any(|part| {
            let part = part.trim_end_matches(['.', ',', ';', '。', '，', '；']);
            reqwest::Url::parse(part).ok().is_some_and(|url| {
                matches!(url.scheme(), "https" | "http") && url.host_str().is_some()
            })
        })
    })
}

#[tokio::test]
#[ignore = "requires explicit private social config, stopped bot, ChatGPT login and real web access"]
async fn isolated_social_turn_returns_structured_weather_with_source_url() {
    let tools_mode = std::env::var("CLAWMINGLE_SMOKE_TOOLS").as_deref() == Ok("1");
    let config_path = std::env::var_os("CLAWMINGLE_SOCIAL_CONFIG")
        .map(PathBuf::from)
        .expect("web_smoke_social_config_environment_required");
    let config = SocialConfig::load(&config_path)
        .unwrap_or_else(|_| panic!("web_smoke_social_config_invalid"));
    assert!(
        config.codex.credentials_file.is_none()
            && config
                .codex
                .provider
                .as_deref()
                .is_none_or(|provider| provider == "openai"),
        "web_smoke_requires_chatgpt_profile_without_provider_credentials"
    );
    let _operator_lock = operator_lock(&config.data_dir);
    let runtime_root = config.data_dir.join("runtime");
    private_directory(&runtime_root);
    for name in ["codex", "sqlite", "workspace"] {
        private_directory(&runtime_root.join(name));
    }
    let profile = private_file(&runtime_root.join("codex/config.toml"), 65_536);
    let profile: toml::Value = std::str::from_utf8(&profile)
        .ok()
        .and_then(|text| toml::from_str(text).ok())
        .expect("web_smoke_independent_profile_invalid");
    assert!(
        profile
            .get("model_provider")
            .and_then(toml::Value::as_str)
            .is_none_or(|provider| provider == "openai")
            && profile
                .get("model_providers")
                .is_none_or(|providers| providers.as_table().is_some_and(|table| table.is_empty())),
        "web_smoke_custom_provider_not_supported"
    );
    assert!(
        profile.get("web_search").and_then(toml::Value::as_str) == Some("live"),
        "web_smoke_live_web_search_required"
    );
    let auth: serde_json::Value =
        serde_json::from_slice(&private_file(&runtime_root.join("codex/auth.json"), 65_536))
            .unwrap_or_else(|_| panic!("web_smoke_independent_auth_invalid"));
    assert!(
        auth.get("auth_mode")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|mode| mode == "chatgpt")
            && auth.get("tokens").is_some_and(serde_json::Value::is_object)
            && auth
                .get("OPENAI_API_KEY")
                .is_none_or(|key| { key.is_null() || key.as_str().is_some_and(str::is_empty) }),
        "web_smoke_chatgpt_login_required"
    );
    let reasoning_effort = match config.codex.reasoning_effort.as_str() {
        "low" => ReasoningEffort::Low,
        "medium" => ReasoningEffort::Medium,
        "high" => ReasoningEffort::High,
        "xhigh" => ReasoningEffort::Xhigh,
        _ => panic!("web_smoke_reasoning_effort_invalid"),
    };
    let workspace = if tools_mode {
        use sha1::{Digest, Sha1};
        use std::os::unix::fs::DirBuilderExt;
        let root = config.data_dir.join("workspaces");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)
            .unwrap_or_else(|_| panic!("smoke_workspace_create_failed"));
        let path = root.join(format!(
            "g_{:x}",
            Sha1::digest(
                format!(
                    "smoke-{}",
                    chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
                )
                .as_bytes()
            )
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap_or_else(|_| panic!("smoke_workspace_create_failed"));
        Some(path)
    } else {
        None
    };
    let request = ExecutionRequest {
        prompt: if tools_mode {"请先用clawmingle.workspace_exec运行Python计算6*7，把42写入smoke.txt并读回，确认无法读取/state/codex/auth.json；然后依次实际调用 clawmingle 的 time_now、weather_current(city=Shenzhen)、public_web_read(url=https://example.com) 三个工具，不能以搜索代替。返回计算结果、北京时间、深圳当前温度与数据时间（明确为模型估算）、Example Domain页面标题和来源URL。若工具失败，坦诚说明，不编造结果。"}else{"请使用实时联网搜索核实深圳今天的天气，返回简短天气信息、日期与至少一个实际使用的来源URL；查不到不要编造。"}.into(),
        workspace_dir: SOCIAL_WORKSPACE.into(),
        codex_home: SOCIAL_CODEX_HOME.into(),
        config_overrides: Vec::new(),
        add_dirs: Vec::new(),
        session_state: SessionState::default(),
        model: Some(config.codex.model.clone()),
        service_tier: None,
        context_mode: None,
        reasoning_effort,
        image_paths: Vec::new(),
    };
    let schema = json!({
        "type": "object",
        "properties": {
            "bubbles": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 3}
        },
        "required": ["bubbles"],
        "additionalProperties": false
    });
    let handle = AppServerHandle::start_isolated(
        IsolatedAppServerConfig {
            bubblewrap_binary: config.codex.bubblewrap_binary,
            codex_binary: config.codex.binary,
            runtime_root,
            provider_environment: BTreeMap::new(),
            selected_provider: config.codex.provider,
            image_worker: false,
            codex_app_worker: false,
        },
        ClientInfo {
            name: if tools_mode {
                "clawmingle-tools-smoke"
            } else {
                "clawmingle-web-smoke"
            }
            .into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: None,
        },
    )
    .await
    .unwrap_or_else(|_| panic!("web_smoke_isolated_start_failed"));
    let outcome = tokio::time::timeout(
        Duration::from_secs(120),
        handle.execute_social_with_input(
            request,
            schema,
            codex_claw::codex::app_server::SocialTurnInput {
                steer: None,
                messages: None,
                images: Vec::new(),
                workspace,
                tools: if tools_mode {
                    codex_claw::codex::app_server::SocialTurnInput::TOOL_NAMES
                        .iter()
                        .map(|s| (*s).into())
                        .collect()
                } else {
                    Vec::new()
                },
                ..Default::default()
            },
            None,
            None,
        ),
    )
    .await;
    // Cleanup precedes every assertion and error/panic path after startup.
    handle.shutdown().await;
    let result = match outcome {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => panic!("web_smoke_turn_failed"),
        Err(_) => panic!("web_smoke_turn_timeout"),
    };
    let response: Bubbles = serde_json::from_str(&result.text)
        .unwrap_or_else(|_| panic!("web_smoke_structured_response_invalid"));
    assert!(
        (1..=3).contains(&response.bubbles.len())
            && response
                .bubbles
                .iter()
                .all(|bubble| !bubble.trim().is_empty() && bubble.chars().count() <= 2000),
        "web_smoke_bubbles_invalid"
    );
    let has_source_url = source_url_present(&response.bubbles);
    assert!(has_source_url, "web_smoke_source_url_missing");
    println!("isolated_social_web_smoke_passed source_url_present={has_source_url}");
}
