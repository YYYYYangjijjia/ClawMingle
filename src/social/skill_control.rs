//! Authorized instruction-skill controls. Incoming platform identity supplies
//! permission; model output, quoted messages, and downloaded prose never do.
use super::{
    config::{SocialConfig, private_read_limit},
    control::{control_reply, effective_config},
    skill_install,
};
use crate::model::social::GroupState;
use anyhow::Result;

pub async fn handle(
    state: &mut GroupState,
    seq: u64,
    text: &str,
    config: &SocialConfig,
    private: bool,
    target: &str,
    now: i64,
) -> Result<bool> {
    if skill_install::parse_skill_list_command(text) {
        state.begin_decision(seq)?;
        let selected = effective_config(config, state, None)?.enabled_skills;
        let listing = if selected.is_empty() {
            "暂无。".into()
        } else {
            selected.join("、")
        };
        let mut menu = super::command_menu::result(
            text,
            format!(
                "本对话已启用的 Skill：{listing}\n管理员可用 /skill-install GitHub链接 安装受限技能包，或 /skill on|off NAME 启停。脚本仅通过隔离工作区工具执行。"
            ),
            config.reply_format == "markdown",
        );
        if config.is_scope_admin(&state.record(seq)?.message.author_id, private, target)
            && selected.len() <= 4
        {
            for name in selected
                .iter()
                .filter(|name| !config.enabled_skills.contains(name))
            {
                menu.button(&format!("停用 {name}"), &format!("/skill off {name}"));
            }
        }
        super::control::control_menu_reply(state, seq, menu, config, now)?;
        return Ok(true);
    }
    let toggle = if text.trim() == "/skill" {
        Some(Vec::new())
    } else {
        text.trim()
            .strip_prefix("/skill ")
            .map(|args| args.split_whitespace().collect::<Vec<_>>())
    };
    let explicit = text.trim_start().starts_with("/skill-install");
    let message = &state.record(seq)?.message;
    let addressed_by_name = text.trim_start().to_ascii_lowercase().starts_with("codex");
    if !explicit
        && toggle.is_none()
        && !private
        && !message.at_bot
        && (effective_config(config, state, None)?.mode
            == crate::model::social::ParticipationMode::Mentions
            || !addressed_by_name)
    {
        return Ok(false);
    }
    let parsed = if toggle.is_none() {
        skill_install::parse_install_request(text)
    } else {
        Ok(None)
    };
    if toggle.is_none() && matches!(parsed, Ok(None)) {
        return Ok(false);
    }
    let administrator = config.is_scope_admin(&message.author_id, private, target);
    state.begin_decision(seq)?;
    let reply = if !administrator {
        "只有已配置的群命令管理员可以安装或启停 Skill；私聊也需要对应身份授权。".into()
    } else if let Some(arguments) = toggle {
        match arguments.as_slice() {
            [action, name] if matches!(*action, "on" | "off") && valid_name(name) => {
                if *action == "off" {
                    if config.enabled_skills.iter().any(|skill| skill == name) {
                        "此 Skill 由后台全局启用，请在管理页面停用。".into()
                    } else {
                        state.enabled_skills.retain(|skill| skill != name);
                        format!("已在本对话停用 {name}。")
                    }
                } else {
                    let path = config.data_dir.join("skills").join(name).join("SKILL.md");
                    match private_read_limit(&path, 16384) {
                        Ok(content) if !content.contains("description: 已移除") => {
                            enable(state, name, config)
                        }
                        _ => "此 Skill 尚未安装或已移除，请先安装。".into(),
                    }
                }
            }
            _ => "格式：/skill on NAME 或 /skill off NAME；/skills 查看本对话列表。".into(),
        }
    } else {
        match parsed {
            Err(_)=>"请提供一个明确的 GitHub Skill 目录或 SKILL.md 链接；使用 /skill-install 链接。不接受含凭据、查询参数或其他站点的地址。".into(),
            Ok(Some(spec))=>match skill_install::install(&spec,&config.data_dir.join("skills")).await {
                Ok(installed)=>format!("{}\n{}",enable(state,&installed.name,config),super::skill_package::PACKAGE_NOTICE),
                Err(_)=>"Skill 安装未完成：请检查GitHub来源、SKILL.md格式、包文件/大小限制及同名版本冲突。未执行安装脚本、安装依赖或扩展权限；已有包不会被覆盖。".into(),
            },
            Ok(None)=>unreachable!("ordinary chat returned above"),
        }
    };
    control_reply(state, seq, reply, config, now)?;
    Ok(true)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}
