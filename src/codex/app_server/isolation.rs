//! A deliberately small filesystem view for the social bot's Codex process.
//!
//! The configuration and credentials belong to this bot installation. Nothing
//! is copied from the operator's Codex home, shell profile, or environment.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use tokio::process::Command;

use crate::codex::types::ExecutionRequest;

pub const SOCIAL_WORKSPACE: &str = "/workspace";
pub const SOCIAL_CODEX_HOME: &str = "/state/codex";
pub(crate) const SOCIAL_SQLITE_HOME: &str = "/state/sqlite";

/// Pre-existing, private runtime directories, separate from the QQ/Jev store.
///
/// `runtime_root` contains `codex/config.toml`, optional `codex/auth.json`,
/// `sqlite/`, and an empty `workspace/`. Directories must be owned by the
/// current non-root user and mode 0700; config/auth files must be mode 0600.
/// Use a native Linux Codex binary, rather than a shell or npm wrapper.
///
/// Only explicitly supplied provider credentials are forwarded. Their names
/// must be declared by `env_key`/`env_http_headers` in the independent config
/// (or be `OPENAI_API_KEY`). This type intentionally does not implement Debug.
#[derive(Clone)]
pub struct IsolatedAppServerConfig {
    pub bubblewrap_binary: PathBuf,
    pub codex_binary: PathBuf,
    pub runtime_root: PathBuf,
    pub provider_environment: BTreeMap<String, OsString>,
    pub selected_provider: Option<String>,
    /// Access-token-only, one-shot image worker with no shared profile state.
    pub image_worker: bool,
    /// Access-only, no-model worker for the host's exact TinyFish tool bridge.
    pub codex_app_worker: bool,
}

pub(crate) struct HookMounts {
    pub(crate) prepared: Arc<super::native_hooks::PreparedHooks>,
    pub(crate) proxy: PathBuf,
    pub(crate) input: PathBuf,
}

impl HookMounts {
    pub(crate) async fn prepare(config: &IsolatedAppServerConfig) -> Result<Option<Arc<Self>>> {
        let Some(source) = config.hook_source()? else {
            return Ok(None);
        };
        let (proxy, input) = hook_binaries()?;
        let prepared = Arc::new(super::native_hooks::PreparedHooks::prepare(&source).await?);
        Ok(Some(Arc::new(Self {
            prepared,
            proxy,
            input,
        })))
    }
}

fn hook_binaries() -> Result<(PathBuf, PathBuf)> {
    let executable = std::env::current_exe().context("native_hook_runtime_unavailable")?;
    let mut directory = executable
        .parent()
        .context("native_hook_runtime_unavailable")?;
    // Cargo integration tests live in target/{profile}/deps; installed binaries
    // have their two fixed, trusted helpers alongside the main executable.
    if directory.file_name().is_some_and(|name| name == "deps") {
        directory = directory
            .parent()
            .context("native_hook_runtime_unavailable")?;
    }
    Ok((
        validated_binary(&directory.join("clawmingle-hook-proxy"))
            .context("native_hook_proxy_missing_build_all_bins")?,
        validated_binary(&directory.join("clawmingle-hook-input"))
            .context("native_hook_input_missing_build_all_bins")?,
    ))
}

impl IsolatedAppServerConfig {
    /// Validate without starting Codex or contacting a model/provider.
    pub fn validate(&self) -> Result<()> {
        self.validate_config(self.selected_provider.as_deref())
            .map(|_| ())
    }

