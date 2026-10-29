use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::model::social::ParticipationMode;

/// Shared persona for every participation mode; existing nonempty Soul stays authoritative.
pub const DEFAULT_SOUL: &str = "你是 Codex，一个有主见、略嘴欠的聪明群友。诚实、敏锐、愿意思考，有自己的判断，也愿意在证据充分时改变看法。先理解对方具体在说什么，再自然回应；赞同时说明理由，不为讨好而附和或夸奖，分歧时把原因讲清楚。熟悉、轻松的聊天里可以接梗、逗趣、适度吐槽，幽默应贴合当前语境和彼此关系，不把每句话变成讽刺，不刻意抬杠，不贴标签或贬低人。对方认真、难受、急于解决问题或明确说别开玩笑时，就收起调侃，把事情处理好。闲聊可以简短俏皮；讲解、排查、执行任务时保留同样的聪明与坦诚，按需要充分解释并交付结果，不用嘴欠或简短掩盖敷衍。不确定就直说，区分事实、推测与未知，不冒充人类经历，不声称完成未实际执行的操作。语气像熟悉的群友，自然、有温度，不用固定人设口头禅；无需在回复中宣告自己“有主见”或“嘴欠”。所有参与模式使用同一人格与回答质量；模式只决定何时接话。人格不会改变权限、工具边界或程序规则。自然运用已确认的共同记忆和成员偏好，仅在与当前交流有关时提及；不靠反复展示“我记得你”表现熟悉，不把旧记录当作固定标签。区分自述、推断和玩笑，允许人的偏好及关系变化，遇到当前说法与记忆冲突时核对和更正。对缺少的背景按需查询当前会话记忆与原文，不编造共同经历，不跨群聊或私聊混用私人信息。";

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SocialConfig {
    pub data_dir: PathBuf,
    pub qq_credentials_file: PathBuf,
    pub allowed_groups: Vec<String>,
    pub mode: ParticipationMode,
    pub quiet_ms: u64,
    pub max_wait_ms: u64,
    pub traffic: super::traffic::TrafficSettings,
    pub model_admission: crate::codex::app_server::admission::ModelAdmissionConfig,
    pub input_budget: super::input_budget::InputBudget,
    pub batching: super::batching::Settings,
    pub context_messages: usize,
    pub context_windows: super::behavior::ContextWindows,
    pub behavior: super::behavior::Behavior,
    pub max_messages: usize,
    pub max_active_requests: usize,
    pub retention_hours: u64,
    pub request_link_seconds: u64,
    pub max_bubbles: usize,
    pub max_bubble_chars: usize,
    pub bubble_interval_ms: u64,
    pub jev: JevConfig,
    pub codex: CodexConfig,
    pub command_admins: Vec<String>,
    /// Explicit per-conversation administrators. An empty entry denies everyone;
    /// a missing entry retains command_admins for backwards compatibility.
    pub scope_admins: BTreeMap<String, Vec<String>>,
    pub tasks_enabled: bool,
    pub private_enabled: bool,
    pub private_allowed_users: Vec<String>,
    pub max_private_sessions: usize,
    pub model_presets: BTreeMap<String, ModelPreset>,
    pub soul: String,
    /// Explicit opt-in: both shared persona files are required when enabled.
    pub persona_files: bool,
    /// Resolved from IDENTITY.md, never a second inline configuration source.
    #[serde(skip)]
    pub identity: String,
    /// Resolved from this conversation's persisted identity, never from TOML.
    #[serde(skip)]
    pub conversation_name: Option<String>,
    /// Resolved only from the current group, never loaded from TOML.
    #[serde(skip)]
    pub group_profile: Option<crate::model::social::GroupProfile>,
    pub enabled_skills: Vec<String>,
    pub reply_format: String,
    pub enabled_tools: Vec<String>,
    pub backup: super::backup::BackupConfig,
    pub memory: super::memory::MemorySettings,
    pub interactions: super::interaction::InteractionSettings,
    pub integrations: crate::codex::app_server::scoped_integrations::IntegrationConfig,
    pub stickers: super::stickers::StickerSettings,
    pub documents: super::documents::DocumentConfig,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPreset {
    pub model: String,
    pub reasoning_effort: String,
}

impl Default for SocialConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("~/.local/share/clawmingle"),
            qq_credentials_file: PathBuf::from("~/.config/clawmingle/qq.env"),
            allowed_groups: Vec::new(),
            mode: ParticipationMode::Mentions,
            quiet_ms: 1200,
            max_wait_ms: 4000,
            traffic: Default::default(),
            model_admission: Default::default(),
            input_budget: Default::default(),
            batching: Default::default(),
            context_messages: 20,
            context_windows: super::behavior::ContextWindows::default(),
            behavior: super::behavior::Behavior::default(),
            memory: super::memory::MemorySettings::default(),
            interactions: super::interaction::InteractionSettings::default(),
            integrations: Default::default(),
            stickers: super::stickers::StickerSettings::default(),
            documents: super::documents::DocumentConfig::default(),
            max_messages: 500,
            max_active_requests: 16,
            retention_hours: 24,
            request_link_seconds: 600,
            max_bubbles: 3,
            max_bubble_chars: 1000,
            bubble_interval_ms: 600,
            jev: JevConfig::default(),
            codex: CodexConfig::default(),
            command_admins: Vec::new(),
            scope_admins: BTreeMap::new(),
            tasks_enabled: true,
            private_enabled: true,
            private_allowed_users: Vec::new(),
            max_private_sessions: 16,
            soul: DEFAULT_SOUL.into(),
            persona_files: false,
            identity: String::new(),
            conversation_name: None,
            group_profile: None,
            enabled_skills: Vec::new(),
            reply_format: "text".into(),
            enabled_tools: crate::codex::app_server::SocialTurnInput::TOOL_NAMES
                .iter()
                .filter(|name| {
                    !crate::codex::app_server::scoped_integrations::TOOL_NAMES.contains(name)
                })
                .map(|s| (*s).into())
                .collect(),
            backup: super::backup::BackupConfig::default(),
            model_presets: BTreeMap::from([
                (
                    "sol".into(),
                    ModelPreset {
                        model: "gpt-6.1-sol".into(),
                        reasoning_effort: "high".into(),
                    },
                ),
                (
                    "luna".into(),
                    ModelPreset {
                        model: "gpt-6-luna".into(),
                        reasoning_effort: "medium".into(),
                    },
                ),
            ]),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JevConfig {
    pub model: String,
    pub credentials_file: PathBuf,
    pub timeout_ms: u64,
    pub retries: u32,
}

impl Default for JevConfig {
    fn default() -> Self {
        Self {
            model: "jev-1.14.0".into(),
            credentials_file: PathBuf::from("~/.config/clawmingle/jev.env"),
            timeout_ms: 5000,
            retries: 1,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodexConfig {
    pub binary: PathBuf,
    pub bubblewrap_binary: PathBuf,
    pub model: String,
    pub reasoning_effort: String,
    /// None uses the provider in the independent runtime config.toml.
    pub provider: Option<String>,
    pub credentials_file: Option<PathBuf>,
    pub turn_timeout_seconds: u64,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("/opt/codex/current/bin/codex"),
            bubblewrap_binary: PathBuf::from("/usr/bin/bwrap"),
            model: "gpt-6.1-sol".into(),
            reasoning_effort: "high".into(),
            provider: None,
            credentials_file: None,
            turn_timeout_seconds: 180,
        }
    }
}

pub fn private_read(path: &Path) -> Result<String> {
    private_read_limit(path, 65_536)
}

pub fn private_read_limit(path: &Path, maximum_bytes: u64) -> Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        let directory = std::fs::symlink_metadata(
            path.parent()
                .ok_or_else(|| anyhow::anyhow!("config_has_no_parent"))?,
        )?;
        let owner = std::fs::metadata("/proc/self")?.uid();
        ensure!(
            directory.is_dir()
                && directory.uid() == owner
                && directory.permissions().mode() & 0o777 == 0o700,
            "private_config_directory_required"
        );
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(0o400000)
            .open(path)?; // Linux O_NOFOLLOW
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == owner
                && metadata.permissions().mode() & 0o777 == 0o600
                && metadata.len() <= maximum_bytes,
            "private_config_file_required"
        );
        use std::io::Read;
        let mut text = String::new();
        file.take(maximum_bytes + 1).read_to_string(&mut text)?;
        ensure!(text.len() as u64 <= maximum_bytes, "private_file_too_large");
        Ok(text)
    }
    #[cfg(not(unix))]
    anyhow::bail!("social_runtime_requires_linux")
}

