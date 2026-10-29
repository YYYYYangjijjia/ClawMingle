//! QQ group transport values. Message scene authentication data is discarded
//! while decoding, before an event can be logged or persisted by a caller.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GroupMessageSource {
    At,
    #[default]
    All,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupUser {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub member_openid: Option<String>,
    #[serde(default)]
    pub user_openid: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub member_role: Option<String>,
    #[serde(default)]
    pub bot: bool,
    /// Platform-provided self mention marker; IDs may use different namespaces.
    #[serde(default)]
    pub is_you: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupAttachment {
    pub content_type: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub voice_wav_url: Option<String>,
    #[serde(default)]
    pub asr_refer_text: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupMessageElement {
    #[serde(default)]
    pub msg_idx: Option<String>,
    #[serde(default)]
    pub author: Option<GroupUser>,
    #[serde(default)]
    pub message_type: Option<u32>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub attachments: Vec<GroupAttachment>,
    #[serde(default)]
    pub ark_data: Option<serde_json::Value>,
    #[serde(default)]
    pub msg_elements: Vec<GroupMessageElement>,
}

/// Only non-secret scene identifiers are retained. `auth_token`, including an
/// unknown or repeated entry, is intentionally absent from this type.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct GroupMessageScene {
    pub source: Option<String>,
    pub msg_idx: Option<String>,
    pub ref_msg_idx: Option<String>,
}

impl<'de> Deserialize<'de> for GroupMessageScene {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireScene {
            #[serde(default)]
            source: Option<String>,
            #[serde(default)]
            ext: Vec<String>,
            // Also accept this type's sanitized serialized representation.
            #[serde(default)]
            msg_idx: Option<String>,
            #[serde(default)]
            ref_msg_idx: Option<String>,
        }
        let wire = WireScene::deserialize(deserializer)?;
        let mut scene = Self {
            source: wire.source,
            msg_idx: wire.msg_idx,
            ref_msg_idx: wire.ref_msg_idx,
        };
        for entry in wire.ext {
            match entry.split_once('=') {
                Some(("msg_idx", value)) => scene.msg_idx = Some(value.to_owned()),
                Some(("ref_msg_idx", value)) => scene.ref_msg_idx = Some(value.to_owned()),
                _ => {}
            }
        }
        Ok(scene)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupMessageEvent {
    pub id: String,
    pub group_openid: String,
    pub author: GroupUser,
    #[serde(default)]
    pub content: String,
    pub timestamp: DateTime<Utc>,
    #[serde(default)]
    pub message_type: Option<u32>,
    #[serde(default)]
    pub mentions: Vec<GroupUser>,
    #[serde(default)]
    pub message_scene: Option<GroupMessageScene>,
    #[serde(default)]
    pub attachments: Vec<GroupAttachment>,
    #[serde(default)]
    pub msg_elements: Vec<GroupMessageElement>,
    /// Set by the gateway from the dispatch name, never inferred from text.
    #[serde(default)]
    pub source: GroupMessageSource,
    /// READY's bot user id, also retained across a resumable connection.
    #[serde(default)]
    pub bot_id: Option<String>,
}

impl GroupMessageEvent {
    pub fn is_at_bot(&self) -> bool {
        self.source == GroupMessageSource::At
            || self.mentions.iter().any(|user| user.is_you)
            || self.bot_id.as_deref().is_some_and(|bot_id| {
                self.mentions.iter().any(|user| {
                    user.id.as_deref() == Some(bot_id)
                        || user.member_openid.as_deref() == Some(bot_id)
                })
            })
    }

    /// A quote remains context; this helper does not turn it into a mention.
    pub fn quoted_message(&self) -> Option<GroupQuote> {
        if self.message_type != Some(super::types::MSG_TYPE_QUOTE) {
            return None;
        }
        let element = self.msg_elements.first();
        let reference_idx = element
            .and_then(|element| element.msg_idx.clone())
            .or_else(|| {
                self.message_scene
                    .as_ref()
                    .and_then(|scene| scene.ref_msg_idx.clone())
            });
        Some(GroupQuote {
            reference_idx,
            author: element.and_then(|element| element.author.clone()),
            text: element
                .and_then(|element| element.content.clone())
                .unwrap_or_default(),
            timestamp: element.and_then(|element| element.timestamp),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupQuote {
    pub reference_idx: Option<String>,
    pub author: Option<GroupUser>,
    pub text: String,
    /// The platform does not always provide the original message's time.
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupMessageAck {
    pub id: String,
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub ext_info: Option<GroupMessageAckExt>,
}

impl GroupMessageAck {
    pub fn ref_idx(&self) -> Option<&str> {
        self.ext_info
            .as_ref()
            .and_then(|ext| ext.ref_idx.as_deref())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupMessageAckExt {
    #[serde(default)]
    pub ref_idx: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> serde_json::Value {
        serde_json::json!({
            "id": "message-1", "group_openid": "group-1",
            "author": {"member_openid": "member-1"},
            "content": "帮我看看", "timestamp": "2026-10-22T12:00:00+08:00"
        })
    }

    #[test]
    fn at_event_needs_neither_mentions_nor_text_prefix() {
        let mut parsed: GroupMessageEvent = serde_json::from_value(event()).unwrap();
        parsed.source = GroupMessageSource::At;
        assert!(parsed.is_at_bot());
        assert!(parsed.mentions.is_empty());
        assert_eq!(parsed.content, "帮我看看");
    }

    #[test]
    fn only_structured_mentions_match_the_bot_in_all_events() {
        let mut wire = event();
        wire["content"] = "Codex 帮帮我".into();
        let mut parsed: GroupMessageEvent = serde_json::from_value(wire).unwrap();
        parsed.bot_id = Some("bot-1".into());
        assert!(!parsed.is_at_bot());
        parsed.mentions.push(GroupUser {
            id: Some("bot-1".into()),
            ..GroupUser::default()
        });
        assert!(parsed.is_at_bot());
    }

    #[test]
    fn server_self_mention_marker_matches_without_ready_identity() {
        let mut wire = event();
        wire["mentions"] = serde_json::json!([{
            "member_openid": "group-scoped-bot", "is_you": true
        }]);
        let mut parsed: GroupMessageEvent = serde_json::from_value(wire.clone()).unwrap();
        assert!(parsed.is_at_bot());
        parsed.bot_id = Some("different-ready-namespace".into());
        assert!(parsed.is_at_bot());
        wire["mentions"][0]["is_you"] = false.into();
        assert!(
            !serde_json::from_value::<GroupMessageEvent>(wire.clone())
                .unwrap()
                .is_at_bot()
        );
        wire["mentions"][0]["is_you"] = "true".into();
        assert!(serde_json::from_value::<GroupMessageEvent>(wire).is_err());
    }

    #[test]
    fn scene_discards_authentication_and_preserves_complete_reference_values() {
        let wire = serde_json::json!({
            "source": "default",
            "ext": ["msg_idx=ref-1==", "auth_token=private-marker", "ref_msg_idx=ref-0"]
        });
        let scene: GroupMessageScene = serde_json::from_value(wire).unwrap();
        assert_eq!(scene.msg_idx.as_deref(), Some("ref-1=="));
        assert_eq!(scene.ref_msg_idx.as_deref(), Some("ref-0"));
        let serialized = serde_json::to_string(&scene).unwrap();
        assert!(!serialized.contains("auth_token"));
        assert!(!serialized.contains("private-marker"));
        assert!(!format!("{scene:?}").contains("private-marker"));
        assert_eq!(
            serde_json::from_str::<GroupMessageScene>(&serialized).unwrap(),
            scene
        );
    }

    #[test]
    fn quote_keeps_author_text_and_reference_without_becoming_a_mention() {
        let mut wire = event();
        wire["message_type"] = 103.into();
        wire["msg_elements"] = serde_json::json!([{
            "msg_idx": "ref-old", "content": "引用正文", "author": {"member_openid": "member-2"}
        }]);
        let parsed: GroupMessageEvent = serde_json::from_value(wire).unwrap();
        let quote = parsed.quoted_message().unwrap();
        assert_eq!(quote.reference_idx.as_deref(), Some("ref-old"));
        assert_eq!(
            quote.author.unwrap().member_openid.as_deref(),
            Some("member-2")
        );
        assert_eq!(quote.text, "引用正文");
        assert_eq!(quote.timestamp, None);
        assert!(!parsed.is_at_bot());
    }
}