    pub(crate) fn validate_request(&self, request: &ExecutionRequest) -> Result<()> {
        ensure!(!self.codex_app_worker, "hosted_worker_model_turn_forbidden");
        ensure!(
            request.workspace_dir == Path::new(SOCIAL_WORKSPACE) && request.add_dirs.is_empty(),
            "social Codex execution requires /workspace and no extra directories"
        );
        ensure!(
            request.image_paths.iter().all(|path| {
                path.starts_with(SOCIAL_WORKSPACE)
                    && path
                        .components()
                        .all(|part| !matches!(part, Component::ParentDir))
            }),
            "social image inputs must be inside /workspace"
        );
        let mut selected_provider = None;
        for value in &request.config_overrides {
            let (key, value) = super::session::parse_config_override(value)
                .context("invalid social Codex model override")?;
            ensure!(
                is_model_setting(&key)
                    || (key == "web_search" && value.as_str() == Some("disabled"))
                    || (key == "features.goals" && value.as_bool() == Some(false)),
                "unsupported social Codex override"
            );
            if key == "model_provider" {
                selected_provider = Some(
                    value
                        .as_str()
                        .context("model_provider must be a string")?
                        .to_owned(),
                );
            }
        }
        self.validate_config(
            selected_provider
                .as_deref()
                .or(self.selected_provider.as_deref()),
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn command(&self) -> Result<Command> {
        self.command_with_hooks(None)
    }

    pub(crate) fn hook_source(&self) -> Result<Option<String>> {
        let path = self.runtime_root.join("codex/hooks.json");
        if !path.try_exists()? {
            return Ok(None);
        }
        private_file(&path)?;
        ensure!(
            std::fs::metadata(&path)?.len() <= 256 * 1024,
            "native_hook_source_too_large"
        );
        let source = std::fs::read_to_string(path).context("native_hook_source_unreadable")?;
        super::native_hooks::validate_source(&source)?;
        Ok(Some(source))
    }

    pub(crate) fn command_with_hooks(&self, hooks: Option<&HookMounts>) -> Result<Command> {
        // Revalidate before every respawn as well as the initial launch.
        self.validate()?;
        let codex = validated_binary(&self.codex_binary)?;
        // New Codex models route hosted tools through this bundled native
        // helper. Mount only the matching, validated executable; the model's
        // command tools remain disabled and no host directories are exposed.
        let code_mode_host = if !self.codex_app_worker {
            code_mode_host_binary(&codex)?
        } else {
            None
        };
        let bubblewrap = validated_binary(&self.bubblewrap_binary)?;
        let mut cmd = Command::new(bubblewrap);
        cmd.env_clear().current_dir("/");
        cmd.args([
            "--unshare-all",
            "--unshare-user",
            "--share-net",
            "--cap-drop",
            "ALL",
            "--new-session",
            "--die-with-parent",
        ]);
        // All destinations live in a fresh root. In particular, never create
        // mountpoints below an attacker-writable bind: older bwrap versions
        // could follow a symlink there into /oldroot during setup.
        for path in [
            "/usr",
            "/usr/bin",
            "/etc",
            "/etc/ssl",
            "/etc/ssl/certs",
            "/home",
            "/home/clawmingle",
            "/run",
            "/state",
            SOCIAL_CODEX_HOME,
            SOCIAL_SQLITE_HOME,
            SOCIAL_WORKSPACE,
        ] {
            cmd.args(["--dir", path]);
        }
        cmd.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
        cmd.arg("--ro-bind").arg(codex).arg("/usr/bin/codex");
        if let Some(host) = &code_mode_host {
            cmd.arg("--ro-bind")
                .arg(host)
                .arg("/usr/bin/codex-code-mode-host");
        }
        cmd.args(["--ro-bind", "/etc/resolv.conf", "/etc/resolv.conf"]);
        cmd.args([
            "--ro-bind",
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/ssl/certs/ca-certificates.crt",
        ]);
        // Credential values never enter argv or the inherited host environment.
        for (key, value) in &self.provider_environment {
            cmd.env(key, value);
        }
        // Command::env_clear removes the host environment before bwrap itself
        // starts. Setting secrets via --setenv would expose them in argv.
        for (key, value) in [
            ("HOME", "/home/clawmingle"),
            ("CODEX_HOME", SOCIAL_CODEX_HOME),
            ("CODEX_SQLITE_HOME", SOCIAL_SQLITE_HOME),
            ("PATH", "/usr/bin"),
            ("LANG", "C.UTF-8"),
            ("TMPDIR", "/tmp"),
            ("SSL_CERT_FILE", "/etc/ssl/certs/ca-certificates.crt"),
        ] {
            cmd.env(key, value);
        }
        // Last mounts: no subsequent --dir/--file/chmod or nested mount below
        // these persistent, writable source trees is permitted.
        cmd.arg("--bind")
            .arg(self.runtime_root.join("codex"))
            .arg(SOCIAL_CODEX_HOME);
        cmd.arg("--bind")
            .arg(self.runtime_root.join("sqlite"))
            .arg(SOCIAL_SQLITE_HOME);
        cmd.arg("--ro-bind")
            .arg(self.runtime_root.join("workspace"))
            .arg(SOCIAL_WORKSPACE);
        if let Some(hooks) = hooks {
            cmd.args([
                "--dir",
                "/bin",
                "--dir",
                super::native_hooks::AUTHORITY_SOCKET_DIRECTORY,
            ]);
            cmd.arg("--ro-bind").arg(&hooks.proxy).arg("/bin/sh");
            cmd.arg("--ro-bind")
                .arg(hooks.prepared.socket_directory())
                .arg(super::native_hooks::AUTHORITY_SOCKET_DIRECTORY);
            cmd.arg("--ro-bind")
                .arg(hooks.prepared.hooks_file())
                .arg("/state/codex/hooks.json");
            // Only public runtime libraries for the strict proxy, not a shell,
            // language runtime, host home or arbitrary host executables.
            #[cfg(target_arch = "x86_64")]
            let libraries = [
                "/lib64/ld-linux-x86-64.so.2",
                "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
                "/lib/x86_64-linux-gnu/libgcc_s.so.1",
                "/lib/x86_64-linux-gnu/libc.so.6",
            ];
            #[cfg(target_arch = "aarch64")]
            let libraries = [
                "/lib/ld-linux-aarch64.so.1",
                "/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1",
                "/lib/aarch64-linux-gnu/libgcc_s.so.1",
                "/lib/aarch64-linux-gnu/libc.so.6",
            ];
            for library in libraries {
                let source = Path::new(library)
                    .canonicalize()
                    .context("native_hook_library_missing")?;
                cmd.arg("--ro-bind").arg(source).arg(library);
            }
        }
        cmd.args(["--chdir", SOCIAL_WORKSPACE, "--", "/usr/bin/codex"]);
        for setting in forced_settings() {
            if self.image_worker && *setting == "features.image_generation=false" {
                continue;
            }
            if self.codex_app_worker && *setting == "features.apps=false" {
                continue;
            }
            cmd.arg("-c").arg(setting);
        }
        if hooks.is_some() {
            cmd.arg("-c").arg("features.hooks=true");
        }
        if self.codex_app_worker {
            cmd.arg("-c").arg("features.apps=true");
            cmd.arg("-c").arg("apps._default.enabled=false");
            cmd.arg("-c")
                .arg("apps.app-695325bae7348191b58ae9349a963d22.enabled=true");
        }
        if self.image_worker {
            cmd.arg("-c").arg("features.image_generation=true");
            cmd.arg("-c").arg("web_search=\"disabled\"");
        }
        cmd.arg("-c").arg(format!(
            "features.code_mode_host={}",
            code_mode_host.is_some()
        ));
        if let Some(provider) = &self.selected_provider {
            cmd.arg("-c").arg(format!(
                "model_provider={}",
                serde_json::to_string(provider)?
            ));
        }
        cmd.args(["app-server", "--listen", "stdio://"]);
        Ok(cmd)
    }

    fn validate_config(&self, provider_override: Option<&str>) -> Result<toml::Value> {
        #[cfg(not(target_os = "linux"))]
        anyhow::bail!("isolated social Codex requires Linux and bubblewrap");

        ensure!(
            !(self.image_worker && self.codex_app_worker),
            "isolated_worker_roles_conflict"
        );

        validated_binary(&self.bubblewrap_binary)?;
        validated_binary(&self.codex_binary)?;
        private_directory(&self.runtime_root)?;
        for name in ["codex", "sqlite", "workspace"] {
            private_directory(&self.runtime_root.join(name))?;
        }
        if self.image_worker {
            validate_worker_inputs(&self.runtime_root.join("workspace"))?;
        } else {
            ensure!(
                std::fs::read_dir(self.runtime_root.join("workspace"))?
                    .next()
                    .is_none(),
                "social Codex workspace must be empty to avoid inherited project configuration"
            );
        }
        let disabled_hooks = self.runtime_root.join("codex/hooks.disabled.json");
        if disabled_hooks.try_exists()? {
            private_file(&disabled_hooks)?;
            ensure!(
                std::fs::metadata(&disabled_hooks)?.len() <= 256 * 1024,
                "native_hook_source_too_large"
            );
            super::native_hooks::validate_source(&std::fs::read_to_string(disabled_hooks)?)?;
        }
        if self.hook_source()?.is_some() {
            hook_binaries()?;
        }
        let path = self.runtime_root.join("codex/config.toml");
        private_file(&path)?;
        let contents = std::fs::read_to_string(&path)
            .map_err(|_| anyhow::anyhow!("cannot read independent Codex configuration"))?;
        let config: toml::Value = contents
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid independent Codex configuration"))?;
        let table = config
            .as_table()
            .context("Codex configuration must be a table")?;
        for key in table.keys() {
            ensure!(
                is_model_setting(key)
                    || matches!(
                        key.as_str(),
                        "model_providers"
                            | "cli_auth_credentials_store"
                            | "forced_login_method"
                            | "disable_response_storage"
                            | "hide_agent_reasoning"
                            | "web_search"
                    ),
                "independent Codex config contains an unsupported setting; only model/provider/auth settings are allowed"
            );
        }
        if let Some(search) = config.get("web_search") {
            ensure!(
                matches!(search.as_str(), Some("live" | "cached" | "disabled")),
                "invalid social web search mode"
            );
        }
        if let Some(store) = config.get("cli_auth_credentials_store") {
            ensure!(
                store.as_str() == Some("file"),
                "isolated Codex credentials must use its own auth.json"
            );
        }
        let providers = config
            .get("model_providers")
            .and_then(toml::Value::as_table);
        let mut allowed_env = BTreeSet::from(["OPENAI_API_KEY".to_owned()]);
        if let Some(providers) = providers {
            for provider in providers.values() {
                if let Some(key) = provider.get("env_key").and_then(toml::Value::as_str) {
                    allowed_env.insert(key.to_owned());
                }
                if let Some(headers) = provider
                    .get("env_http_headers")
                    .and_then(toml::Value::as_table)
                {
                    for key in headers.values().filter_map(toml::Value::as_str) {
                        allowed_env.insert(key.to_owned());
                    }
                }
            }
        }
        for (key, value) in &self.provider_environment {
            ensure!(
                valid_provider_env(key) && allowed_env.contains(key),
                "unsupported provider environment variable"
            );
            ensure!(!value.is_empty(), "provider environment variable is empty");
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                ensure!(
                    !value.as_bytes().contains(&0),
                    "invalid provider environment value"
                );
            }
        }
        let provider_name = provider_override
            .or_else(|| config.get("model_provider").and_then(toml::Value::as_str))
            .unwrap_or("openai");
        let provider = providers.and_then(|providers| providers.get(provider_name));
        if self.image_worker || self.codex_app_worker {
            ensure!(
                provider_name == "openai"
                    && providers.is_none()
                    && self.provider_environment.is_empty()
                    && !self.runtime_root.join("codex/auth.json").try_exists()?,
                "worker_requires_external_openai_auth"
            );
        }
        ensure!(
            provider_name == "openai" || provider.is_some(),
            "selected Codex provider is not configured"
        );
        if let Some(key) = provider
            .and_then(|p| p.get("env_key"))
            .and_then(toml::Value::as_str)
        {
            ensure!(
                self.provider_environment.contains_key(key),
                "selected provider credential is not configured"
            );
        }
        let openai_auth = provider_name == "openai"
            || provider
                .and_then(|p| p.get("requires_openai_auth"))
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
        let auth = self.runtime_root.join("codex/auth.json");
        if auth.try_exists()? {
            private_file(&auth)?;
        }
        if !self.image_worker
            && !self.codex_app_worker
            && openai_auth
            && !self.provider_environment.contains_key("OPENAI_API_KEY")
        {
            private_file(&auth).context("independent Codex login or API credential is required")?;
            let bytes = std::fs::read(&auth)
                .map_err(|_| anyhow::anyhow!("cannot read independent Codex login"))?;
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("invalid independent Codex login"))?;
            let key = value
                .get("OPENAI_API_KEY")
                .and_then(serde_json::Value::as_str);
            let token = value
                .get("tokens")
                .and_then(|tokens| tokens.get("access_token"))
                .and_then(serde_json::Value::as_str);
            ensure!(
                key.or(token).is_some_and(|value| !value.trim().is_empty()),
                "independent Codex login is empty"
            );
        }
        Ok(config)
    }
}