pub fn read_environment(path: &Path) -> Result<BTreeMap<String, String>> {
    let raw = private_read(path)?;
    let mut values = BTreeMap::new();
    for line in raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let (name, raw_value) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("credential_assignment_required"))?;
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
            "credential_name_invalid"
        );
        let parsed =
            shlex::split(raw_value).ok_or_else(|| anyhow::anyhow!("credential_quotes_invalid"))?;
        ensure!(
            parsed.len() == 1 && !parsed[0].is_empty() && !parsed[0].contains(['\r', '\n', '\0']),
            "credential_value_missing"
        );
        ensure!(
            values.insert(name.to_string(), parsed[0].clone()).is_none(),
            "duplicate_credential_assignment"
        );
    }
    Ok(values)
}

impl SocialConfig {
    pub fn is_scope_admin(&self, author: &str, private: bool, target: &str) -> bool {
        let key = format!("{}:{target}", if private { "private" } else { "group" });
        self.scope_admins
            .get(&key)
            .unwrap_or(&self.command_admins)
            .iter()
            .any(|id| id == author)
    }
    pub fn load(path: &Path) -> Result<Self> {
        let raw = private_read(path)?;
        let mut config: Self =
            toml::from_str(&raw).map_err(|_| anyhow::anyhow!("social_config_invalid"))?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("home_unavailable"))?;
        for path in [
            &mut config.data_dir,
            &mut config.qq_credentials_file,
            &mut config.jev.credentials_file,
            &mut config.codex.binary,
            &mut config.codex.bubblewrap_binary,
        ] {
            *path = expand_home(path, &home);
            ensure!(path.is_absolute(), "absolute_config_path_required");
        }
        if let Some(path) = &mut config.codex.credentials_file {
            *path = expand_home(path, &home);
        }
        if let Some(path) = &mut config.backup.directory {
            *path = expand_home(path, &home);
        }
        super::persona::load(&mut config)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.model_admission.validate()?;
        self.backup.validate(&self.data_dir)?;
        ensure!(
            self.scope_admins.len() <= 64
                && self.scope_admins.iter().all(|(scope, admins)| {
                    let target = scope
                        .strip_prefix("group:")
                        .or_else(|| scope.strip_prefix("private:"));
                    target.is_some_and(|target| {
                        !target.is_empty()
                            && target.len() <= 256
                            && target
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                    }) && admins.len() <= 32
                        && admins.iter().all(|id| {
                            !id.is_empty()
                                && id.len() <= 256
                                && id
                                    .bytes()
                                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                        })
                }),
            "invalid_scope_admins"
        );
        crate::codex::app_server::SocialTurnInput::validate_tools(&self.enabled_tools)?;
        ensure!(
            matches!(self.reply_format.as_str(), "text" | "markdown"),
            "invalid_reply_format"
        );
        ensure!(self.soul.chars().count() <= 8000, "soul_too_large");
        super::persona::validate(self)?;
        self.behavior.validate()?;
        self.memory.validate()?;
        self.interactions.validate()?;
        self.integrations.validate()?;
        self.stickers.validate()?;
        self.documents.validate()?;
        self.context_windows.validate()?;
        self.input_budget.validate()?;
        self.batching.validate()?;
        self.traffic.validate()?;
        ensure!(
            self.enabled_skills.len() <= 16
                && self.enabled_skills.iter().all(|name| !name.is_empty()
                    && name.len() <= 32
                    && name.bytes().all(|b| b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || b == b'_'
                        || b == b'-')),
            "invalid_enabled_skills"
        );
        ensure!(
            (1..=32).contains(&self.max_private_sessions),
            "invalid_private_session_limit"
        );
        ensure!(
            self.command_admins.len() <= 32 && self.private_allowed_users.len() <= 64,
            "invalid_user_allowlist_limit"
        );
        ensure!(
            self.command_admins
                .iter()
                .chain(&self.private_allowed_users)
                .all(|id| !id.is_empty()
                    && id.len() <= 256
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')),
            "invalid_user_allowlist"
        );
        ensure!(
            self.model_presets.len() <= 16
                && self
                    .model_presets
                    .iter()
                    .all(|(alias, preset)| !alias.is_empty()
                        && alias.len() <= 32
                        && alias
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                        && !preset.model.is_empty()
                        && preset.model.len() <= 128
                        && preset
                            .model
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b))
                        && matches!(
                            preset.reasoning_effort.as_str(),
                            "low" | "medium" | "high" | "xhigh"
                        )),
            "invalid_model_presets"
        );
        ensure!(
            !self.allowed_groups.is_empty() && self.allowed_groups.len() <= 8,
            "explicit_group_allowlist_required"
        );
        ensure!(
            self.allowed_groups.iter().all(|g| !g.starts_with("YOUR_")
                && !g.is_empty()
                && g.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')),
            "invalid_group_allowlist"
        );
        let mut groups = self.allowed_groups.clone();
        groups.sort();
        groups.dedup();
        ensure!(
            groups.len() == self.allowed_groups.len(),
            "duplicate_group_allowlist"
        );
        ensure!(
            self.quiet_ms >= 100 && self.quiet_ms <= self.max_wait_ms && self.max_wait_ms <= 10_000,
            "invalid_collection_window"
        );
        ensure!(
            (1..=40).contains(&self.context_messages) && (50..=2000).contains(&self.max_messages),
            "invalid_history_limits"
        );
        ensure!(
            (1..=16).contains(&self.max_active_requests)
                && (1..=168).contains(&self.retention_hours)
                && self.request_link_seconds <= 3600,
            "invalid_retention_or_queue"
        );
        ensure!(
            (1..=3).contains(&self.max_bubbles)
                && (1..=2000).contains(&self.max_bubble_chars)
                && self.bubble_interval_ms <= 10_000,
            "invalid_bubble_limits"
        );
        ensure!(
            (100..=30_000).contains(&self.jev.timeout_ms)
                && self.jev.retries <= 2
                && !self.jev.model.is_empty(),
            "invalid_jev_config"
        );
        ensure!(
            !self.codex.model.is_empty()
                && matches!(
                    self.codex.reasoning_effort.as_str(),
                    "low" | "medium" | "high" | "xhigh"
                ),
            "invalid_codex_model_config"
        );
        ensure!(
            (10..=600).contains(&self.codex.turn_timeout_seconds),
            "invalid_codex_timeout"
        );
        Ok(())
    }
}

fn expand_home(path: &Path, home: &Path) -> PathBuf {
    path.strip_prefix("~")
        .map(|tail| home.join(tail))
        .unwrap_or_else(|_| path.to_path_buf())
}
