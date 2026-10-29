//! Host-owned binding of native Codex tools to one conversation environment.
//! All task tools are supplied by Codex / the official browser MCP. This module
//! only controls authority, lifecycle and routing; it never executes model code.
use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use super::{
    AppServerSupervisor, SocialTurnInput, client::JsonRpcClient, native_browser::NativeBrowser,
    native_environment::NativeEnvironmentGuard,
};

pub(super) struct NativeSession {
    environment: Option<NativeEnvironmentGuard>,
    worker: Option<(Arc<AppServerSupervisor>, Arc<JsonRpcClient>)>,
    browser: Option<NativeBrowser>,
    hooks: Option<Arc<super::isolation::HookMounts>>,
    hook_state: Option<Value>,
    agent_context: Option<super::native_agent_context::AgentContext>,
}

impl NativeSession {
    pub(super) async fn start(
        supervisor: &Arc<AppServerSupervisor>,
        client: &Arc<JsonRpcClient>,
        input: &mut SocialTurnInput,
    ) -> Result<Option<Self>> {
        // workspace_exec remains the established conversation execution grant.
        // Merely selecting a browser or a Skill cannot grant arbitrary execution.
        if !input.tools.iter().any(|name| name == "workspace_exec") {
            return Ok(None);
        }
        let config = supervisor
            .isolation_config()
            .context("native_environment_requires_isolation")?;
        ensure!(
            !config.image_worker && !config.codex_app_worker,
            "native_environment_worker_forbidden"
        );
        let workspace = input
            .workspace
            .as_deref()
            .context("native_environment_workspace_required")?;
        supervisor.validate_tool_workspace(workspace)?;
        let skills = supervisor.validate_tool_skills(&input.skill_packages)?;
        let browser = match input
            .integrations
            .as_ref()
            .and_then(|scope| scope.native_browser_runtime())
        {
            Some(runtime) => Some(NativeBrowser::prepare(runtime)?),
            None => None,
        };
        let node = input
            .integrations
            .as_ref()
            .and_then(|scope| scope.native_node_runtime())
            .map(super::workspace_runtime::validated_native_binary)
            .transpose()?;
        let hooks = supervisor.task_hooks();
        let agent_context = super::native_agent_context::AgentContext::prepare(&input.images)?;
        let environment = NativeEnvironmentGuard::start(&config, workspace, &skills, |cmd| {
            agent_context.append_mounts(cmd);
            if let Some(node) = &node {
                cmd.arg("--ro-bind").arg(node).arg("/runtime/node");
            }
            if let Some(browser) = &browser {
                browser.append_mounts(cmd);
            }
            if let Some(hooks) = &hooks {
                cmd.arg("--ro-bind")
                    .arg(&hooks.input)
                    .arg(super::native_hooks::HOOK_INPUT_BINARY);
            }
            Ok(())
        })
        .await?;
        environment.register(client).await?;
        let hook_state = if let Some(hooks) = &hooks {
            hooks.prepared.attach_executor(environment.host_rpc())?;
            let catalog: Value = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                client.request("hooks/list", &json!({"cwds":["/workspace"]})),
            )
            .await
            .context("native_hook_catalog_timeout")??;
            Some(
                hooks
                    .prepared
                    .trust_state(&catalog, "/state/codex/hooks.json")?,
            )
        } else {
            None
        };
        // Do not publish competing wrappers for the native task tools.
        input.tools.retain(|name| {
            name != "workspace_exec" && (browser.is_none() || name != "public_browser")
        });
        Ok(Some(Self {
            environment: Some(environment),
            worker: supervisor
                .is_task_worker()
                .then(|| (supervisor.clone(), client.clone())),
            browser,
            hooks,
            hook_state,
            agent_context: Some(agent_context),
        }))
    }

    pub(super) fn environment(&self) -> Value {
        self.environment
            .as_ref()
            .expect("active environment")
            .environment_params()
    }
    pub(super) fn capability_roots(&self) -> Vec<Value> {
        self.environment
            .as_ref()
            .expect("active environment")
            .selected_capability_roots()
    }

    pub(super) fn failure_receiver(&self) -> tokio::sync::watch::Receiver<Option<String>> {
        self.environment
            .as_ref()
            .expect("active environment")
            .failure_receiver()
    }

    pub(super) fn configure(&self, config: &mut HashMap<String, Value>) {
        config.insert("features.shell_tool".into(), json!(true));
        if self.worker.is_some() {
            config.insert("agents.enabled".into(), json!(true));
            config.insert("features.multi_agent".into(), json!(true));
            // Force V2 even when old history recorded Disabled, and retain V2's
            // tree-local registry checks instead of V1 arbitrary UUID resume.
            config.insert(
                "features.multi_agent_v2".into(),
                self.agent_context
                    .as_ref()
                    .expect("active agent context")
                    .config(&self.capability_roots()),
            );
        }
        config.insert("features.unified_exec".into(), json!(true));
        config.insert("tools.view_image".into(), json!(true));
        // Native shell tool policy otherwise strips even the executor's PATH.
        // Supply only fixed values from the credential-free execution root.
        config.insert(
            "shell_environment_policy.set".into(),
            json!(
                super::native_environment::EXECUTION_ENVIRONMENT
                    .iter()
                    .copied()
                    .collect::<std::collections::BTreeMap<_, _>>()
            ),
        );
        if let Some(state) = &self.hook_state {
            config.insert("features.hooks".into(), json!(true));
            config.insert("hooks.state".into(), state.clone());
        }
        // Personal account applications remain opt-in. No profile is imported.
        if let Some(browser) = &self.browser {
            config.insert(
                "mcp_servers.playwright".into(),
                browser.mcp_config(
                    self.environment
                        .as_ref()
                        .expect("active environment")
                        .environment_id(),
                ),
            );
        }
    }

    /// Revoke outbound browser work immediately while keeping the executor
    /// transport alive for native turn interruption and MCP process shutdown.
    pub(super) fn cancel_browser(&self) {
        if let Some(browser) = &self.browser {
            browser.cancel();
        }
    }

    pub(super) async fn shutdown(&mut self) -> Result<()> {
        if let Some((worker, client)) = &self.worker {
            worker.shutdown().await;
            // A concurrent supervisor shutdown may already have taken its
            // client. Await this exact connection's cleanup before releasing
            // the executor lease, including after a cancelled shutdown await.
            client.shutdown("native task completed").await;
            ensure!(
                client.shutdown_confirmed(),
                "native_task_shutdown_unconfirmed"
            );
        }
        self.worker.take();
        let result = if let Some(mut environment) = self.environment.take() {
            environment.shutdown().await
        } else {
            Ok(())
        };
        if let Some(browser) = self.browser.take() {
            browser.shutdown().await;
        }
        self.agent_context.take();
        result
    }
}

