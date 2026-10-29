//! Short-lived model picker discovery using only the bot's isolated profile.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::codex::app_server::AvailableModel;
use crate::codex::{AppServerHandle, ClientInfo, IsolatedAppServerConfig};

use super::{
    config::{SocialConfig, read_environment},
    store::{acquire_lock, private_directory},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AvailableModels {
    pub models: Vec<AvailableModel>,
}

/// The operator lock is held through startup, query and shutdown. Calling this
/// while the bot is running fails instead of sharing its profile or process.
/// This creates no QQ connection, thread, turn or model completion request.
pub async fn load_available_models(config: &SocialConfig) -> Result<AvailableModels> {
    config.validate()?;
    let _lock = acquire_lock(&config.data_dir)?;
    let handle = start_discovery(config).await?;
    // Keep cleanup outside the result's `?` path: both successful queries and
    // every timeout/protocol failure shut down the temporary isolated child.
    let result = handle.available_models().await;
    handle.shutdown().await;
    result.map(|models| AvailableModels { models })
}

/// Explicit administrator discovery of transferable connected-app metadata.
/// The worker never starts a model turn or invokes an account-data tool.
pub async fn capability_catalog(config: &SocialConfig) -> Result<Value> {
    config.validate()?;
    let _lock = acquire_lock(&config.data_dir)?;
    let handle = start_discovery(config).await?;
    let result =
        crate::codex::app_server::scoped_integrations::hosted_catalog(&handle.supervisor).await;
    handle.shutdown().await;
    result
}

/// Read-only management probe. Checks the configured adapter under the operator
/// lock and emits only a tool count; schemas and upstream errors stay private.
pub async fn check_integrations(config: &SocialConfig, plugin_ids: &[String]) -> Result<Value> {
    use crate::codex::app_server::scoped_integrations::{IntegrationGrant, bind};
    config.validate()?;
    let _lock = acquire_lock(&config.data_dir)?;
    anyhow::ensure!(
        !plugin_ids.is_empty()
            && plugin_ids.len() <= 16
            && plugin_ids
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == plugin_ids.len(),
        "integration_check_arguments_required"
    );
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(45);
    let mut integrations = config.integrations.clone();
    let mut hosted = false;
    for plugin_id in plugin_ids {
        let plugin = integrations
            .plugins
            .get_mut(plugin_id)
            .ok_or_else(|| anyhow::anyhow!("integration_not_registered"))?;
        // Testing a configured adapter does not enable it in any conversation.
        plugin.enabled = true;
        hosted |= plugin.codex_app;
    }
    let grant = IntegrationGrant {
        plugins: plugin_ids.to_vec(),
        ..Default::default()
    };
    let scope = bind(&integrations, &grant)?;
    let handle = if hosted {
        Some(start_discovery(config).await?)
    } else {
        None
    };
    let scope = if let Some(handle) = &handle {
        scope.with_codex_authority(handle.supervisor.clone())
    } else {
        scope
    };
    let (_cancel, cancellation) = tokio::sync::watch::channel(false);
    let result = tokio::time::timeout_at(deadline, async {
        let mut count = 0usize;
        for plugin_id in plugin_ids {
            let result = scope
                .call(
                    "plugin_tools",
                    &json!({"plugin":plugin_id}).to_string(),
                    cancellation.clone(),
                )
                .await;
            anyhow::ensure!(result["ok"] == true, "integration_check_failed");
            let tools = result
                .get("tools")
                .and_then(Value::as_array)
                .filter(|tools| !tools.is_empty())
                .ok_or_else(|| anyhow::anyhow!("integration_tools_unavailable"))?;
            count += tools.len();
        }
        Ok(json!({"ok":true,"tool_count":count}))
    })
    .await
    .unwrap_or_else(|_| Err(anyhow::anyhow!("integration_check_timeout")));
    if let Some(handle) = handle {
        handle.shutdown().await;
    }
    result
}

async fn start_discovery(config: &SocialConfig) -> Result<AppServerHandle> {
    let mut config = config.clone();
    super::native_runtime::prepare_profile(&mut config)?;
    let runtime_root = config.data_dir.join("runtime");
    private_directory(&runtime_root)?;
    for name in ["codex", "sqlite", "workspace"] {
        private_directory(&runtime_root.join(name))?;
    }
    let provider_environment = match &config.codex.credentials_file {
        Some(path) => read_environment(path)?
            .into_iter()
            .map(|(name, value)| (name, value.into()))
            .collect(),
        None => BTreeMap::new(),
    };
    AppServerHandle::start_isolated(
        IsolatedAppServerConfig {
            bubblewrap_binary: config.codex.bubblewrap_binary.clone(),
            codex_binary: config.codex.binary.clone(),
            runtime_root,
            provider_environment,
            selected_provider: config.codex.provider.clone(),
            image_worker: false,
            codex_app_worker: false,
        },
        ClientInfo {
            name: "clawmingle-models".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: Some("ClawMingle model picker".into()),
        },
    )
    .await
    .map_err(|_| anyhow::anyhow!("isolated_model_discovery_start_failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn running_operator_lock_rejects_discovery_before_starting_or_reading_credentials() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        private_directory(directory.path()).unwrap();
        let config = SocialConfig {
            data_dir: directory.path().to_owned(),
            allowed_groups: vec!["test_group".into()],
            ..SocialConfig::default()
        };
        let _running = acquire_lock(&config.data_dir).unwrap();
        let error = load_available_models(&config).await.unwrap_err();
        assert_eq!(error.to_string(), "social_runtime_already_running");
        let error = check_integrations(&config, &["tinyfish".into()])
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "social_runtime_already_running");
        assert!(!directory.path().join("runtime").exists());
    }

    #[tokio::test]
    async fn integration_checks_reject_unknown_and_duplicate_ids_before_startup() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = SocialConfig {
            data_dir: directory.path().to_owned(),
            allowed_groups: vec!["test_group".into()],
            ..SocialConfig::default()
        };
        let error = check_integrations(&config, &["unknown".into()])
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "integration_not_registered");
        let error = check_integrations(&config, &["tinyfish".into(), "tinyfish".into()])
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "integration_check_arguments_required");
        assert!(!directory.path().join("runtime").exists());
    }

    #[tokio::test]
    #[ignore = "explicit private CLAWMINGLE_SOCIAL_CONFIG and stopped bot required; no model turns"]
    async fn hosted_tinyfish_fetch_reuses_independent_login_without_model_turn() {
        use crate::codex::app_server::scoped_integrations::{IntegrationGrant, bind};
        let path = std::env::var_os("CLAWMINGLE_SOCIAL_CONFIG")
            .expect("explicit private smoke config required");
        let config = SocialConfig::load(std::path::Path::new(&path))
            .unwrap_or_else(|_| panic!("private smoke config load failed"));
        let _lock = acquire_lock(&config.data_dir)
            .unwrap_or_else(|_| panic!("stopped bot and exclusive operator lock required"));
        let mut integrations = config.integrations.clone();
        let plugin = integrations
            .plugins
            .get_mut("tinyfish")
            .expect("TinyFish registration required");
        assert!(plugin.codex_app, "hosted TinyFish registration required");
        plugin.enabled = true;
        let grant = IntegrationGrant {
            plugins: vec!["tinyfish".into()],
            ..Default::default()
        };
        let scope = bind(&integrations, &grant).unwrap();
        let handle = start_discovery(&config)
            .await
            .unwrap_or_else(|_| panic!("isolated smoke authority start failed"));
        let scope = scope.with_codex_authority(handle.supervisor.clone());
        let (_cancel, cancellation) = tokio::sync::watch::channel(false);
        let result = scope.call("plugin_call", &json!({
            "plugin":"tinyfish", "tool":"fetch_content", "arguments":{
                "urls":["https://example.com"], "format":"markdown", "links":false,
                "image_links":false,"iframe_links":false,"page_metadata":false,"per_url_timeout_ms":8000
            }
        }).to_string(), cancellation).await;
        handle.shutdown().await;
        assert_eq!(result["ok"], true, "hosted TinyFish read-only fetch failed");
        assert!(result["content"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|v| v.as_str().is_some_and(|s| s.contains("Example Domain")))
        }));
    }

    #[test]
    fn management_output_has_the_documented_envelope_and_field_names() {
        let result = AvailableModels {
            models: vec![AvailableModel {
                id: "example-model".into(),
                model: "example-model".into(),
                display_name: "Example".into(),
                description: "Picker entry".into(),
                reasoning_efforts: vec!["none".into(), "future_effort".into()],
                default_effort: "future_effort".into(),
            }],
        };
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 1);
        assert_eq!(
            value["models"][0]["reasoning_efforts"],
            serde_json::json!(["none", "future_effort"])
        );
        assert_eq!(value["models"][0]["default_effort"], "future_effort");
    }
}
