//! Command presentation only: no state changes, model calls or permission grants.
use super::{
    commands::HELP_TEXT,
    config::SocialConfig,
    control::{current_mode, mode_name},
};
use crate::model::social::{CommandButton, GroupState, ParticipationMode};

const MODEL_PAGE_SIZE: usize = 4;
const TOOL_PAGE_SIZE: usize = 6;

pub(super) struct MenuReply {
    pub text: String,
    pub buttons: Vec<CommandButton>,
}

impl MenuReply {
    pub fn new(text: String) -> Self {
        Self {
            text,
            buttons: Vec::new(),
        }
    }
    pub fn button(&mut self, label: &str, command: &str) {
        let label = label
            .split(|character: char| character.is_whitespace() || character.is_control())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let label = if label.chars().count() > 10 {
            format!("{}…", label.chars().take(9).collect::<String>())
        } else if label.is_empty() {
            "操作".into()
        } else {
            label
        };
        self.buttons.push(CommandButton {
            label,
            command: command.into(),
        });
    }
    pub fn navigation(&mut self) {
        self.button("返回帮助", "/help");
    }
}

pub(super) fn escaped(text: &str) -> String {
    let mut output = String::new();
    for character in text.chars() {
        if "\\`*_{}[]()#+-.!<>|".contains(character) {
            output.push('\\');
        }
        output.push(character);
    }
    output
}

fn title(text: &str, markdown: bool) -> String {
    if markdown {
        format!("## {}", escaped(text))
    } else {
        text.into()
    }
}

fn field(name: &str, value: &str, markdown: bool) -> String {
    if markdown {
        format!("**{}**：{}", escaped(name), escaped(value))
    } else {
        format!("{name}：{value}")
    }
}

fn code(text: &str, markdown: bool) -> String {
    if markdown {
        format!("**{}**", escaped(text))
    } else {
        text.into()
    }
}

