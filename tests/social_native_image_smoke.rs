//! Explicit real native image edit using the bot account, never QQ or Jev.
#![cfg(target_os = "linux")]

use base64::{Engine as _, engine::general_purpose::STANDARD};
use codex_claw::{
    codex::{AppServerHandle, ClientInfo, IsolatedAppServerConfig, app_server::NativeImageRequest},
    session::state::ReasoningEffort,
    social::SocialConfig,
};
use fs2::FileExt;
use std::{
    collections::BTreeMap, fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf,
    time::Duration,
};

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAIAAAD8GO2jAAAAJklEQVR4nO3NsQkAAAjAsP7/tF7hIASyp6ZbAoFAIBAIBAKB4EuwNof8LkGrxSIAAAAASUVORK5CYII=";

#[tokio::test]
#[ignore = "requires explicitly stopped bot, independent account and a real native image edit"]
async fn native_account_edits_reference_pixels_in_a_fresh_isolated_worker() {
    let config = SocialConfig::load(&PathBuf::from(
        std::env::var_os("CLAWMINGLE_SOCIAL_CONFIG").expect("native_smoke_config_required"),
    ))
    .unwrap_or_else(|_| panic!("native_smoke_config_invalid"));
    assert!(
        config.codex.credentials_file.is_none()
            && config
                .codex
                .provider
                .as_deref()
                .is_none_or(|p| p == "openai")
    );
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(0o400000 | 0o4000)
        .open(config.data_dir.join("operator.lock"))
        .unwrap();
    lock.try_lock_exclusive()
        .expect("native_smoke_bot_must_be_stopped");
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
            name: "clawmingle-native-image-smoke".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: None,
        },
    )
    .await
    .unwrap_or_else(|_| panic!("native_smoke_start_failed"));
    let result=handle.generate_social_image(NativeImageRequest {prompt:"将参考图中的绿色区域改为纯橙色，保持正方形、纯色背景，不添加文字、图案或其他物体。生成一张适合聊天发送的小尺寸图片。".into(),references:vec![STANDARD.decode(PNG).unwrap()],model:config.codex.model,reasoning_effort:ReasoningEffort::High,timeout:Duration::from_secs(240)},None).await;
    handle.shutdown().await;
    let image = result.unwrap_or_else(|error| panic!("native_smoke_failed: {error}"));
    assert!(
        image.png.starts_with(b"\x89PNG\r\n\x1a\n")
            && image.png.len() > 100
            && image.png.len() <= codex_claw::codex::MAX_GENERATED_IMAGE_BYTES
    );
    if let Some(output) = std::env::var_os("CLAWMINGLE_IMAGE_SMOKE_OUTPUT") {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(0o400000)
            .open(PathBuf::from(output))
            .unwrap();
        file.write_all(&image.png).unwrap();
    }
    println!("Native Codex-account image edit returned one bounded PNG; worker shut down.");
}
