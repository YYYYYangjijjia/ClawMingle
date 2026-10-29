use super::{
    SocialConfig,
    control::{control_reply, effective_config},
};
use crate::{codex::app_server::SocialTurnInput, model::social::GroupState};
use anyhow::Result;

pub fn handle(
    state: &mut GroupState,
    seq: u64,
    text: &str,
    config: &SocialConfig,
    private: bool,
    target: &str,
    now: i64,
) -> Result<bool> {
    let args = text.split_whitespace().collect::<Vec<_>>();
    if !matches!(args.first(), Some(&"/tools" | &"/tool")) {
        return Ok(false);
    }
    let mut selected = effective_config(config, state, None)?.enabled_tools;
    let page = match args.as_slice() {
        ["/tools"] => Some(1),
        ["/tools", page] => page.parse::<usize>().ok().filter(|page| *page > 0),
        _ => None,
    };
    if let Some(page) = page {
        let administrator =
            config.is_scope_admin(&state.record(seq)?.message.author_id, private, target);
        let menu = super::command_menu::tools(
            &selected,
            config.reply_format == "markdown",
            administrator,
            page,
        );
        state.begin_decision(seq)?;
        super::control::control_menu_reply(state, seq, menu, config, now)?;
        return Ok(true);
    }
    let reply = match args.as_slice() {
        ["/tool", ..]
            if !config.is_scope_admin(&state.record(seq)?.message.author_id, private, target) =>
        {
            "只有已配置的群命令管理员可以调整工具，私聊也需要对应身份授权。".into()
        }
        ["/tool", _, name]
            if crate::codex::app_server::scoped_integrations::TOOL_NAMES.contains(name) =>
        {
            "扩展能力需要独立授权，请用 /capabilities 查看，并用 /capability on|off browser 或插件ID 调整。".into()
        }
        ["/tool", "default"] => {
            state.tools_override = None;
            "已恢复跟随全局工具设置；下次回复生效。".into()
        }
        ["/tool", action, name]
            if matches!(*action, "on" | "off") && SocialTurnInput::TOOL_NAMES.contains(name) =>
        {
            selected.retain(|tool| tool != name);
            if *action == "on" {
                selected.push((*name).into())
            }
            selected.sort();
            state.tools_override = Some(selected);
            "已更新本对话工具；下次回复生效。".into()
        }
        _ => "格式：/tools [页码]、/tool on 工具名、/tool off 工具名、/tool default。".into(),
    };
    state.begin_decision(seq)?;
    control_reply(state, seq, reply, config, now)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paged_tool_commands_keep_authorization_and_plain_text_statuses() {
        fn menu(
            command: &str,
            author: &str,
            private: bool,
            markdown: bool,
        ) -> crate::model::social::Bubble {
            let config = SocialConfig {
                command_admins: vec!["admin".into()],
                reply_format: if markdown { "markdown" } else { "text" }.into(),
                ..SocialConfig::default()
            };
            let mut state = GroupState::default();
            let seq = state
                .ingest(
                    crate::model::social::ChatMessage {
                        id: "menu-request".into(),
                        author_id: author.into(),
                        text: command.into(),
                        ..Default::default()
                    },
                    500,
                )
                .unwrap()
                .unwrap();
            assert!(handle(&mut state, seq, command, &config, private, "test-group", 0).unwrap());
            assert!(state.tools_override.is_none());
            state.requests.values().next().unwrap().bubbles[0].clone()
        }
        let second = menu("/tools 2", "admin", false, true);
        for name in [
            "tasks_manage",
            "memory_manage",
            "sticker_lookup",
            "sticker_manage",
        ] {
            assert!(
                second
                    .command_buttons
                    .iter()
                    .any(|button| button.command.ends_with(name))
            );
        }
        assert!(second.command_buttons.len() <= 10);
        for private in [false, true] {
            assert!(
                menu("/tools 2", "member", private, true)
                    .command_buttons
                    .iter()
                    .all(|button| !button.command.starts_with("/tool "))
            );
        }
        let plain = menu("/tools", "admin", false, false);
        assert!(plain.command_buttons.is_empty());
        for name in [
            "workspace_exec",
            "tasks_manage",
            "memory_manage",
            "sticker_lookup",
            "sticker_manage",
        ] {
            assert!(plain.text.contains(name));
        }
        for invalid in [
            "/tools 0",
            "/tools -1",
            "/tools 2 extra",
            "/tools 184467440737095516160",
        ] {
            assert!(menu(invalid, "admin", false, true).text.contains("格式"));
        }
    }

    #[test]
    fn tool_changes_require_platform_identity_and_only_affect_current_scope() {
        let config = SocialConfig {
            command_admins: vec!["admin".into()],
            ..SocialConfig::default()
        };
        let mut state = GroupState::default();
        let seq = state
            .ingest(
                crate::model::social::ChatMessage {
                    id: "m".into(),
                    author_id: "member".into(),
                    ..Default::default()
                },
                500,
            )
            .unwrap()
            .unwrap();
        handle(
            &mut state,
            seq,
            "/tool off weather_current",
            &config,
            false,
            "test-group",
            0,
        )
        .unwrap();
        assert!(state.tools_override.is_none());
        let seq = state
            .ingest(
                crate::model::social::ChatMessage {
                    id: "m2".into(),
                    author_id: "admin".into(),
                    ..Default::default()
                },
                500,
            )
            .unwrap()
            .unwrap();
        handle(
            &mut state,
            seq,
            "/tool off weather_current",
            &config,
            false,
            "test-group",
            0,
        )
        .unwrap();
        assert!(
            !state
                .tools_override
                .as_ref()
                .unwrap()
                .iter()
                .any(|s| s == "weather_current")
        );
        assert!(config.enabled_tools.iter().any(|s| s == "weather_current"));
    }
}
