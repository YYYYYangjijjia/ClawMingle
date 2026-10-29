//! Portable preparation of the bot's independent execution runtime.
//!
//! This never reads or copies an operator's Codex profile. Runtime registration
//! and per-conversation tool grants remain separate from installing dependencies.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use tokio::{io::AsyncReadExt, process::Command};

use crate::codex::app_server::{
    scoped_integrations::BrowserRuntime, workspace_runtime::validated_native_binary,
};

use super::{config::SocialConfig, store::private_directory};

const DEFAULT_CODEX: &str = "/opt/codex/current/bin/codex";
const INSTALLER: &str = include_str!("../../scripts/prepare_native_runtime.py");
const WORKER: &str = include_str!("../../scripts/clawmingle_browser_worker.mjs");
const INSTALL_TIMEOUT: Duration = Duration::from_secs(900);
const MAX_INSTALL_OUTPUT: usize = 16 * 1024;

/// Called after acquiring the application lock and before creating app-server.
/// Existing configuration files and explicitly configured browser runtimes are
/// preserved. A disabled browser never triggers an automatic installation.
pub async fn prepare(config: &mut SocialConfig) -> Result<()> {
    prepare_profile(config)?;
    // Clear stale readiness before preparation; this manifest is informational
    // and never grants tools or substitutes for executable validation.
    publish_readiness(config, false)?;
    if !config.integrations.browser.enabled {
        return Ok(());
    }
    if config.integrations.browser.runtime.is_some() {
        return validate_and_publish(config).await;
    }

    // The helper is embedded in the release binary: source checkouts and personal
    // npm installations are not prerequisites for a compiled deployment.
    let runtime_root = config.data_dir.join("runtime");
    let helper_dir = tempfile::Builder::new()
        .prefix(".prepare-tools-")
        .tempdir_in(&runtime_root)?;
    let helper = helper_dir.path().join("prepare_native_runtime.py");
    let worker = helper_dir.path().join("clawmingle_browser_worker.mjs");
    write_private_new(&helper, INSTALLER.as_bytes())?;
    write_private_new(&worker, WORKER.as_bytes())?;
    let python = find_python()?;
    let mut command = Command::new(python);
    command
        .arg("-I")
        .arg(&helper)
        .arg("--root")
        .arg(runtime_root.join("tools"))
        .arg("--bubblewrap")
        .arg(&config.codex.bubblewrap_binary)
        .arg("--worker")
        .arg(&worker)
        .current_dir(helper_dir.path())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Only transport settings are inherited. HOME, npm/Node hooks, browser
    // profiles and all application/provider credentials are intentionally absent.
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NODE_EXTRA_CA_CERTS",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value).arg("--inherit-env").arg(name);
        }
    }
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .context("cannot start Python 3 runtime bootstrap")?;
    let process_group = BootstrapProcessGroup(child.id());
    let stdout = child
        .stdout
        .take()
        .context("runtime bootstrap stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("runtime bootstrap stderr unavailable")?;
    let execution = async {
        let (status, output, diagnostics) = tokio::try_join!(
            async { child.wait().await.context("runtime bootstrap wait failed") },
            bounded_output(stdout),
            bounded_output(stderr),
        )?;
        ensure!(
            status.success(),
            "native runtime preparation failed: {}",
            String::from_utf8_lossy(&diagnostics).trim()
        );
        let runtime: BrowserRuntime = serde_json::from_slice(&output)
            .context("runtime bootstrap returned invalid registration")?;
        Ok::<_, anyhow::Error>(runtime)
    };
    let runtime = tokio::time::timeout(INSTALL_TIMEOUT, execution)
        .await
        .context("native runtime preparation exceeded 15 minutes")??;
    drop(process_group);
    // The same native-executable policy used at mount time also checks newly
    // created releases before making them available to any conversation.
    validated_native_binary(&runtime.node).context("prepared Node executable is invalid")?;
    validated_native_binary(&runtime.chromium)
        .context("prepared Chromium executable is invalid")?;
    ensure!(
        runtime.bubblewrap == config.codex.bubblewrap_binary,
        "runtime bubblewrap mismatch"
    );
    config.integrations.browser.runtime = Some(runtime);
    validate_and_publish(config).await
}