pub(super) fn validate_worker_inputs(path: &Path) -> Result<()> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let entries = std::fs::read_dir(path)?
        .take(4)
        .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(entries.len() <= 3, "image_worker_input_limit");
    for entry in entries {
        let entry_name = entry.file_name();
        let name = entry_name.to_str().context("image_worker_input_invalid")?;
        ensure!(
            matches!(
                name,
                "reference-1.png"
                    | "reference-2.png"
                    | "reference-3.png"
                    | "reference-1.jpg"
                    | "reference-2.jpg"
                    | "reference-3.jpg"
            ),
            "image_worker_input_invalid"
        );
        private_file(&entry.path())?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(0o400000 | 0o4000)
            .open(entry.path())?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.nlink() == 1 && metadata.len() <= 2 * 1024 * 1024,
            "image_worker_input_invalid"
        );
        let mut signature = [0; 8];
        let length = file.read(&mut signature)?;
        ensure!(
            (name.ends_with(".png") && signature == *b"\x89PNG\r\n\x1a\n")
                || (name.ends_with(".jpg")
                    && length >= 3
                    && signature.starts_with(b"\xff\xd8\xff")),
            "image_worker_input_invalid"
        );
    }
    Ok(())
}

pub(crate) fn is_model_setting(key: &str) -> bool {
    matches!(
        key,
        "model"
            | "model_provider"
            | "model_reasoning_effort"
            | "model_reasoning_summary"
            | "model_context_window"
            | "model_auto_compact_token_limit"
            | "tool_output_token_limit"
            | "model_verbosity"
            | "service_tier"
    )
}