fn enable(state: &mut GroupState, name: &str, config: &SocialConfig) -> String {
    let mut candidate = state.clone();
    if !candidate.enabled_skills.iter().any(|skill| skill == name) {
        candidate.enabled_skills.push(name.into());
    }
    let valid = effective_config(config, &candidate, None)
        .and_then(|effective| super::capabilities::instructions(&effective));
    if valid.is_err() {
        return "指令总量或启用数量超过限制；文件已保存，请先停用其他 Skill 再启用。".into();
    }
    state.enabled_skills = candidate.enabled_skills;
    format!("已在本对话启用 {name}，后续回复可按它的指令工作。")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::social::{ChatMessage, DecisionStatus, ParticipationMode};
    fn setup() -> (GroupState, SocialConfig, u64) {
        let mut state = GroupState::default();
        let config = SocialConfig {
            allowed_groups: vec!["test-group".into()],
            command_admins: vec!["owner".into()],
            ..SocialConfig::default()
        };
        let seq = state
            .ingest(
                ChatMessage {
                    id: "message".into(),
                    author_id: "member".into(),
                    text: "request".into(),
                    ..ChatMessage::default()
                },
                500,
            )
            .unwrap()
            .unwrap();
        (state, config, seq)
    }
    #[tokio::test]
    async fn unprivileged_install_is_rejected_before_network_or_file_access() {
        let (mut state, config, seq) = setup();
        assert!(
            handle(
                &mut state,
                seq,
                "/skill-install https://github.com/example/project/tree/main/skills/weather",
                &config,
                false,
                "test-group",
                0
            )
            .await
            .unwrap()
        );
        assert_eq!(state.record(seq).unwrap().status, DecisionStatus::Bypassed);
        assert!(
            state.requests.values().next().unwrap().bubbles[0]
                .text
                .contains("只有")
        );
        assert!(state.enabled_skills.is_empty());
    }
    #[tokio::test]
    async fn private_conversation_does_not_implicitly_grant_installation_permission() {
        let (mut state, config, seq) = setup();
        handle(
            &mut state,
            seq,
            "/skill on weather",
            &config,
            true,
            "test-group",
            0,
        )
        .await
        .unwrap();
        assert!(state.enabled_skills.is_empty());
    }
    #[tokio::test]
    async fn non_addressed_natural_install_does_not_bypass_mention_mode() {
        let (mut state, config, seq) = setup();
        assert!(
            !handle(
                &mut state,
                seq,
                "Codex 请安装这个skill https://github.com/example/project/tree/main/skills/weather",
                &config,
                false,
                "test-group",
                0
            )
            .await
            .unwrap()
        );
        assert_eq!(config.mode, ParticipationMode::Mentions);
        assert!(state.requests.is_empty());
    }
    #[tokio::test]
    async fn ordinary_discussion_never_becomes_a_control_action() {
        let (mut state, config, seq) = setup();
        assert!(
            !handle(
                &mut state,
                seq,
                "我昨天安装过这个skill https://github.com/example/project/tree/main/skills/weather",
                &config,
                true,
                "test-group",
                0
            )
            .await
            .unwrap()
        );
        assert!(state.requests.is_empty());
    }

    #[tokio::test]
    async fn helpful_mode_does_not_install_a_human_to_human_request() {
        let (mut state, mut config, seq) = setup();
        config.mode = ParticipationMode::Helpful;
        state.messages[0].message.author_id = "owner".into();
        assert!(
            !handle(
                &mut state,
                seq,
                "请安装这个 skill https://example.com/SKILL.md",
                &config,
                false,
                "test-group",
                0
            )
            .await
            .unwrap()
        );
        assert_eq!(state.record(seq).unwrap().status, DecisionStatus::Pending);
    }
}