/// Metadata discovery needs the independent profile, but never downloads a
/// browser or changes the service's prepared-tools readiness.
pub(crate) fn prepare_profile(config: &mut SocialConfig) -> Result<()> {
    ensure!(cfg!(target_os = "linux"), "native runtime requires Linux");
    config.codex.binary = resolve_codex_binary(&config.codex.binary)?;
    config.codex.bubblewrap_binary = validated_native_binary(&config.codex.bubblewrap_binary)
        .context("bubblewrap is required: install the distribution's bubblewrap package")?;
    initialize_independent_home(config)
}

async fn validate_and_publish(config: &SocialConfig) -> Result<()> {
    if let Some(runtime) = &config.integrations.browser.runtime {
        // Uses the same checks as a real turn. No browser process or request is
        // started; immediately close the unused private transport.
        crate::codex::app_server::native_browser::NativeBrowser::prepare(runtime)?
            .shutdown()
            .await;
        publish_readiness(config, true)?;
    }
    Ok(())
}

fn publish_readiness(config: &SocialConfig, browser_ready: bool) -> Result<()> {
    let root = config.data_dir.join("runtime");
    let mut output = tempfile::NamedTempFile::new_in(&root)?;
    serde_json::to_writer(
        &mut output,
        &serde_json::json!({"version":1,"browser_ready":browser_ready}),
    )?;
    output.as_file().sync_all()?;
    output.persist(root.join("native-tools.json"))?;
    Ok(())
}

fn initialize_independent_home(config: &SocialConfig) -> Result<()> {
    let root = config.data_dir.join("runtime");
    private_directory(&root)?;
    for name in ["codex", "sqlite", "workspace"] {
        private_directory(&root.join(name))?;
    }
    let path = root.join("codex/config.toml");
    match fs::symlink_metadata(&path) {
        Ok(_) => return Ok(()), // Isolation validation owns existing-file checks.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect independent Codex config"),
    }
    let mut table = toml::Table::new();
    for (key, value) in [
        ("model", config.codex.model.as_str()),
        (
            "model_reasoning_effort",
            config.codex.reasoning_effort.as_str(),
        ),
        ("web_search", "live"),
        ("cli_auth_credentials_store", "file"),
    ] {
        table.insert(key.into(), toml::Value::String(value.into()));
    }
    if let Some(provider) = &config.codex.provider {
        table.insert(
            "model_provider".into(),
            toml::Value::String(provider.clone()),
        );
    }
    let contents = toml::to_string(&table)?;
    let mut temporary = tempfile::NamedTempFile::new_in(root.join("codex"))?;
    temporary.write_all(contents.as_bytes())?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error).context("cannot create independent Codex config"),
    }
}

fn write_private_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)?;
    Ok(())
}

/// Resolve an installed native Codex executable without running an npm wrapper
/// or importing its profile. Only the historical default may fall back to PATH.
pub fn resolve_codex_binary(configured: &Path) -> Result<PathBuf> {
    if configured.try_exists()? {
        return resolve_candidate(configured).context("configured Codex executable is invalid");
    }
    ensure!(
        configured == Path::new(DEFAULT_CODEX),
        "configured Codex executable does not exist; custom paths never fall back to PATH"
    );
    resolve_codex_from_path(&std::env::var_os("PATH").unwrap_or_default())
}

fn resolve_codex_from_path(search_path: &std::ffi::OsStr) -> Result<PathBuf> {
    for directory in std::env::split_paths(search_path) {
        if directory.is_absolute() {
            let candidate = directory.join("codex");
            if let Ok(binary) = resolve_candidate(&candidate) {
                return Ok(binary);
            }
        }
    }
    bail!("Codex native executable unavailable: install @openai/codex on PATH or set codex.binary")
}

fn resolve_candidate(path: &Path) -> Result<PathBuf> {
    if let Ok(binary) = validated_native_binary(path) {
        return Ok(binary);
    }
    let wrapper = path
        .canonicalize()
        .context("Codex executable unavailable")?;
    for candidate in npm_native_candidates(&wrapper) {
        if let Ok(binary) = validated_native_binary(&candidate) {
            return Ok(binary);
        }
    }
    bail!("Codex must resolve to a trusted native ELF executable or official npm vendor binary")
}