pub(super) fn help(markdown: bool) -> MenuReply {
    let text = if markdown {
        HELP_TEXT.into()
    } else {
        HELP_TEXT
            .lines()
            .map(|line| line.trim_start_matches('#').trim_start().replace("**", ""))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut reply = MenuReply::new(text);
    for (label, command) in [
        ("状态", "/status"),
        ("连通测试", "/ping"),
        ("参与模式", "/mode"),
        ("模型选择", "/models"),
        ("技能", "/skills"),
        ("工具与权限", "/tools"),
        ("记忆", "/memory"),
        ("定时任务", "/task"),
    ] {
        reply.button(label, command);
    }
    reply
}

pub(super) fn modes(
    state: &GroupState,
    config: &SocialConfig,
    private: bool,
    may_configure: bool,
) -> MenuReply {
    let markdown = config.reply_format == "markdown";
    let mode = current_mode(state, config, private);
    let current = format!(
        "{}（{}）",
        config.behavior.profile(mode).name,
        mode_name(mode)
    );
    let mut reply = MenuReply::new(format!(
        "{}\n\n{}\n\n模式决定接话时机与频率；回答风格和详略仍按你的需求。",
        title("参与模式", markdown),
        field("当前模式", &current, markdown)
    ));
    if private {
        reply
            .text
            .push_str("\n\n私聊固定逐条回应，无需切换参与模式。");
    } else {
        for (mode, argument, description) in [
            (
                ParticipationMode::Mentions,
                "1",
                "仅在真正 @ 机器人时回应。",
            ),
            (
                ParticipationMode::Helpful,
                "2",
                "按需接话，关注对机器人的提问、续聊和求助。",
            ),
            (
                ParticipationMode::Social,
                "3",
                "更积极参与话题，也会避免无意义接话和刷屏。",
            ),
            (
                ParticipationMode::All,
                "all",
                "每条消息都回应，适合临时测试。",
            ),
        ] {
            let name = &config.behavior.profile(mode).name;
            let command = format!("/mode {argument}");
            reply.text.push_str(&format!(
                "\n\n{}\n{} {}",
                field(name, description, markdown),
                code(&command, markdown),
                if mode == current_mode(state, config, private) {
                    "（当前）"
                } else {
                    ""
                }
            ));
            if may_configure {
                reply.button(name, &command);
            }
        }
        if !may_configure {
            reply
                .text
                .push_str("\n\n群模式由已配置的群命令管理员切换。");
        }
    }
    reply.navigation();
    reply
}

pub(super) fn model_label(state: &GroupState, config: &SocialConfig) -> String {
    match state.model_alias.as_deref() {
        Some(alias) => match config.model_presets.get(alias) {
            Some(preset) => format!("{alias} · {} · {}", preset.model, preset.reasoning_effort),
            None => "原预设已不可用，请重新选择".into(),
        },
        None => format!("{} · {}", config.codex.model, config.codex.reasoning_effort),
    }
}

pub(super) fn models(
    state: &GroupState,
    config: &SocialConfig,
    may_configure: bool,
    page: usize,
    next: bool,
) -> MenuReply {
    let markdown = config.reply_format == "markdown";
    // Ordinary members can choose their own next model, never the group default.
    let next = next || !may_configure;
    let pages = config.model_presets.len().div_ceil(MODEL_PAGE_SIZE).max(1);
    let mut reply = MenuReply::new(format!(
        "{}\n\n{}\n\n{}",
        title(
            if next {
                "仅下一次使用的模型"
            } else {
                "默认模型选择"
            },
            markdown
        ),
        field("当前默认", &model_label(state, config), markdown),
        if next {
            "只影响你下一次新请求，10 分钟内有效；补充消息不消耗设置。"
        } else {
            "选择后会影响本对话后续的新请求。每个预设包含模型和思考强度。"
        }
    ));
    if page > pages {
        reply
            .text
            .push_str(&format!("\n\n页码不存在，共 {pages} 页。"));
        reply.button("返回第一页", if next { "/models next" } else { "/models" });
    } else if config.model_presets.is_empty() {
        reply
            .text
            .push_str("\n\n暂无可选模型预设，请在管理页面配置。");
    } else {
        reply.text.push_str(&format!("\n\n第 {page} / {pages} 页"));
        for (offset, (alias, preset)) in config
            .model_presets
            .iter()
            .skip((page - 1) * MODEL_PAGE_SIZE)
            .take(MODEL_PAGE_SIZE)
            .enumerate()
        {
            let command = format!("/model {}{alias}", if next { "next " } else { "" });
            let number = (page - 1) * MODEL_PAGE_SIZE + offset + 1;
            reply.text.push_str(&format!(
                "\n\n{}\n{}",
                field(&format!("预设 {number}"), alias, markdown),
                field(
                    "模型 / 思考强度",
                    &format!("{} / {}", preset.model, preset.reasoning_effort),
                    markdown
                )
            ));
            let selectable = matches!(super::commands::parse_command(&command), Some(super::commands::ChatCommand::Model(Some(ref selected)) | super::commands::ChatCommand::ModelNext(Some(ref selected))) if selected == alias);
            if selectable {
                reply
                    .text
                    .push_str(&format!("\n{}", code(&command, markdown)));
                reply.button(&format!("{number}·{alias}"), &command);
            } else {
                reply
                    .text
                    .push_str("\n此别名与命令保留字冲突，请在后台重命名后选择。");
            }
        }
        let selector = if next { "/models next" } else { "/models" };
        if page > 1 {
            reply.button("上一页", &format!("{selector} {}", page - 1));
        }
        if page < pages {
            reply.button("下一页", &format!("{selector} {}", page + 1));
        }
    }
    if next {
        reply.text.push_str(&format!(
            "\n\n{}：取消个人临时选择。",
            code("/model next off", markdown)
        ));
        reply.button("取消临时选择", "/model next off");
        if may_configure {
            reply.button("设置默认模型", "/models");
        }
    } else {
        reply.text.push_str(&format!(
            "\n\n{}：只为下一次请求选模型。",
            code("/models next", markdown)
        ));
        reply.button("仅下一次使用", "/models next");
    }
    reply.navigation();
    reply
}

pub(super) fn status(
    state: &GroupState,
    config: &SocialConfig,
    private: bool,
    author: &str,
    active: usize,
    now: i64,
) -> MenuReply {
    let markdown = config.reply_format == "markdown";
    let mode = current_mode(state, config, private);
    let text = [
        title("ClawMingle 状态", markdown),
        field("参与模式", &config.behavior.profile(mode).name, markdown),
        field("默认模型 / 思考强度", &model_label(state, config), markdown),
        field("活跃请求", &active.to_string(), markdown),
        field(
            "你的下一次模型",
            state
                .next_models
                .get(author)
                .map(|n| n.alias.as_str())
                .unwrap_or("未指定"),
            markdown,
        ),
    ]
    .join("\n\n");
    let mut reply = MenuReply::new(text);
    super::task_status::append_overview(&mut reply, state, author, markdown, now);
    for (label, command) in [
        ("参与模式", "/mode"),
        ("模型选择", "/models"),
        ("连通测试", "/ping"),
    ] {
        reply.button(label, command);
    }
    reply.navigation();
    reply
}

pub(super) fn tools(
    selected: &[String],
    markdown: bool,
    administrator: bool,
    page: usize,
) -> MenuReply {
    let mut reply = MenuReply::new(title("工具管理", markdown));
    let choices = [
        ("workspace_exec", "代码与文件"),
        ("time_now", "当前时间"),
        ("weather_current", "天气查询"),
        ("public_web_read", "网页读取"),
        ("message_lookup", "按 ID 查消息"),
        ("messages_list", "聊天记录"),
        ("qq_info", "QQ 会话资料"),
        ("task_update", "任务进展"),
        ("tasks_manage", "定时任务管理"),
        ("memory_manage", "对话记忆管理"),
        ("sticker_lookup", "表情包查询"),
        ("sticker_manage", "表情包管理"),
        ("group_documents", "群在线文档"),
        ("identity_manage", "本会话称呼"),
        ("group_profile_manage", "本群档案"),
    ];
    let pages = choices.len().div_ceil(TOOL_PAGE_SIZE);
    for (index, (name, label)) in choices.into_iter().enumerate() {
        let enabled = selected.iter().any(|tool| tool == name);
        let action = if enabled { "off" } else { "on" };
        let command = format!("/tool {action} {name}");
        reply.text.push_str(&format!(
            "\n\n{}\n{}",
            field(label, if enabled { "已启用" } else { "未启用" }, markdown),
            code(if administrator { &command } else { name }, markdown)
        ));
        if administrator && index / TOOL_PAGE_SIZE + 1 == page {
            reply.button(
                &format!("{} {label}", if enabled { "停用" } else { "启用" }),
                &command,
            );
        }
    }
    if page > pages {
        reply.text.push_str(&format!(
            "\n\n页码不存在，共 {pages} 页；/tools 返回第一页。"
        ));
        reply.button("返回第一页", "/tools");
    } else if administrator {
        reply
            .text
            .push_str(&format!("\n\n操作选项：第 {page} / {pages} 页。"));
        if page > 1 {
            let command = format!("/tools {}", page - 1);
            reply
                .text
                .push_str(&format!("\n{}：上一页。", code(&command, markdown)));
            reply.button("上一页", &command);
        }
        if page < pages {
            let command = format!("/tools {}", page + 1);
            reply
                .text
                .push_str(&format!("\n{}：下一页。", code(&command, markdown)));
            reply.button("下一页", &command);
        }
    }
    reply.text.push_str(&format!(
        "\n\n{}：查看浏览器和文档插件的独立授权。\n\n{}：管理员恢复跟随全局工具设置。",
        code("/capabilities", markdown),
        code("/tool default", markdown)
    ));
    reply.button("扩展能力", "/capabilities");
    reply.navigation();
    reply
}

/// Give secondary commands and result/error messages a consistent presentation.
/// No success is inferred from the command: the handler's actual result is kept.
pub(super) fn result(input: &str, text: String, markdown: bool) -> MenuReply {
    let mut input = input.trim();
    if input.starts_with("<@")
        && let Some(end) = input.find('>')
    {
        input = input[end + 1..].trim_start();
    }
    let command = input.split_whitespace().next().unwrap_or("");
    let (heading, shortcuts): (&str, &[(&str, &str)]) = match command {
        "/mode" => ("参与模式", &[("查看模式", "/mode")]),
        "/model" | "/models" | "/ask" => (
            "模型设置",
            &[("模型列表", "/models"), ("仅下一次使用", "/models next")],
        ),
        "/skills" | "/skill" | "/skill-install" => (
            "技能管理",
            &[("已启用技能", "/skills"), ("扩展能力", "/capabilities")],
        ),
        "/tools" | "/tool" => (
            "工具管理",
            &[("查看工具", "/tools"), ("扩展能力", "/capabilities")],
        ),
        "/capabilities" | "/capability" => (
            "扩展能力",
            &[("查看授权", "/capabilities"), ("查看工具", "/tools")],
        ),
        "/memory" => (
            "对话记忆",
            &[("我的记忆", "/memory list"), ("记忆帮助", "/memory")],
        ),
        "/task" => ("定时任务", &[("任务菜单", "/task")]),
        "/ping" => ("连通测试", &[("当前状态", "/status")]),
        "/status" => ("任务进展", &[("全部状态", "/status")]),
        "/cancel" | "/continue" | "/reset" => ("会话管理", &[("当前状态", "/status")]),
        _ => ("命令提示", &[]),
    };
    let text = if markdown {
        let body = text
            .lines()
            .map(|line| {
                if let Some((key, value)) = line.split_once('：')
                    && !key.is_empty()
                    && key.chars().count() <= 16
                    && !value.is_empty()
                {
                    format!("**{}**：{}", escaped(key), escaped(value))
                } else {
                    escaped(line)
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        format!("{}\n\n{body}", title(heading, true))
    } else {
        text
    };
    let text = if text.chars().count() > 3900 {
        let mut preview = text.chars().take(3800).collect::<String>();
        while preview.ends_with('\\') {
            preview.pop();
        }
        format!("{preview}…\n\n内容较长，请缩小查询范围或在管理页面查看完整记录。")
    } else {
        text
    };
    let mut reply = MenuReply::new(text);
    for (label, command) in shortcuts {
        reply.button(label, command);
    }
    reply.navigation();
    reply
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::social::{Bubble, ChatMessage},
        social::{commands::parse_command, control::handle_command},
    };
    fn config() -> SocialConfig {
        SocialConfig {
            reply_format: "markdown".into(),
            command_admins: vec!["admin".into()],
            ..Default::default()
        }
    }
    fn run(
        state: &mut GroupState,
        config: &SocialConfig,
        author: &str,
        command: &str,
        private: bool,
    ) -> Bubble {
        let seq = state
            .ingest(
                ChatMessage {
                    id: format!("test-{}", state.next_seq),
                    author_id: author.into(),
                    text: command.into(),
                    ..Default::default()
                },
                2000,
            )
            .unwrap()
            .unwrap();
        handle_command(
            state,
            seq,
            parse_command(command).unwrap(),
            config,
            private,
            "test-group",
            0,
        )
        .unwrap();
        state.requests[&state.record(seq).unwrap().applied_request.unwrap()].bubbles[0].clone()
    }
    #[test]
    fn mode_menu_persists_choices_and_stale_buttons_cannot_grant_permissions() {
        let config = config();
        let mut state = GroupState::default();
        let menu = run(&mut state, &config, "admin", "/mode", false);
        assert!(menu.markdown && menu.text.starts_with("## 参与模式"));
        assert_eq!(menu.command_buttons.len(), 5);
        assert!(state.mode_override.is_none());
        let loaded: GroupState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(
            loaded.requests[&1].bubbles[0].command_buttons,
            menu.command_buttons
        );
        let revoked = SocialConfig {
            command_admins: vec![],
            ..config.clone()
        };
        let result = run(&mut state, &revoked, "admin", "/mode 3", false);
        assert!(result.text.contains("只有群命令管理员"));
        assert!(state.mode_override.is_none());
        let member = run(&mut state, &config, "member", "/mode", false);
        assert!(
            member
                .command_buttons
                .iter()
                .all(|b| !b.command.starts_with("/mode "))
        );
        let private = run(&mut state, &config, "admin", "/mode", true);
        assert!(private.text.contains("私聊固定"));
        assert!(
            private
                .command_buttons
                .iter()
                .all(|b| !b.command.starts_with("/mode "))
        );
    }
    #[test]
    fn model_pages_keep_full_aliases_effort_and_personal_selection_scope() {
        let mut config = config();
        config.model_presets.clear();
        for index in 0..16 {
            config.model_presets.insert(
                format!("preset-{index:02}-long-alias"),
                super::super::config::ModelPreset {
                    model: "example/model-with-a-long-name".into(),
                    reasoning_effort: "high".into(),
                },
            );
        }
        let mut state = GroupState::default();
        for page in 1..=4 {
            let reply = run(
                &mut state,
                &config,
                "member",
                &format!("/models {page}"),
                false,
            );
            assert!(
                reply
                    .text
                    .contains("example/model\\-with\\-a\\-long\\-name / high")
            );
            let choices = reply
                .command_buttons
                .iter()
                .filter(|b| b.command.starts_with("/model next preset-"))
                .collect::<Vec<_>>();
            assert_eq!(choices.len(), 4);
            assert!(reply.command_buttons.len() <= 10);
            assert!(reply.text.chars().count() <= 4000);
            assert!(
                reply
                    .command_buttons
                    .iter()
                    .all(|b| b.label.chars().count() <= 10)
            );
            for choice in choices {
                assert!(matches!(
                    parse_command(&choice.command),
                    Some(super::super::commands::ChatCommand::ModelNext(Some(_)))
                ));
            }
            assert!(reply.command_buttons.iter().all(|b| b.command != "/models"));
        }
        assert!(state.model_alias.is_none());
        assert!(state.next_models.is_empty());
        let last = models(&state, &config, true, usize::MAX, false);
        assert!(last.text.contains("页码不存在"));
        let first = run(&mut state, &config, "admin", "/model", false);
        assert!(
            first
                .command_buttons
                .iter()
                .any(|b| b.command == "/model preset-00-long-alias")
        );
        let next = run(&mut state, &config, "member", "/model next", false);
        assert!(next.text.contains("10 分钟"));
        run(
            &mut state,
            &config,
            "member",
            "/model next preset-00-long-alias",
            false,
        );
        assert_eq!(state.next_models["member"].alias, "preset-00-long-alias");
        assert!(state.model_alias.is_none());
    }
    #[test]
    fn plain_text_keeps_manual_choices_without_markdown_or_keyboard() {
        let config = SocialConfig {
            reply_format: "text".into(),
            ..config()
        };
        for command in ["/help", "/status", "/mode", "/models", "/model next"] {
            let mut state = GroupState::default();
            let reply = run(&mut state, &config, "admin", command, false);
            assert!(!reply.markdown);
            assert!(reply.command_buttons.is_empty());
            assert!(!reply.text.contains('`'));
            assert!(!reply.text.contains("**"));
            if command == "/mode" {
                assert!(reply.text.contains("/mode 3"));
            }
        }
    }
    #[test]
    fn legacy_bubbles_default_to_no_custom_buttons() {
        let bubble: Bubble =
            serde_json::from_value(serde_json::json!({"text":"legacy", "help_buttons":true}))
                .unwrap();
        assert!(bubble.command_buttons.is_empty());
        assert!(bubble.help_buttons);
    }
    #[test]
    fn reserved_model_aliases_do_not_create_misleading_selection_buttons() {
        let mut config = config();
        let preset = config.model_presets.values().next().unwrap().clone();
        config.model_presets.clear();
        for alias in ["next", "off", "once"] {
            config.model_presets.insert(alias.into(), preset.clone());
        }
        let default = models(&GroupState::default(), &config, true, 1, false);
        assert!(
            default
                .buttons
                .iter()
                .all(|button| !button.label.contains("next") && !button.label.contains("once"))
        );
        let next = models(&GroupState::default(), &config, true, 1, true);
        let off = next
            .buttons
            .iter()
            .filter(|button| button.command == "/model next off")
            .collect::<Vec<_>>();
        assert_eq!(off.len(), 1);
        assert_eq!(off[0].label, "取消临时选择");
        assert!(next.text.contains("保留字冲突"));
    }

    #[test]
    fn empty_models_and_bounded_secondary_output_are_readable() {
        let config = SocialConfig {
            model_presets: Default::default(),
            ..config()
        };
        let reply = models(&GroupState::default(), &config, true, 1, false);
        assert!(reply.text.contains("暂无"));
        let reply = result("/memory list", "*记录*".repeat(1000), true);
        assert!(reply.text.chars().count() < 4000);
        assert!(reply.text.contains("完整记录"));
        let reply = result("/model unknown", "未知模型别名".into(), true);
        assert!(reply.text.contains("未知模型"));
        assert!(!reply.text.contains("成功"));
    }
    #[test]
    fn tool_choices_show_only_authorized_toggles_and_fit_one_keyboard() {
        let reply = tools(&["time_now".into()], true, true, 1);
        assert!(reply.buttons.len() <= 10);
        assert!(
            reply
                .buttons
                .iter()
                .any(|b| b.command == "/tool off time_now")
        );
        assert!(
            reply
                .buttons
                .iter()
                .any(|b| b.command == "/tool on workspace_exec")
        );
        assert!(
            tools(&[], true, false, 1)
                .buttons
                .iter()
                .all(|b| !b.command.starts_with("/tool "))
        );
        crate::qq::message_content::command_keyboard(&reply.buttons).unwrap();
    }

    #[test]
    fn tool_pages_cover_every_configurable_tool_and_plain_text_keeps_all_statuses() {
        let selected = vec!["memory_manage".into(), "sticker_lookup".into()];
        let expected = crate::codex::app_server::SocialTurnInput::TOOL_NAMES
            .iter()
            .filter(|name| {
                !crate::codex::app_server::scoped_integrations::TOOL_NAMES.contains(name)
            })
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let mut offered = std::collections::BTreeSet::new();
        for page in 1..=expected.len().div_ceil(TOOL_PAGE_SIZE) {
            let reply = tools(&selected, false, true, page);
            for name in &expected {
                let enabled = selected.iter().any(|tool| tool.as_str() == *name);
                let command = format!("/tool {} {name}", if enabled { "off" } else { "on" });
                assert!(reply.text.contains(&command), "missing tool {name}");
            }
            assert!(reply.text.contains("对话记忆管理：已启用"));
            assert!(reply.text.contains("表情包管理：未启用"));
            assert!(reply.buttons.len() <= 10);
            crate::qq::message_content::command_keyboard(&reply.buttons).unwrap();
            for button in reply.buttons {
                if let Some(command) = button.command.strip_prefix("/tool ") {
                    offered.insert(command.split_whitespace().nth(1).unwrap().to_owned());
                }
            }
        }
        assert_eq!(
            offered
                .iter()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            expected
        );
        let first = tools(&selected, true, true, 1);
        let second = tools(&selected, true, true, 2);
        assert!(
            first
                .buttons
                .iter()
                .any(|button| button.command == "/tools 2")
        );
        assert!(
            second
                .buttons
                .iter()
                .any(|button| button.command == "/tools 1")
        );
        assert!(
            tools(&selected, false, false, 1)
                .text
                .contains("memory_manage")
        );
        let invalid = tools(&selected, false, true, usize::MAX);
        assert!(invalid.text.contains("页码不存在"));
        assert!(
            invalid
                .buttons
                .iter()
                .all(|button| !button.command.starts_with("/tool "))
        );
    }

    #[test]
    fn button_labels_normalize_configured_whitespace_without_changing_commands() {
        let mut reply = MenuReply::new("扩展能力".into());
        let command = "/capability on documents";
        reply.button("授权 文档\n\t插件\u{0007}", command);
        reply.button("\n\t\u{0000}", command);
        reply.button(&format!("授权\n{}", "文档".repeat(12)), command);
        assert_eq!(reply.buttons[0].label, "授权 文档 插件");
        assert_eq!(reply.buttons[1].label, "操作");
        assert!(reply.buttons.iter().all(|button| button.command == command));
        assert!(
            reply
                .buttons
                .iter()
                .all(|button| button.label.chars().count() <= 10)
        );
        crate::qq::message_content::command_keyboard(&reply.buttons).unwrap();
    }
}
