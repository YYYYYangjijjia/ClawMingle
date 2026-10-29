//! Pure payload builders for passive QQ group and private messages.
//!
//! These helpers do not allocate a sequence, send a request, retry, or decide
//! whether an unsuccessful request is safe to repeat. The delivery owner must
//! persist and reuse the supplied `msg_seq` for the same logical bubble.

use crate::model::social::CommandButton;
use std::fmt;

use serde_json::{Value, json};

pub const MAX_TEXT_CHARS: usize = 4000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutboundStyle {
    #[default]
    Text,
    Markdown,
}

#[derive(Debug, Clone, Copy)]
pub struct PassiveMessage<'a> {
    pub text: &'a str,
    pub style: OutboundStyle,
    pub help_buttons: bool,
    pub command_buttons: &'a [CommandButton],
    pub reference_idx: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageScope {
    Group,
    Private,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageContentError {
    EmptyText,
    TextContainsNul,
    TextTooLong { actual: usize },
    InvalidPassiveMessageId,
    InvalidSequence,
    InvalidCommandButtons,
}

impl fmt::Display for MessageContentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyText => f.write_str("message text must not be blank"),
            Self::TextContainsNul => f.write_str("message text must not contain NUL"),
            Self::TextTooLong { actual } => {
                write!(
                    f,
                    "message text has {actual} characters; limit is {MAX_TEXT_CHARS}"
                )
            }
            Self::InvalidPassiveMessageId => {
                f.write_str("a nonblank passive message ID without NUL is required")
            }
            Self::InvalidCommandButtons => f.write_str("invalid command menu buttons"),
            Self::InvalidSequence => f.write_str("message sequence must be greater than zero"),
        }
    }
}

impl std::error::Error for MessageContentError {}

/// Build a strict plain-text payload without changing the supplied content.
/// Existing legacy text senders may retain their separate compatibility limit.
pub fn text_body(
    text: &str,
    passive_msg_id: &str,
    msg_seq: u32,
) -> Result<Value, MessageContentError> {
    validate(text, passive_msg_id, msg_seq)?;
    Ok(json!({
        "content": text,
        "msg_type": 0,
        "msg_id": passive_msg_id,
        "msg_seq": msg_seq,
    }))
}

/// Build custom Markdown, optionally with four fixed read-only command buttons.
/// Buttons only fill the input field in both scopes; they never invoke callbacks
/// or automatically submit a command. QQ still controls Markdown availability.
pub fn markdown_body(
    text: &str,
    passive_msg_id: &str,
    msg_seq: u32,
    scope: MessageScope,
    help_buttons: bool,
) -> Result<Value, MessageContentError> {
    validate(text, passive_msg_id, msg_seq)?;
    let mut body = proactive_markdown_body(text, msg_seq, scope, help_buttons)?;
    body["msg_id"] = json!(passive_msg_id);
    Ok(body)
}

/// Same rendering without borrowing a user's passive reply window.
pub fn proactive_markdown_body(
    text: &str,
    msg_seq: u32,
    scope: MessageScope,
    help_buttons: bool,
) -> Result<Value, MessageContentError> {
    validate_content(text, msg_seq)?;
    let mut body = json!({
        "msg_type": 2,
        "msg_seq": msg_seq,
        "markdown": { "content": text },
    });
    if help_buttons {
        body["keyboard"] = help_keyboard(scope);
    }
    Ok(body)
}

/// Build a bounded keyboard from host-created command choices. Two columns
/// stay readable on mobile. Permissions are rechecked when the user sends it.
pub fn command_keyboard(buttons: &[CommandButton]) -> Result<Value, MessageContentError> {
    if buttons.is_empty()
        || buttons.len() > 10
        || buttons.iter().any(|button| {
            button.label.trim().is_empty()
                || button.label.chars().count() > 10
                || button.label.chars().any(char::is_control)
                || !button.command.starts_with('/')
                || button.command.len() > 256
                || button.command.chars().any(char::is_control)
        })
    {
        return Err(MessageContentError::InvalidCommandButtons);
    }
    let rows = buttons
        .chunks(2)
        .enumerate()
        .map(|(row, buttons)| {
            let buttons = buttons
                .iter()
                .enumerate()
                .map(|(column, button)| {
                    json!({
                        "id": format!("menu-{}", row * 2 + column),
                        "render_data": {"label": button.label, "style": 1},
                        "action": {"type": 2, "permission": {"type": 2},
                            "data": button.command, "enter": false},
                    })
                })
                .collect::<Vec<_>>();
            json!({"buttons": buttons})
        })
        .collect::<Vec<_>>();
    Ok(json!({"content": {"rows": rows}}))
}