impl Drop for NativeSession {
    fn drop(&mut self) {
        let worker = self.worker.take();
        let environment = self.environment.take();
        let browser = self.browser.take();
        let hooks = self.hooks.take();
        let agent_context = self.agent_context.take();
        if worker.is_none() && environment.is_none() && browser.is_none() {
            return;
        }
        if let Some(browser) = &browser {
            browser.cancel();
        }
        tokio::spawn(async move {
            if let Some((worker, client)) = worker {
                worker.shutdown().await;
                client.shutdown("native task dropped").await;
            }
            if let Some(mut environment) = environment {
                let _ = environment.shutdown().await;
            }
            if let Some(browser) = browser {
                browser.shutdown().await;
            }
            drop(hooks);
            drop(agent_context);
        });
    }
}

/// Reopening a legacy turn must not inherit native tool flags or an old MCP
/// process from a previously enabled environment.
pub(super) fn disable(config: &mut HashMap<String, Value>) {
    config.insert("shell_environment_policy.set".into(), json!({}));
    // The isolated profile forbids configured MCP servers, and switching out
    // of native mode unloads the old thread before rebuilding its config.
    // An enabled=false leaf alone creates an invalid transport-less server.
    config.insert("mcp_servers".into(), json!({}));
    for key in [
        "features.hooks",
        "agents.enabled",
        "features.multi_agent",
        "features.multi_agent_v2",
        "features.shell_tool",
        "features.unified_exec",
        "tools.view_image",
    ] {
        config.insert(key.into(), json!(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabling_native_mode_clears_execution_and_browser_flags() {
        let mut config = HashMap::from([
            ("features.shell_tool".into(), json!(true)),
            ("model".into(), json!("test-model")),
        ]);
        disable(&mut config);
        assert_eq!(config["features.shell_tool"], false);
        assert_eq!(config["features.unified_exec"], false);
        assert_eq!(config["tools.view_image"], false);
        assert_eq!(config["mcp_servers"], json!({}));
        assert!(!config.contains_key("mcp_servers.playwright.enabled"));
        assert_eq!(config["model"], "test-model");
    }
}