fn forced_settings() -> &'static [&'static str] {
    &[
        "approval_policy=\"never\"",
        "sandbox_mode=\"read-only\"",
        "cli_auth_credentials_store=\"file\"",
        "features.shell_tool=false",
        // Native per-turn executors must remain alive until upstream MCP/thread
        // shutdown completes. The upstream 60s idle delay outlives that lease.
        "thread_unload_delay_secs=0",
        "features.shell_snapshot=false",
        "features.unified_exec=false",
        "features.apps=false",
        "features.multi_agent=false",
        "features.multi_agent_v2=false",
        "agents.enabled=false",
        "features.hooks=false",
        "features.plugins=false",
        "features.memories=false",
        "features.code_mode=false",
        // Native image generation accepts local paths in the shared Codex
        // profile; 0.161 has no per-turn restricted read roots. Publish only
        // images collected from the conversation's separate workspace.
        "features.image_generation=false",
        "features.skill_mcp_dependency_install=false",
        "tools.view_image=false",
        "shell_environment_policy.inherit=\"none\"",
        "check_for_update_on_startup=false",
    ]
}

fn valid_provider_env(key: &str) -> bool {
    key.bytes()
        .enumerate()
        .all(|(i, b)| b == b'_' || b.is_ascii_uppercase() || (i > 0 && b.is_ascii_digit()))
        && !key.is_empty()
        && !matches!(key, "HOME" | "PATH" | "ENV" | "SHELL" | "TMPDIR" | "PWD")
        && ![
            "LD_",
            "DYLD_",
            "CODEX_",
            "BASH",
            "SSH_",
            "PYTHON",
            "NODE_",
            "RUST",
            "QQ_",
            "JEV_",
            "TYPESAFE_",
        ]
        .iter()
        .any(|prefix| key.starts_with(prefix))
}