fn npm_native_candidates(wrapper: &Path) -> Vec<PathBuf> {
    let Some(package) = wrapper.parent().and_then(Path::parent) else {
        return Vec::new();
    };
    if wrapper.file_name().and_then(|s| s.to_str()) != Some("codex.js")
        || wrapper
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            != Some("bin")
        || package.file_name().and_then(|s| s.to_str()) != Some("codex")
        || package
            .parent()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            != Some("@openai")
    {
        return Vec::new();
    }
    let (target, platform) = match std::env::consts::ARCH {
        "x86_64" => ("x86_64-unknown-linux-musl", "codex-linux-x64"),
        "aarch64" => ("aarch64-unknown-linux-musl", "codex-linux-arm64"),
        _ => return Vec::new(),
    };
    let vendor = Path::new("vendor").join(target).join("codex/codex");
    vec![
        package.join(&vendor),
        package
            .join("node_modules/@openai")
            .join(platform)
            .join(&vendor),
        package
            .parent()
            .expect("checked npm scope")
            .join(platform)
            .join(vendor),
    ]
}

fn find_python() -> Result<PathBuf> {
    for candidate in [
        Path::new("/usr/bin/python3"),
        Path::new("/usr/local/bin/python3"),
    ] {
        if let Ok(path) = validated_native_binary(candidate) {
            return Ok(path);
        }
    }
    bail!("Python 3 is required for runtime bootstrap: install the distribution's python3 package")
}

