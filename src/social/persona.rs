//! Shared operator-owned identity and personality; conversation memory stays scoped.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use super::config::{DEFAULT_SOUL, SocialConfig, private_read_limit};

pub const IDENTITY_FILE: &str = "persona/IDENTITY.md";
pub const SOUL_FILE: &str = "persona/SOUL.md";
pub const DEFAULT_IDENTITY: &str = "你叫 Codex，是参与 QQ 群聊与私聊的 AI 群友。ClawMingle 是承载你的应用名称，不是你的称呼。诚实说明自己的 AI 身份，不冒充真人或虚构个人经历。身份设定不改变工具权限或程序规则。";

pub fn load(config: &mut SocialConfig) -> Result<()> {
    if config.persona_files {
        config.identity = private_read_limit(&config.data_dir.join(IDENTITY_FILE), 8000)
            .context("identity_file_unavailable")?;
        config.soul = private_read_limit(&config.data_dir.join(SOUL_FILE), 32000)
            .context("soul_file_unavailable")?;
    }
    Ok(())
}

pub fn validate(config: &SocialConfig) -> Result<()> {
    if config.persona_files {
        ensure!(
            !config.identity.trim().is_empty()
                && config.identity.chars().count() <= 2000
                && !config.identity.contains('\0'),
            "identity_file_invalid"
        );
        ensure!(
            !config.soul.trim().is_empty() && !config.soul.contains('\0'),
            "soul_file_invalid"
        );
    }
    Ok(())
}

pub fn settings(config: &SocialConfig) -> Value {
    let legacy_default = config.soul.trim().is_empty() || config.soul == DEFAULT_SOUL;
    // A legacy custom Soul may define another name: don't silently suggest Codex
    // as its replacement. The operator supplies Identity when first saving it.
    let identity = if config.persona_files {
        config.identity.as_str()
    } else if legacy_default {
        DEFAULT_IDENTITY
    } else {
        ""
    };
    json!({
        "files_enabled": config.persona_files,
        "identity": identity,
        "identity_source": if config.persona_files {IDENTITY_FILE} else {"legacy_soul"},
        "soul_source": if config.persona_files {SOUL_FILE} else if legacy_default {"default"} else {"social.toml:soul"},
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::Path,
    };

    fn write(path: &Path, value: &str) {
        fs::write(path, value).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let dir = root.path().join("persona");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        write(&dir.join("IDENTITY.md"), "你叫星河。\n");
        write(&dir.join("SOUL.md"), " 坦率、有主见。\n");
        let config_path = root.path().join("social.toml");
        write(
            &config_path,
            &format!(
                "data_dir = {:?}\nallowed_groups = ['test-group']\npersona_files = true\nsoul = '旧配置不会覆盖文件'",
                root.path().to_str().unwrap()
            ),
        );
        (root, config_path)
    }

    #[test]
    fn file_persona_overrides_inline_and_is_injected_in_every_mode() {
        let (_root, path) = fixture();
        let mut config = SocialConfig::load(&path).unwrap();
        assert_eq!(config.identity, "你叫星河。\n");
        assert_eq!(config.soul, " 坦率、有主见。\n");
        for mode in ["mode-1", "mode-2", "mode-3", "mode-all"] {
            config.mode = serde_json::from_value(json!(mode)).unwrap();
            let text = super::super::capabilities::instructions(&config).unwrap();
            assert!(text.contains("IDENTITY.md") && text.contains("SOUL.md"));
            assert!(text.contains(&config.identity) && text.contains(&config.soul));
            assert!(!text.contains("旧配置不会覆盖文件"));
        }
        assert_eq!(settings(&config)["identity_source"], IDENTITY_FILE);
    }

    #[test]
    fn invalid_files_fail_instead_of_silently_using_old_persona() {
        let (root, path) = fixture();
        let identity = root.path().join(IDENTITY_FILE);
        for value in [" ".to_owned(), "a".repeat(2001), "无效\0身份".to_owned()] {
            write(&identity, &value);
            assert!(SocialConfig::load(&path).is_err());
        }
        fs::remove_file(&identity).unwrap();
        assert!(SocialConfig::load(&path).is_err());
        symlink(root.path().join(SOUL_FILE), &identity).unwrap();
        assert!(SocialConfig::load(&path).is_err());
        fs::remove_file(&identity).unwrap();
        write(&identity, "身份");
        fs::set_permissions(&identity, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(SocialConfig::load(&path).is_err());
        write(&identity, "身份");
        write(&root.path().join(SOUL_FILE), "\n");
        assert!(SocialConfig::load(&path).is_err());
    }

    #[test]
    fn legacy_custom_identity_is_preserved_until_explicit_migration() {
        let (_root, path) = fixture();
        let raw = fs::read_to_string(&path)
            .unwrap()
            .replace("persona_files = true", "persona_files = false");
        write(&path, &raw);
        let config = SocialConfig::load(&path).unwrap();
        assert_eq!(config.soul, "旧配置不会覆盖文件");
        assert!(config.identity.is_empty());
        assert_eq!(settings(&config)["identity"], "");
        assert!(
            !super::super::capabilities::instructions(&config)
                .unwrap()
                .contains("你叫星河")
        );
    }
}