fn reject_personal_path(path: &Path) -> Result<()> {
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        ensure!(
            path != home,
            "personal home cannot be a social runtime root"
        );
        for name in [".codex", ".codex-claw", ".ssh"] {
            ensure!(
                !path.starts_with(home.join(name)),
                "personal profile cannot be used by social Codex"
            );
        }
    }
    ensure!(
        path != Path::new("/") && path != Path::new("/home") && !path.starts_with("/root"),
        "protected directory cannot be a social runtime root"
    );
    Ok(())
}

/// Follow Codex's package layout without importing its config, profile, or
/// resource directory. Only the validated helper executable becomes a mount.
#[cfg(unix)]
fn code_mode_host_binary(codex: &Path) -> Result<Option<PathBuf>> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let uid = std::fs::metadata("/proc/self")?.uid();

    let directory = codex.parent().context("Codex binary has no parent")?;
    if directory.file_name().is_some_and(|name| name == "bin") {
        let package = directory.parent().context("Codex package has no root")?;
        let marker = package.join("codex-package.json");
        match std::fs::symlink_metadata(&marker) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file()
                        && (metadata.uid() == 0 || metadata.uid() == uid)
                        && !metadata.file_type().is_symlink()
                        && metadata.permissions().mode() & 0o022 == 0,
                    "Codex package marker must be a non-writable regular file"
                );
                let resources = package.join("codex-resources");
                let candidate = resources.join("codex-code-mode-host");
                if candidate
                    .try_exists()
                    .context("cannot inspect Codex package helper")?
                {
                    for parent in [package, directory, resources.as_path()] {
                        let metadata = std::fs::symlink_metadata(parent)?;
                        ensure!(
                            metadata.is_dir()
                                && (metadata.uid() == 0 || metadata.uid() == uid)
                                && !metadata.file_type().is_symlink()
                                && metadata.permissions().mode() & 0o022 == 0,
                            "Codex package helper directories must be real and non-writable by others"
                        );
                    }
                    let helper = validated_binary(&candidate)?;
                    let metadata = std::fs::metadata(&helper)?;
                    ensure!(
                        metadata.uid() == 0 || metadata.uid() == uid,
                        "Codex helper owner is untrusted"
                    );
                    ensure!(
                        helper == candidate,
                        "Codex package helper must stay inside the selected package"
                    );
                    return Ok(Some(helper));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot inspect Codex package marker"),
        }
    }
    let sibling = directory.join("codex-code-mode-host");
    if sibling
        .try_exists()
        .context("cannot inspect Codex sibling helper")?
    {
        Ok(Some(validated_binary(&sibling)?))
    } else {
        Ok(None)
    }
}

#[cfg(not(unix))]
fn code_mode_host_binary(_codex: &Path) -> Result<Option<PathBuf>> {
    anyhow::bail!("isolated social Codex requires Linux")
}

#[cfg(unix)]
fn validated_binary(path: &Path) -> Result<PathBuf> {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    ensure!(
        path.is_absolute(),
        "isolation executable path must be absolute"
    );
    let path = path
        .canonicalize()
        .context("isolation executable is unavailable")?;
    reject_personal_path(&path)?;
    let meta = std::fs::metadata(&path)?;
    ensure!(
        meta.is_file()
            && meta.permissions().mode() & 0o022 == 0
            && meta.permissions().mode() & 0o111 != 0,
        "isolation executable must be a non-writable executable file"
    );
    let mut magic = [0; 4];
    std::fs::File::open(&path)?.read_exact(&mut magic)?;
    ensure!(
        &magic == b"\x7fELF",
        "isolation requires native Linux executables, not profile wrappers"
    );
    Ok(path)
}