async fn bounded_output(mut reader: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut reader)
        .take((MAX_INSTALL_OUTPUT + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(
        bytes.len() <= MAX_INSTALL_OUTPUT,
        "runtime bootstrap output limit exceeded"
    );
    Ok(bytes)
}

/// Killing the complete process group also stops npm and Chromium downloads on
/// startup cancellation, timeout, malformed output, or a dropped prepare future.
struct BootstrapProcessGroup(Option<u32>);
impl Drop for BootstrapProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            unsafe extern "C" {
                fn kill(
                    pid: std::os::raw::c_int,
                    signal: std::os::raw::c_int,
                ) -> std::os::raw::c_int;
            }
            // SAFETY: a positive child PID identifies the private process group
            // established with process_group(0); no borrowed memory is involved.
            unsafe { kill(-(pid as i32), 9) };
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn missing_custom_codex_never_uses_path() {
        assert!(resolve_codex_binary(Path::new("/nonexistent/custom-codex")).is_err());
    }

    #[test]
    fn npm_resolution_only_accepts_the_official_wrapper_layout() {
        assert_eq!(
            npm_native_candidates(Path::new("/opt/npm/@openai/codex/bin/codex.js")).len(),
            3
        );
        for wrapper in [
            "/tmp/codex.js",
            "/opt/npm/other/codex/bin/codex.js",
            "/opt/npm/@openai/codex/bin/other.js",
        ] {
            assert!(npm_native_candidates(Path::new(wrapper)).is_empty());
        }
    }

    #[test]
    fn native_codex_path_search_skips_invalid_and_relative_candidates() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let invalid = root.path().join("invalid");
        let valid = root.path().join("valid");
        fs::create_dir_all(&invalid).unwrap();
        fs::create_dir_all(&valid).unwrap();
        fs::write(invalid.join("codex"), "#!/bin/sh\nexit 99\n").unwrap();
        fs::set_permissions(invalid.join("codex"), fs::Permissions::from_mode(0o700)).unwrap();
        symlink("/usr/bin/true", valid.join("codex")).unwrap();
        let paths = std::env::join_paths([
            Path::new("relative-bin"),
            invalid.as_path(),
            valid.as_path(),
        ])
        .unwrap();
        assert_eq!(
            resolve_codex_from_path(&paths).unwrap(),
            fs::canonicalize("/usr/bin/true").unwrap()
        );
        let invalid_paths =
            std::env::join_paths([Path::new("relative-bin"), invalid.as_path()]).unwrap();
        assert!(resolve_codex_from_path(&invalid_paths).is_err());
        // A configured invalid wrapper is an error, even with a usable native
        // installation available to the discovery helper.
        assert!(resolve_codex_binary(&invalid.join("codex")).is_err());
    }

    #[test]
    fn npm_vendor_layouts_resolve_to_validated_native_without_executing_wrappers() {
        use std::os::unix::fs::symlink;
        for selected in 0..3 {
            let root = tempfile::tempdir().unwrap();
            let wrapper = root.path().join("@openai/codex/bin/codex.js");
            fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
            fs::write(
                &wrapper,
                "throw new Error('fixture wrapper must never execute');",
            )
            .unwrap();
            let candidates = npm_native_candidates(&wrapper);
            assert_eq!(candidates.len(), 3);
            fs::create_dir_all(candidates[selected].parent().unwrap()).unwrap();
            symlink("/usr/bin/true", &candidates[selected]).unwrap();
            assert_eq!(
                resolve_codex_binary(&wrapper).unwrap(),
                fs::canonicalize("/usr/bin/true").unwrap()
            );
        }
    }

    #[test]
    fn config_load_expands_custom_codex_home_path_without_importing_auth() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = SocialConfig {
            data_dir: root.path().join("state"),
            allowed_groups: vec!["fixture-group".into()],
            ..Default::default()
        };
        config.codex.binary = "~/.clawmingle-fixture/native-codex".into();
        let path = root.path().join("social.toml");
        write_private_new(&path, toml::to_string(&config).unwrap().as_bytes()).unwrap();
        let loaded = SocialConfig::load(&path).unwrap();
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(
            loaded.codex.binary,
            home.join(".clawmingle-fixture/native-codex")
        );
        assert!(!root.path().join("state").exists());
        config.codex.binary = "/usr/bin/true".into();
        fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        let loaded = SocialConfig::load(&path).unwrap();
        assert_eq!(
            resolve_codex_binary(&loaded.codex.binary).unwrap(),
            fs::canonicalize("/usr/bin/true").unwrap()
        );
        assert!(!root.path().join("state").exists());
    }

    #[test]
    fn independent_config_is_private_and_existing_configuration_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let config = SocialConfig {
            data_dir: temp.path().to_owned(),
            ..Default::default()
        };
        initialize_independent_home(&config).unwrap();
        let path = temp.path().join("runtime/codex/config.toml");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let first = fs::read_to_string(&path).unwrap();
        assert!(first.contains("cli_auth_credentials_store = \"file\""));
        assert!(first.contains("web_search = \"live\""));
        fs::write(&path, "# existing independent operator configuration\n").unwrap();
        initialize_independent_home(&config).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# existing independent operator configuration\n"
        );
        assert!(!temp.path().join("runtime/codex/auth.json").exists());
    }

    #[tokio::test]
    async fn bootstrap_output_is_bounded() {
        assert!(bounded_output(&b"ok"[..]).await.is_ok());
        let excessive = vec![b'x'; MAX_INSTALL_OUTPUT + 1];
        assert!(bounded_output(excessive.as_slice()).await.is_err());
    }

    #[tokio::test]
    async fn disabled_and_explicit_invalid_browser_runtimes_never_run_the_installer() {
        let temporary = tempfile::tempdir().unwrap();
        let mut config = SocialConfig {
            data_dir: temporary.path().to_owned(),
            ..Default::default()
        };
        // These binaries are validated but never executed on either reuse path.
        config.codex.binary = PathBuf::from("/usr/bin/true");
        config.codex.bubblewrap_binary = PathBuf::from("/usr/bin/true");
        config.integrations.browser.enabled = false;
        prepare(&mut config).await.unwrap();
        assert!(config.integrations.browser.runtime.is_none());
        assert!(!temporary.path().join("runtime/tools").exists());
        let readiness = temporary.path().join("runtime/native-tools.json");
        assert_eq!(
            fs::metadata(&readiness).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(&readiness).unwrap()).unwrap(),
            serde_json::json!({"version":1,"browser_ready":false})
        );

        config.integrations.browser.enabled = true;
        let existing = BrowserRuntime {
            bubblewrap: "/operator/bwrap".into(),
            node: "/operator/node".into(),
            node_modules: "/operator/node_modules".into(),
            chromium: "/operator/chrome".into(),
            worker: "/operator/worker".into(),
        };
        let expected = serde_json::to_value(&existing).unwrap();
        config.integrations.browser.runtime = Some(existing);
        // An invalid explicit runtime fails without installing a replacement or
        // marking readiness true. Preserve the operator's selected paths.
        assert!(prepare(&mut config).await.is_err());
        assert_eq!(
            serde_json::to_value(&config.integrations.browser.runtime).unwrap(),
            expected
        );
        assert!(!temporary.path().join("runtime/tools").exists());
    }
}