fn validate(text: &str, passive_msg_id: &str, msg_seq: u32) -> Result<(), MessageContentError> {
    if passive_msg_id.trim().is_empty() || passive_msg_id.contains('\0') {
        return Err(MessageContentError::InvalidPassiveMessageId);
    }
    validate_content(text, msg_seq)
}

fn validate_content(text: &str, msg_seq: u32) -> Result<(), MessageContentError> {
    if text.trim().is_empty() {
        return Err(MessageContentError::EmptyText);
    }
    if text.contains('\0') {
        return Err(MessageContentError::TextContainsNul);
    }
    let actual = text.chars().count();
    if actual > MAX_TEXT_CHARS {
        return Err(MessageContentError::TextTooLong { actual });
    }
    if msg_seq == 0 {
        return Err(MessageContentError::InvalidSequence);
    }
    Ok(())
}

fn help_keyboard(scope: MessageScope) -> Value {
    // Group auto-submit is unsupported. Keep private behavior identical so a
    // button click always leaves the final send decision with the user.
    let enter = match scope {
        MessageScope::Group | MessageScope::Private => false,
    };
    let button = |id: &str, label: &str, command: &str| {
        json!({
            "id": id,
            "render_data": { "label": label, "style": 1 },
            "action": {
                "type": 2,
                "permission": { "type": 2 },
                "data": command,
                "enter": enter,
            },
        })
    };
    json!({
        "content": {
            "rows": [
                { "buttons": [
                    button("help", "帮助", "/help"),
                    button("status", "状态", "/status"),
                ] },
                { "buttons": [
                    button("ping", "连通测试", "/ping"),
                    button("models", "模型列表", "/models"),
                ] },
            ],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_keyboards_are_bounded_prefills_with_unique_ids() {
        let buttons = (0..10)
            .map(|index| CommandButton {
                label: format!("模型{index}"),
                command: format!("/model preset-{index}"),
            })
            .collect::<Vec<_>>();
        let keyboard = command_keyboard(&buttons).unwrap();
        let rows = keyboard["content"]["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 5);
        let mut ids = std::collections::BTreeSet::new();
        for row in rows {
            for button in row["buttons"].as_array().unwrap() {
                assert_eq!(button["action"]["type"], 2);
                assert_eq!(button["action"]["enter"], false);
                ids.insert(button["id"].as_str().unwrap());
            }
        }
        assert_eq!(ids.len(), 10);
        let mut excessive = buttons.clone();
        excessive.push(buttons[0].clone());
        assert!(command_keyboard(&excessive).is_err());
        for command in ["/mode 1\n/reset", "https://example.com", "/mode\0"] {
            assert!(
                command_keyboard(&[CommandButton {
                    label: "选择".into(),
                    command: command.into()
                }])
                .is_err()
            );
        }
        assert!(
            command_keyboard(&[CommandButton {
                label: "模".repeat(11),
                command: "/models".into()
            }])
            .is_err()
        );
    }

    #[test]
    fn text_payload_preserves_content_and_delivery_identity() {
        let body = text_body("  原样\n内容  ", "passive-message", 7).unwrap();
        assert_eq!(
            body,
            json!({
                "content": "  原样\n内容  ", "msg_type": 0,
                "msg_id": "passive-message", "msg_seq": 7,
            })
        );
    }

    #[test]
    fn markdown_without_keyboard_has_only_custom_content_and_delivery_fields() {
        assert_eq!(
            markdown_body(
                "# 标题\n**正文**",
                "passive-message",
                3,
                MessageScope::Private,
                false
            )
            .unwrap(),
            json!({
                "msg_type": 2, "msg_id": "passive-message", "msg_seq": 3,
                "markdown": { "content": "# 标题\n**正文**" },
            })
        );
    }

    #[test]
    fn group_keyboard_uses_exact_two_by_two_command_payload() {
        assert_eq!(
            markdown_body("帮助", "passive-message", 1, MessageScope::Group, true).unwrap(),
            json!({
                "msg_type": 2, "msg_id": "passive-message", "msg_seq": 1,
                "markdown": { "content": "帮助" },
                "keyboard": { "content": { "rows": [
                    { "buttons": [
                        { "id": "help", "render_data": { "label": "帮助", "style": 1 },
                          "action": { "type": 2, "permission": { "type": 2 },
                                      "data": "/help", "enter": false } },
                        { "id": "status", "render_data": { "label": "状态", "style": 1 },
                          "action": { "type": 2, "permission": { "type": 2 },
                                      "data": "/status", "enter": false } },
                    ] },
                    { "buttons": [
                        { "id": "ping", "render_data": { "label": "连通测试", "style": 1 },
                          "action": { "type": 2, "permission": { "type": 2 },
                                      "data": "/ping", "enter": false } },
                        { "id": "models", "render_data": { "label": "模型列表", "style": 1 },
                          "action": { "type": 2, "permission": { "type": 2 },
                                      "data": "/models", "enter": false } },
                    ] },
                ] } },
            })
        );
    }

    #[test]
    fn both_scopes_only_fill_commands_without_callbacks() {
        for scope in [MessageScope::Group, MessageScope::Private] {
            let body = markdown_body("帮助", "passive-message", 1, scope, true).unwrap();
            let rows = body["keyboard"]["content"]["rows"].as_array().unwrap();
            assert_eq!(rows.len(), 2);
            let mut commands = Vec::new();
            for row in rows {
                let buttons = row["buttons"].as_array().unwrap();
                assert_eq!(buttons.len(), 2);
                for button in buttons {
                    let action = &button["action"];
                    assert_eq!(action["type"], 2);
                    assert_eq!(action["enter"], false);
                    assert_eq!(action["permission"], json!({ "type": 2 }));
                    assert!(action.get("callback_data").is_none());
                    assert!(action.get("unsupport_tips").is_none());
                    commands.push(action["data"].as_str().unwrap());
                }
            }
            assert_eq!(commands, ["/help", "/status", "/ping", "/models"]);
        }
    }

    #[test]
    fn unicode_limit_counts_characters_in_both_formats() {
        let allowed = "🦀".repeat(MAX_TEXT_CHARS);
        let excessive = format!("{allowed}字");
        assert!(text_body(&allowed, "passive-message", 1).is_ok());
        assert!(markdown_body(&allowed, "passive-message", 1, MessageScope::Group, false).is_ok());
        let expected = Err(MessageContentError::TextTooLong { actual: 4001 });
        assert_eq!(text_body(&excessive, "passive-message", 1), expected);
        assert_eq!(
            markdown_body(
                &excessive,
                "passive-message",
                1,
                MessageScope::Private,
                true
            ),
            expected
        );
    }

    #[test]
    fn invalid_content_and_delivery_identity_are_rejected() {
        for text in ["", " \n\t", "\u{3000}"] {
            assert_eq!(
                text_body(text, "passive-message", 1),
                Err(MessageContentError::EmptyText)
            );
        }
        assert_eq!(
            text_body("a\0b", "passive-message", 1),
            Err(MessageContentError::TextContainsNul)
        );
        for id in ["", "  ", "id\0suffix"] {
            assert_eq!(
                text_body("正文", id, 1),
                Err(MessageContentError::InvalidPassiveMessageId)
            );
        }
        assert_eq!(
            text_body("正文", "passive-message", 0),
            Err(MessageContentError::InvalidSequence)
        );
        assert_eq!(
            markdown_body("\0", "passive-message", 1, MessageScope::Group, true),
            Err(MessageContentError::TextContainsNul)
        );
    }

    #[test]
    fn building_payload_never_changes_the_persisted_sequence() {
        let first = markdown_body(
            "正文",
            "passive-message",
            u32::MAX,
            MessageScope::Group,
            true,
        )
        .unwrap();
        let repeated = markdown_body(
            "正文",
            "passive-message",
            u32::MAX,
            MessageScope::Group,
            true,
        )
        .unwrap();
        assert_eq!(first, repeated);
        assert_eq!(first["msg_seq"], u32::MAX);
    }
}