pub(crate) fn validate_workspace_directory(path: &Path) -> Result<()> {
    private_directory(path)
}

fn private_directory(path: &Path) -> Result<()> {
    private_path(path, true)
}

fn private_file(path: &Path) -> Result<()> {
    private_path(path, false)
}

#[cfg(unix)]
fn private_path(path: &Path, directory: bool) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    ensure!(path.is_absolute(), "social runtime paths must be absolute");
    reject_personal_path(path)?;
    let mut ancestor = PathBuf::new();
    for component in path.components() {
        ensure!(
            !matches!(component, Component::ParentDir | Component::CurDir),
            "social runtime paths must be normalized"
        );
        ancestor.push(component);
        ensure!(
            !std::fs::symlink_metadata(&ancestor)?
                .file_type()
                .is_symlink(),
            "social runtime paths must not contain symlinks"
        );
    }
    let meta = std::fs::symlink_metadata(path)?;
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let uid = unsafe { geteuid() };
    ensure!(
        uid != 0 && meta.uid() == uid,
        "social runtime must belong to the current non-root user"
    );
    let expected_mode = if directory { 0o700 } else { 0o600 };
    ensure!(
        meta.permissions().mode() & 0o7777 == expected_mode,
        "social runtime directory/file permissions must be 0700/0600"
    );
    ensure!(
        if directory {
            meta.is_dir()
        } else {
            meta.is_file()
        },
        "invalid social runtime path type"
    );
    Ok(())
}

#[cfg(not(unix))]
fn validated_binary(_path: &Path) -> Result<PathBuf> {
    anyhow::bail!("isolated social Codex requires Linux")
}

