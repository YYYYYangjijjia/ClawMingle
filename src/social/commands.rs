//! Pure chat-command parsing. Execution, model lookup, and authorization belong to callers.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatCommand {
    Help,
    Status,
    StatusRequest(u64),
    StatusCode(String),
    Mode(Option<String>),
    Models,
    ModelMenu { page: usize, next: bool },
    Model(Option<String>),
    ModelNext(Option<String>),
    Ask { model: String, prompt: String },
    Ping,
    Cancel(Option<u64>),
    CancelCode(String),
    Reset,
    Invalid(&'static str),
}

pub const HELP_TEXT: &str = "## ClawMingle 命令菜单

**状态与对话**
**/status** 当前状态与任务进度 · **/status 任务编号** 查看任务详情
**/ping** 连通测试
**/mode** 参与模式 · **/models** 模型与思考强度
**/cancel [任务编号]** 取消请求
**/continue [任务编号]** 取回保留结果，不重复执行
**/reset** 开始新会话，保留历史

**模型选择**
**/model ALIAS** 设为本对话默认
**/model next** 打开仅下一次使用的模型菜单
**/model next ALIAS** 个人下一次新请求使用，10 分钟有效
**/model next off** 取消临时选择
**/ask ALIAS 问题** 本次指定模型，可多行

**技能、记忆与任务**
**/skills** 已启用技能 · **/tools** 工具
**/capabilities** 浏览器和文档插件授权
**/memory** 记忆菜单 · **/task** 定时任务与主动推送

**管理操作**
**/mode 1、2、3、all** 切换群参与模式
**/skill on|off NAME** 启停已安装技能
**/skill-install GitHub链接** 安装技能
**/tool on|off 工具名** 切换工具
**/capability on|off browser或插件ID** 调整授权

群模式、群默认模型和重置由群命令管理员修改；私聊只影响自己。工具、技能和扩展授权仍需配置的管理员身份。

点击下方按钮会填入命令，检查后发送即可。也可手动输入上述命令；**/help** 返回此菜单。";

pub const COMMAND_CATALOG: [(&str, &str); 19] = [
    ("/help", "查看命令帮助"),
    ("/status", "查看当前状态"),
    ("/mode", "查看或切换参与模式"),
    ("/models", "查看可用模型"),
    ("/model", "查看或选择默认、下一次模型"),
    ("/ask", "本次使用指定模型提问"),
    ("/ping", "测试连接"),
    ("/cancel", "取消当前或指定请求"),
    ("/continue", "取回已完成但尚未送达的结果"),
    ("/reset", "重置当前群的会话上下文"),
    ("/skills", "查看本对话启用的技能"),
    ("/skill", "管理员启停已安装技能"),
    ("/skill-install", "管理员从GitHub安装文字技能"),
    ("/memory", "提交确认及查看自己的群记忆"),
    ("/tools", "查看可用工具与当前启用工具"),
    ("/capabilities", "查看本对话公共浏览器与文档插件授权"),
    ("/capability", "管理员授权或撤销本对话扩展能力"),
    ("/tool", "管理员切换本对话工具"),
    ("/task", "当前对话定时提醒、Agent任务、主动推送与运行状态"),
];

pub fn catalog() -> &'static [(&'static str, &'static str)] {
    &COMMAND_CATALOG
}

const INVALID_FORMAT: &str = "命令格式有误，请发送 /help 查看用法。";
const UNKNOWN_COMMAND: &str = "未知命令，请发送 /help 查看用法。";

