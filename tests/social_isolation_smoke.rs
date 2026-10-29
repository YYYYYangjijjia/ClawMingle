//! Manual initialization only: no login, thread, turn, QQ, or model request.

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires a local native Codex binary and working unprivileged bubblewrap"]
async fn isolated_app_server_initializes_without_personal_profile_or_credentials() {
    use codex_claw::codex::{AppServerHandle, ClientInfo, IsolatedAppServerConfig};
    use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["codex", "sqlite", "workspace"] {
        let path = root.path().join(name);
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let configuration = root.path().join("codex/config.toml");
    std::fs::write(&configuration,"model_provider='local_probe'\n[model_providers.local_probe]\nname='Local initialization probe'\nbase_url='http://127.0.0.1:1/v1'\nwire_api='responses'\n").unwrap();
    std::fs::set_permissions(configuration, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = IsolatedAppServerConfig {
        bubblewrap_binary: PathBuf::from("/usr/bin/bwrap"),
        codex_binary: PathBuf::from("/opt/codex/current/bin/codex"),
        runtime_root: root.path().to_owned(),
        provider_environment: BTreeMap::new(),
        selected_provider: None,
        image_worker: false,
        codex_app_worker: false,
    };
    let handle = tokio::time::timeout(
        Duration::from_secs(20),
        AppServerHandle::start_isolated(
            config,
            ClientInfo {
                name: "clawmingle-isolation-probe".into(),
                version: "0.1.0".into(),
                title: None,
            },
        ),
    )
    .await
    .expect("initialization deadline")
    .expect("isolated initialize");
    handle.shutdown().await;
    assert!(!root.path().join("codex/auth.json").exists());
    assert!(
        std::fs::read_dir(root.path().join("workspace"))
            .unwrap()
            .next()
            .is_none()
    );
}