#[cfg(not(unix))]
fn private_path(_path: &Path, _directory: bool) -> Result<()> {
    anyhow::bail!("isolated social Codex requires Linux")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::{TempDir, tempdir};

    const LOCAL_PROVIDER: &str = "model_provider = 'test'\n[model_providers.test]\nname = 'test'\nbase_url = 'http://127.0.0.1:1/v1'\nwire_api = 'responses'\n";

    fn fixture() -> (TempDir, IsolatedAppServerConfig) {
        let dir = tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        for name in ["codex", "sqlite", "workspace"] {
            let path = dir.path().join(name);
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let config = IsolatedAppServerConfig {
            // These unit tests validate/build argv only; no process starts.
            bubblewrap_binary: PathBuf::from("/usr/bin/true"),
            codex_binary: PathBuf::from("/usr/bin/true"),
            runtime_root: dir.path().to_owned(),
            provider_environment: BTreeMap::new(),
            selected_provider: None,
            image_worker: false,
            codex_app_worker: false,
        };
        write_config(&config, LOCAL_PROVIDER);
        (dir, config)
    }

    fn write_config(config: &IsolatedAppServerConfig, text: &str) {
        let path = config.runtime_root.join("codex/config.toml");
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn helper_package(root: &Path) -> (PathBuf, PathBuf) {
        for directory in [root.join("bin"), root.join("codex-resources")] {
            std::fs::create_dir(&directory).unwrap();
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let codex = root.join("bin/codex");
        let helper = root.join("codex-resources/codex-code-mode-host");
        for executable in [&codex, &helper] {
            std::fs::copy("/usr/bin/true", executable).unwrap();
            std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        (codex, helper)
    }

    fn package_marker(root: &Path) {
        let marker = root.join("codex-package.json");
        std::fs::write(&marker, r#"{"version":"0.161.0"}"#).unwrap();
        std::fs::set_permissions(marker, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn code_mode_helper_resources_require_package_marker_and_keep_sibling_support() {
        let (root, _config) = fixture();
        let (codex, _helper) = helper_package(root.path());
        assert_eq!(code_mode_host_binary(&codex).unwrap(), None);
        let sibling = root.path().join("bin/codex-code-mode-host");
        std::fs::copy("/usr/bin/true", &sibling).unwrap();
        assert_eq!(code_mode_host_binary(&codex).unwrap(), Some(sibling));
    }

    #[test]
    fn code_mode_helper_package_resource_is_preferred_and_only_file_is_mounted() {
        let (root, mut config) = fixture();
        let (codex, helper) = helper_package(root.path());
        package_marker(root.path());
        std::fs::copy(
            "/usr/bin/true",
            root.path().join("bin/codex-code-mode-host"),
        )
        .unwrap();
        assert_eq!(code_mode_host_binary(&codex).unwrap(), Some(helper.clone()));
        config.codex_binary = codex;
        let command = config.command().unwrap();
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .collect();
        assert!(args.windows(3).any(|args| {
            args[0] == "--ro-bind"
                && args[1] == helper.to_string_lossy()
                && args[2] == "/usr/bin/codex-code-mode-host"
        }));
        assert!(
            !args
                .iter()
                .any(|arg| arg == &root.path().join("codex-resources").to_string_lossy())
        );
    }

    #[test]
    fn code_mode_helper_rejects_package_resource_escape() {
        let (root, _config) = fixture();
        let (codex, helper) = helper_package(root.path());
        package_marker(root.path());
        std::fs::remove_file(&helper).unwrap();
        symlink("/usr/bin/true", &helper).unwrap();
        assert!(code_mode_host_binary(&codex).is_err());
    }

    #[test]
    fn code_mode_helper_rejects_untrusted_package_marker_or_directory() {
        let (root, _config) = fixture();
        let (codex, _helper) = helper_package(root.path());
        package_marker(root.path());
        let marker = root.path().join("codex-package.json");
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(code_mode_host_binary(&codex).is_err());
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(
            root.path().join("codex-resources"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        assert!(code_mode_host_binary(&codex).is_err());
    }

    #[test]
    fn isolated_config_rejects_profile_execution_and_public_credentials() {
        let (_dir, config) = fixture();
        config.validate().unwrap();
        for extra in [
            "[profiles.personal]\nmodel='other'\n",
            "[mcp_servers.test]\ncommand='program'\n",
            "[hooks]\n",
            "notify=['program']\n",
        ] {
            // End the provider table before adding each top-level setting.
            write_config(&config, &format!("{extra}\n{LOCAL_PROVIDER}"));
            assert!(config.validate().is_err());
        }
        write_config(&config, LOCAL_PROVIDER);
        std::fs::set_permissions(
            config.runtime_root.join("codex/config.toml"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn isolated_config_requires_own_login_without_fallback() {
        let (_dir, config) = fixture();
        write_config(&config, "model='example'\n");
        assert!(config.validate().is_err());
        let path = config.runtime_root.join("codex/auth.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn isolated_config_accepts_codex_native_tool_output_budget() {
        let (_dir, config) = fixture();
        write_config(
            &config,
            &format!("tool_output_token_limit=30000\n{LOCAL_PROVIDER}"),
        );
        config.validate().unwrap();
        assert!(config.command().is_ok());
    }

    #[test]
    fn image_worker_requires_external_auth_official_provider_and_explicit_images() {
        let (_directory, mut config) = fixture();
        config.image_worker = true;
        write_config(&config, "model='gpt-6.1-sol'\n");
        config.validate().unwrap();
        let args = config
            .command()
            .unwrap()
            .as_std()
            .get_args()
            .map(|value| value.to_owned())
            .collect::<Vec<_>>();
        assert!(
            args.iter()
                .any(|value| value == "features.image_generation=true")
        );
        assert!(
            !args
                .iter()
                .any(|value| value == "features.image_generation=false")
        );
        config
            .provider_environment
            .insert("OPENAI_API_KEY".into(), "fixture".into());
        assert!(config.validate().is_err());
        config.provider_environment.clear();
        write_config(&config, LOCAL_PROVIDER);
        assert!(config.validate().is_err());
        write_config(&config, "model='gpt-6.1-sol'\n");
        std::fs::write(config.runtime_root.join("workspace/AGENTS.md"), "untrusted").unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn hosted_worker_enables_only_apps_with_no_auth_images_or_workspace_inputs() {
        let (_directory, mut config) = fixture();
        config.codex_app_worker = true;
        write_config(&config, "model_provider='openai'\nweb_search='disabled'\n");
        config.validate().unwrap();
        let command = config.command().unwrap();
        let args = command.as_std().get_args().collect::<Vec<_>>();
        assert!(args.iter().any(|a| *a == "features.apps=true"));
        assert!(!args.iter().any(|a| *a == "features.apps=false"));
        assert!(args.iter().any(|a| *a == "features.image_generation=false"));
        assert!(!args.iter().any(|a| *a == "features.image_generation=true"));
        assert!(args.iter().any(|a| *a == "apps._default.enabled=false"));
        assert!(!args.iter().any(|a| *a == "features.code_mode_host=true"));
        config.image_worker = true;
        assert!(config.validate().is_err());
        config.image_worker = false;
        std::fs::write(config.runtime_root.join("workspace/AGENTS.md"), "untrusted").unwrap();
        assert!(config.validate().is_err());
        std::fs::remove_file(config.runtime_root.join("workspace/AGENTS.md")).unwrap();
        let path = config.runtime_root.join("codex/auth.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn explicit_provider_selection_applies_before_auth_validation_and_launch() {
        let (_dir, mut config) = fixture();
        write_config(&config, LOCAL_PROVIDER.split_once('\n').unwrap().1);
        assert!(config.validate().is_err()); // default OpenAI has no independent auth
        config.selected_provider = Some("test".into());
        config.validate().unwrap();
        let command = config.command().unwrap();
        assert!(
            command
                .as_std()
                .get_args()
                .any(|arg| arg == "model_provider=\"test\"")
        );
        assert!(!config.runtime_root.join("codex/auth.json").exists());
    }

    #[test]
    fn isolated_config_rejects_symlinks_and_project_instructions() {
        let (_dir, config) = fixture();
        let config_path = config.runtime_root.join("codex/config.toml");
        let other = config.runtime_root.join("other.toml");
        std::fs::rename(&config_path, &other).unwrap();
        symlink(&other, &config_path).unwrap();
        assert!(config.validate().is_err());
        std::fs::remove_file(&config_path).unwrap();
        std::fs::rename(other, config_path).unwrap();
        std::fs::write(
            config.runtime_root.join("workspace/AGENTS.md"),
            "unexpected project instructions",
        )
        .unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn provider_environment_is_explicit_and_never_placed_in_argv() {
        let (_dir, mut config) = fixture();
        write_config(
            &config,
            &format!("{LOCAL_PROVIDER}env_key='TEST_MODEL_KEY'\n"),
        );
        assert!(config.validate().is_err());
        let marker = "example-provider-value";
        config
            .provider_environment
            .insert("TEST_MODEL_KEY".to_owned(), marker.into());
        let command = config.command().unwrap();
        let args: Vec<_> = command.as_std().get_args().collect();
        assert!(
            !args
                .iter()
                .any(|arg| arg.to_string_lossy().contains(marker))
        );
        let envs: BTreeMap<_, _> = command.as_std().get_envs().collect();
        assert_eq!(
            envs.get(std::ffi::OsStr::new("TEST_MODEL_KEY"))
                .copied()
                .flatten(),
            Some(std::ffi::OsStr::new(marker))
        );
        config
            .provider_environment
            .insert("QQ_APP_SECRET".to_owned(), "example".into());
        assert!(config.validate().is_err());
        assert!(!valid_provider_env("LD_PRELOAD"));
        assert!(!valid_provider_env("HOME"));
    }

    #[test]
    fn mount_plan_never_creates_nodes_below_persistent_binds() {
        let (_dir, config) = fixture();
        let command = config.command().unwrap();
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let first_bind = args.iter().position(|arg| arg == "--bind").unwrap();
        assert!(
            args[..first_bind].iter().any(|arg| arg == "--unshare-pid")
                || args[..first_bind].iter().any(|arg| arg == "--unshare-all")
        );
        assert!(args[first_bind..].iter().all(|arg| {
            !["--dir", "--file", "--tmpfs", "--proc", "--dev", "--chmod"].contains(&arg.as_str())
        }));
        for flag in ["--bind", "--ro-bind"] {
            for mount in args.windows(3).filter(|args| args[0] == flag) {
                assert_ne!(mount[2], "/");
                assert!(!mount[2].starts_with("/home/"));
                assert!(!mount[2].starts_with("/oldroot"));
            }
        }
        assert!(args.iter().any(|arg| arg == "features.shell_tool=false"));
        assert!(args.iter().any(|arg| arg == "features.apps=false"));
        assert!(
            args.iter()
                .any(|arg| arg == "features.image_generation=false")
        );
        assert!(args.iter().any(|arg| arg == "approval_policy=\"never\""));
    }

    #[test]
    fn social_request_cannot_expand_view_or_enable_tools() {
        use crate::model::settings::{ReasoningEffort, SessionState};
        let (_dir, config) = fixture();
        let mut request = ExecutionRequest {
            prompt: "hello".to_owned(),
            workspace_dir: SOCIAL_WORKSPACE.into(),
            codex_home: SOCIAL_CODEX_HOME.into(),
            config_overrides: Vec::new(),
            add_dirs: Vec::new(),
            session_state: SessionState::default(),
            model: None,
            service_tier: None,
            context_mode: None,
            reasoning_effort: ReasoningEffort::High,
            image_paths: Vec::new(),
        };
        config.validate_request(&request).unwrap();
        request
            .config_overrides
            .push("tool_output_token_limit=30000".into());
        config.validate_request(&request).unwrap();
        request.config_overrides.clear();
        request
            .config_overrides
            .push("features.shell_tool=true".to_owned());
        assert!(config.validate_request(&request).is_err());
        request.config_overrides.clear();
        request
            .config_overrides
            .push("web_search=\"disabled\"".into());
        config.validate_request(&request).unwrap();
        request.config_overrides[0] = "web_search=\"live\"".into();
        assert!(config.validate_request(&request).is_err());
        request.config_overrides[0] = "features.goals=false".into();
        config.validate_request(&request).unwrap();
        request.config_overrides[0] = "features.goals=true".into();
        assert!(config.validate_request(&request).is_err());
        request.config_overrides.clear();
        request.add_dirs.push(PathBuf::from("/home"));
        assert!(config.validate_request(&request).is_err());
    }
}