/// Only the first non-whitespace character can introduce a command.
/// Non-`ask` commands must occupy one line. `ask` trims outer prompt whitespace
/// while preserving all internal whitespace, including newlines and indentation.
pub fn parse_command(text: &str) -> Option<ChatCommand> {
    let text = text.trim_start();
    if !text.starts_with('/') {
        return None;
    }
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    let name = &text[..end];
    let arguments = &text[end..];
    if name == "/ask" {
        return Some(parse_ask(arguments));
    }
    if text.chars().any(is_line_break) {
        return Some(ChatCommand::Invalid(INVALID_FORMAT));
    }
    let arguments = arguments.split_whitespace().collect::<Vec<_>>();
    let command = match (name, arguments.as_slice()) {
        ("/help", []) => ChatCommand::Help,
        ("/status", []) => ChatCommand::Status,
        ("/status", [request]) => match request_id(request) {
            Some(id) => ChatCommand::StatusRequest(id),
            None if super::task_identity::valid_code(request) => {
                ChatCommand::StatusCode((*request).into())
            }
            None => ChatCommand::Invalid(INVALID_FORMAT),
        },
        ("/mode", []) => ChatCommand::Mode(None),
        ("/mode", [mode]) => match *mode {
            "1" | "mode-1" => ChatCommand::Mode(Some("mode-1".into())),
            "2" | "mode-2" => ChatCommand::Mode(Some("mode-2".into())),
            "3" | "mode-3" => ChatCommand::Mode(Some("mode-3".into())),
            "all" | "mode-all" => ChatCommand::Mode(Some("mode-all".into())),
            _ => ChatCommand::Invalid(INVALID_FORMAT),
        },
        ("/models", []) => ChatCommand::Models,
        ("/models", ["next"]) | ("/model", ["next" | "once"]) => ChatCommand::ModelMenu {
            page: 1,
            next: true,
        },
        ("/models", [page]) => model_page(page, false),
        ("/models", ["next", page]) => model_page(page, true),
        ("/model", []) => ChatCommand::Model(None),
        ("/model", ["next" | "once", "off"]) => ChatCommand::ModelNext(None),
        ("/model", ["next" | "once", model]) if valid_alias(model) => {
            ChatCommand::ModelNext(Some((*model).into()))
        }
        ("/model", [model]) if !matches!(*model, "next" | "once") && valid_alias(model) => {
            ChatCommand::Model(Some((*model).into()))
        }
        ("/ping", []) => ChatCommand::Ping,
        ("/cancel", []) => ChatCommand::Cancel(None),
        ("/cancel", [request]) => match request_id(request) {
            Some(id) => ChatCommand::Cancel(Some(id)),
            None if super::task_identity::valid_code(request) => {
                ChatCommand::CancelCode((*request).into())
            }
            None => ChatCommand::Invalid(INVALID_FORMAT),
        },
        ("/reset", []) => ChatCommand::Reset,
        (
            "/help" | "/status" | "/mode" | "/models" | "/model" | "/ping" | "/cancel" | "/reset",
            _,
        ) => ChatCommand::Invalid(INVALID_FORMAT),
        _ => ChatCommand::Invalid(UNKNOWN_COMMAND),
    };
    Some(command)
}

fn model_page(value: &str, next: bool) -> ChatCommand {
    match value.parse::<usize>() {
        Ok(page) if page > 0 => ChatCommand::ModelMenu { page, next },
        _ => ChatCommand::Invalid(INVALID_FORMAT),
    }
}

fn parse_ask(arguments: &str) -> ChatCommand {
    let model_start = arguments
        .find(|character: char| !character.is_whitespace())
        .unwrap_or(arguments.len());
    // The model selector must stay on the command's line; only the body may span lines.
    if arguments[..model_start].chars().any(is_line_break) {
        return ChatCommand::Invalid(INVALID_FORMAT);
    }
    let arguments = &arguments[model_start..];
    let Some(model_end) = arguments.find(char::is_whitespace) else {
        return ChatCommand::Invalid(INVALID_FORMAT);
    };
    let model = &arguments[..model_end];
    let prompt = arguments[model_end..].trim();
    if !valid_alias(model) || prompt.is_empty() {
        return ChatCommand::Invalid(INVALID_FORMAT);
    }
    ChatCommand::Ask {
        model: model.into(),
        prompt: prompt.into(),
    }
}

fn valid_alias(alias: &str) -> bool {
    (1..=32).contains(&alias.len())
        && alias
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn request_id(value: &str) -> Option<u64> {
    let digits = value.strip_prefix('R')?;
    if digits.starts_with('0') || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|id| *id > 0)
}

