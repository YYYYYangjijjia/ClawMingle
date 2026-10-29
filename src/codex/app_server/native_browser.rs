//! Environment plumbing for the existing, unmodified Playwright MCP server.
//!
//! The executor must run in a fresh filesystem and network namespace. This
//! adapter mounts no profile or credentials and exposes only a bounded Unix
//! socket for public TCP connections. All browser tools and media results remain
//! native MCP; there is no ClawMingle browser-operation protocol.
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Result, ensure};
use serde_json::{Value, json};
use tokio::{net::UnixListener, process::Command, sync::watch, task::JoinHandle};

use super::{scoped_integrations::BrowserRuntime, workspace_runtime::validated_native_binary};

const RUNTIME: &str = "/runtime/browser";
const SOCKET: &str = "/run/clawmingle-browser.sock";
const HOOK: &str = include_str!("../../../scripts/clawmingle_browser_network.cjs");

mod proxy;

/// One executor owns one bridge. Keep this alive until that executor has been
/// killed and reaped; then call `shutdown` to close outstanding resource work.
pub(crate) struct NativeBrowser {
    directory: tempfile::TempDir,
    node: PathBuf,
    core: PathBuf,
    chromium: PathBuf,
    cancel: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl NativeBrowser {
    pub(crate) fn prepare(runtime: &BrowserRuntime) -> Result<Self> {
        let node = validated_native_binary(&runtime.node)?;
        let chromium = validated_native_binary(&runtime.chromium)?;
        ensure!(
            chromium.file_name().and_then(|s| s.to_str()) == Some("chrome"),
            "browser_chromium_invalid"
        );
        let core = trusted_directory(&runtime.node_modules.join("playwright-core"))?;
        ensure!(
            core.join("cli.js").is_file() && core.join("lib/coreBundle.js").is_file(),
            "native_browser_mcp_runtime_missing"
        );
        let directory = tempfile::Builder::new()
            .prefix("clawmingle-native-browser-")
            .tempdir()?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        fs::write(directory.path().join("network.cjs"), HOOK)?;
        fs::write(
            directory.path().join("config.json"),
            serde_json::to_vec(&browser_config())?,
        )?;
        for name in ["network.cjs", "config.json"] {
            fs::set_permissions(
                directory.path().join(name),
                fs::Permissions::from_mode(0o600),
            )?;
        }
        let listener = UnixListener::bind(directory.path().join("network.sock"))?;
        fs::set_permissions(
            directory.path().join("network.sock"),
            fs::Permissions::from_mode(0o600),
        )?;
        let (cancel, receiver) = watch::channel(false);
        let policy = proxy::Policy::new()?;
        let task = tokio::spawn(proxy::serve(listener, receiver, policy));
        Ok(Self {
            directory,
            node,
            core,
            chromium,
            cancel,
            task: Some(task),
        })
    }

    /// Append before any writable mounts. The caller supplies `/usr` and shared
    /// libraries, `/proc`, `/dev`, private `/tmp`, and a writable `/workspace`.
    /// This does not enable networking or start a browser.
    pub(crate) fn append_mounts(&self, command: &mut Command) {
        for path in [
            "/runtime",
            RUNTIME,
            "/runtime/browser/node_modules",
            "/runtime/browser/node_modules/playwright-core",
            "/runtime/browser/chromium",
            "/run",
        ] {
            command.args(["--dir", path]);
        }
        for (source, target) in [
            (self.node.as_path(), "/runtime/browser/node"),
            (
                self.core.as_path(),
                "/runtime/browser/node_modules/playwright-core",
            ),
            (
                self.chromium.parent().expect("validated absolute binary"),
                "/runtime/browser/chromium",
            ),
            (
                &self.directory.path().join("network.cjs"),
                "/runtime/browser/network.cjs",
            ),
            (
                &self.directory.path().join("config.json"),
                "/runtime/browser/config.json",
            ),
            (&self.directory.path().join("network.sock"), SOCKET),
        ] {
            command.arg("--ro-bind").arg(source).arg(target);
        }
        for path in ["/etc/fonts", "/etc/ssl/certs"] {
            if Path::new(path).is_dir() {
                command.args(["--ro-bind", path, path]);
            }
        }
    }

    /// Value for the trusted thread's `mcp_servers.playwright` setting. An
    /// explicit environment ID makes native Codex launch stdio on the executor.
    pub(crate) fn mcp_config(&self, environment_id: &str) -> Value {
        json!({
            "enabled":true,
            "command":"/runtime/browser/node",
            "args":["/runtime/browser/network.cjs"],
            "cwd":"/workspace", "environment_id":environment_id,
            "startup_timeout_sec":30,"tool_timeout_sec":90,
        })
    }

    pub(crate) fn cancel(&self) {
        let _ = self.cancel.send(true);
    }

    pub(crate) async fn shutdown(mut self) {
        self.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for NativeBrowser {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn browser_config() -> Value {
    json!({
        "browser":{
            "browserName":"chromium", "isolated":true,
            "launchOptions":{
                "headless":true,"executablePath":"/runtime/browser/chromium/chrome",
                // The outer executor namespace is the security boundary; its
                // process has no network and no personal filesystem mounts.
                "chromiumSandbox":false,
                "downloadsPath":"/workspace/.browser/downloads",
                "args":["--disable-dev-shm-usage","--disable-background-networking","--disable-breakpad","--disable-extensions","--no-pings","--renderer-process-limit=2"],
            },
            "contextOptions":{"viewport":{"width":1280,"height":720},"serviceWorkers":"allow","acceptDownloads":true,"permissions":[],"locale":"zh-CN","timezoneId":"Asia/Shanghai"},
        },
        "capabilities":["vision","pdf"],"imageResponses":"allow",
        // The shared workspace budget owns disk limits. Do not independently
        // evict a useful download based on a smaller browser artifact budget.
        "outputDir":"/workspace/.browser","outputMaxSize":0,
        "timeouts":{"action":5000,"navigation":30000,"settle":500},
    })
}

fn trusted_directory(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "browser_runtime_path_invalid");
    let resolved = path.canonicalize()?;
    let uid = fs::metadata("/proc/self")?.uid();
    for ancestor in resolved.ancestors() {
        let meta = fs::symlink_metadata(ancestor)?;
        ensure!(
            !meta.file_type().is_symlink()
                && (meta.uid() == 0 || meta.uid() == uid)
                && meta.permissions().mode() & 0o022 == 0,
            "browser_runtime_untrusted"
        );
    }
    ensure!(resolved.is_dir(), "browser_runtime_type_invalid");
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_browser_keeps_general_browser_features_and_no_personal_profile() {
        let config = browser_config();
        assert_eq!(config["browser"]["isolated"], true);
        assert_eq!(
            config["browser"]["contextOptions"]["serviceWorkers"],
            "allow"
        );
        assert_eq!(config["browser"]["contextOptions"]["acceptDownloads"], true);
        assert_eq!(
            config["browser"]["launchOptions"]["downloadsPath"],
            "/workspace/.browser/downloads"
        );
        assert!(config["browser"].get("initPage").is_none());
        assert!(config["browser"].get("userDataDir").is_none());
        assert!(
            config["browser"]["contextOptions"]
                .get("storageState")
                .is_none()
        );
        assert!(!config.to_string().contains("remote-debugging-port"));
    }
}