fn is_line_break(character: char) -> bool {
    matches!(
        character,
        '\n' | '\r' | '\u{000b}' | '\u{000c}' | '\u{0085}' | '\u{2028}' | '\u{2029}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_invalid(text: &str) {
        assert!(
            matches!(parse_command(text), Some(ChatCommand::Invalid(_))),
            "unexpected valid command: {text:?}"
        );
    }

    #[test]
    fn task_status_requires_one_complete_request_identifier() {
        assert_eq!(
            parse_command("/status 20261025-143205"),
            Some(ChatCommand::StatusCode("20261025-143205".into()))
        );
        assert_eq!(
            parse_command("/cancel 20261025-143205-94"),
            Some(ChatCommand::CancelCode("20261025-143205-94".into()))
        );
        assert_eq!(
            parse_command("/status R123"),
            Some(ChatCommand::StatusRequest(123))
        );
        for text in [
            "/status 123",
            "/status R0",
            "/status R01",
            "/status R-1",
            "/status R1 extra",
            "/status R1\n/continue",
        ] {
            assert_invalid(text);
        }
    }

    #[test]
    fn model_menu_pages_and_next_shortcut_are_read_only_commands() {
        for (input, page, next) in [
            ("/models 2", 2, false),
            ("/models next 3", 3, true),
            ("/models next", 1, true),
            ("/model next", 1, true),
            ("/model once", 1, true),
        ] {
            assert_eq!(
                parse_command(input),
                Some(ChatCommand::ModelMenu { page, next })
            );
        }
        for input in [
            "/models 0",
            "/models -1",
            "/models 184467440737095516160",
            "/models next 0",
            "/models 1 extra",
        ] {
            assert_invalid(input);
        }
    }

    #[test]
    fn plain_chat_and_embedded_commands_are_not_commands() {
        for text in ["", " \t", "你好", "请执行 /help", "问题\n/reset", "／help"] {
            assert_eq!(parse_command(text), None);
        }
    }

    #[test]
    fn leading_unicode_whitespace_is_supported() {
        assert_eq!(
            parse_command("\n\u{3000}\u{2003}\t/help\u{00a0}"),
            Some(ChatCommand::Help)
        );
        assert_eq!(
            parse_command("/mode\u{3000}2\u{2003}"),
            Some(ChatCommand::Mode(Some("mode-2".into())))
        );
    }

    #[test]
    fn all_argument_free_commands_are_recognized() {
        for (text, expected) in [
            ("/help", ChatCommand::Help),
            ("/status", ChatCommand::Status),
            ("/mode", ChatCommand::Mode(None)),
            ("/models", ChatCommand::Models),
            ("/model", ChatCommand::Model(None)),
            ("/ping", ChatCommand::Ping),
            ("/cancel", ChatCommand::Cancel(None)),
            ("/reset", ChatCommand::Reset),
        ] {
            assert_eq!(parse_command(text), Some(expected));
        }
    }

    #[test]
    fn unknown_commands_use_a_fixed_message() {
        for text in ["/unknown", "/", "/help@bot", "/HELP", "/helpful", "//help"] {
            assert_eq!(
                parse_command(text),
                Some(ChatCommand::Invalid(UNKNOWN_COMMAND))
            );
        }
    }

    #[test]
    fn participation_modes_are_normalized() {
        for (input, expected) in [
            ("1", "mode-1"),
            ("mode-1", "mode-1"),
            ("2", "mode-2"),
            ("mode-2", "mode-2"),
            ("3", "mode-3"),
            ("mode-3", "mode-3"),
            ("all", "mode-all"),
            ("mode-all", "mode-all"),
        ] {
            assert_eq!(
                parse_command(&format!("/mode {input}")),
                Some(ChatCommand::Mode(Some(expected.into())))
            );
        }
    }

    #[test]
    fn invalid_modes_and_extra_mode_arguments_are_rejected() {
        for text in [
            "/mode 0",
            "/mode 4",
            "/mode ALL",
            "/mode 1 all",
            "/mode mode--1",
        ] {
            assert_invalid(text);
        }
    }

    #[test]
    fn default_model_selection_preserves_alias() {
        assert_eq!(
            parse_command("/model Fast_2-beta"),
            Some(ChatCommand::Model(Some("Fast_2-beta".into())))
        );
    }

    #[test]
    fn next_and_once_models_have_the_same_scope() {
        for selector in ["next", "once"] {
            assert_eq!(
                parse_command(&format!("/model {selector} fast")),
                Some(ChatCommand::ModelNext(Some("fast".into())))
            );
            assert_eq!(
                parse_command(&format!("/model {selector} off")),
                Some(ChatCommand::ModelNext(None))
            );
        }
    }

    #[test]
    fn next_model_rejects_extra_arguments() {
        for text in [
            "/model next fast extra",
            "/model once fast extra",
            "/model next off extra",
            "/model fast extra",
        ] {
            assert_invalid(text);
        }
    }

    #[test]
    fn ask_preserves_multiline_body_and_indentation() {
        assert_eq!(
            parse_command("/ask fast \t第一行\n  第二行\r\n第三行  "),
            Some(ChatCommand::Ask {
                model: "fast".into(),
                prompt: "第一行\n  第二行\r\n第三行".into(),
            })
        );
    }

    #[test]
    fn ask_can_start_the_body_on_the_following_line() {
        assert_eq!(
            parse_command("/ask fast\n问题正文"),
            Some(ChatCommand::Ask {
                model: "fast".into(),
                prompt: "问题正文".into(),
            })
        );
    }

    #[test]
    fn commands_inside_ask_are_body_text() {
        assert_eq!(
            parse_command("/ask fast 帮我解释\n/reset\n/model slow"),
            Some(ChatCommand::Ask {
                model: "fast".into(),
                prompt: "帮我解释\n/reset\n/model slow".into(),
            })
        );
    }

    #[test]
    fn ask_requires_a_model_and_nonempty_prompt() {
        for text in ["/ask", "/ask fast", "/ask fast \n\t", "/ask\nfast 问题"] {
            assert_invalid(text);
        }
    }

    #[test]
    fn aliases_are_ascii_and_cannot_inject_paths_or_shell_syntax() {
        for alias in [
            "../fast",
            "fast/model",
            "fast.model",
            "中文",
            "a;id",
            "$(id)",
            "`id`",
            "a=b",
            "a\0b",
            "fast\u{200b}",
            "@fast",
        ] {
            assert_invalid(&format!("/model {alias}"));
            assert_invalid(&format!("/model next {alias}"));
            assert_invalid(&format!("/ask {alias} 问题"));
        }
    }

    #[test]
    fn alias_length_limit_applies_to_every_model_command() {
        let allowed = "a".repeat(32);
        let excessive = "a".repeat(33);
        assert_eq!(
            parse_command(&format!("/model {allowed}")),
            Some(ChatCommand::Model(Some(allowed.clone())))
        );
        assert_eq!(
            parse_command(&format!("/ask {allowed} 问题")),
            Some(ChatCommand::Ask {
                model: allowed,
                prompt: "问题".into(),
            })
        );
        for text in [
            format!("/model {excessive}"),
            format!("/model next {excessive}"),
            format!("/ask {excessive} 问题"),
        ] {
            assert_invalid(&text);
        }
    }

    #[test]
    fn simple_commands_reject_extra_arguments() {
        for text in [
            "/help more",
            "/status more",
            "/models more",
            "/ping more",
            "/reset more",
            "/cancel R1 more",
        ] {
            assert_invalid(text);
        }
    }

    #[test]
    fn non_ask_commands_reject_all_line_break_separators() {
        for separator in [
            '\n', '\r', '\u{000b}', '\u{000c}', '\u{0085}', '\u{2028}', '\u{2029}',
        ] {
            assert_invalid(&format!("/model{separator}fast"));
            assert_invalid(&format!("/ping{separator}/reset"));
            assert_invalid(&format!("/ping{separator}"));
            assert_invalid(&format!("/ask{separator}fast 问题"));
        }
    }

    #[test]
    fn cancel_accepts_positive_canonical_request_ids() {
        assert_eq!(
            parse_command("/cancel R1"),
            Some(ChatCommand::Cancel(Some(1)))
        );
        assert_eq!(
            parse_command("/cancel R42"),
            Some(ChatCommand::Cancel(Some(42)))
        );
        assert_eq!(
            parse_command("/cancel R18446744073709551615"),
            Some(ChatCommand::Cancel(Some(u64::MAX)))
        );
    }

    #[test]
    fn cancel_rejects_zero_signs_noncanonical_ids_and_overflow() {
        for text in [
            "/cancel R0",
            "/cancel R01",
            "/cancel R",
            "/cancel 1",
            "/cancel r1",
            "/cancel R-1",
            "/cancel R+1",
            "/cancel R1.0",
            "/cancel R１",
            "/cancel R18446744073709551616",
        ] {
            assert_invalid(text);
        }
    }

    #[test]
    fn help_is_bounded_and_catalog_commands_are_distinct() {
        assert!(HELP_TEXT.chars().count() <= 900);
        assert_eq!(catalog().len(), 19);
        for (index, (name, description)) in catalog().iter().enumerate() {
            assert!(!description.is_empty());
            assert!(HELP_TEXT.contains(name));
            assert!(
                catalog()[..index]
                    .iter()
                    .all(|(previous, _)| previous != name)
            );
            assert!(parse_command(name).is_some_and(|command| !matches!(
                command,
                ChatCommand::Invalid(_)
            ) || matches!(
                *name,
                "/ask"
                    | "/skills"
                    | "/skill"
                    | "/skill-install"
                    | "/memory"
                    | "/tools"
                    | "/tool"
                    | "/task"
                    | "/continue"
                    | "/capabilities"
                    | "/capability"
            )));
        }
    }
}
