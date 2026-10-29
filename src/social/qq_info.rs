//! Host-bound QQ reads. Arguments cannot choose another group or a raw member ID.
use super::state::participant_alias;
use crate::{
    codex::app_server::QqQuery,
    model::social::GroupState,
    qq::{QqApiClient, QqReadError, QqReadRequest, message_content::MessageScope},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Arguments {
    #[serde(rename = "self")]
    SelfInfo {},
    Group {},
    Members {
        cursor: Option<String>,
    },
    Member {
        participant: String,
    },
    BotState {},
}

pub fn query(qq: Arc<QqApiClient>, private: bool, target: String, state: &GroupState) -> QqQuery {
    let mut aliases = BTreeMap::new();
    for id in state.messages.iter().map(|r| &r.message.author_id).chain(
        state
            .requests
            .values()
            .flat_map(|r| r.contributor_ids.iter()),
    ) {
        aliases.insert(participant_alias(id), id.clone());
    }
    let aliases = Arc::new(Mutex::new(aliases));
    Arc::new(move |arguments: String| {
        let qq = qq.clone();
        let target = target.clone();
        let aliases = aliases.clone();
        Box::pin(async move { run(&qq, private, &target, &aliases, &arguments).await })
    })
}

fn error(code: &str, message: &str) -> Value {
    json!({"ok":false,"error":{"code":code,"message":message}})
}
fn text(value: &Value, key: &str, max: usize) -> Value {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(|v| json!(v.chars().take(max).collect::<String>()))
        .unwrap_or(Value::Null)
}
fn scalar(value: &Value, key: &str, max: usize) -> Value {
    let value = value.get(key).unwrap_or(&Value::Null);
    if value.is_boolean() || value.is_number() {
        value.clone()
    } else {
        value
            .as_str()
            .map(|s| json!(s.chars().take(max).collect::<String>()))
            .unwrap_or(Value::Null)
    }
}
fn boolean(value: &Value, key: &str) -> Value {
    value
        .get(key)
        .and_then(Value::as_bool)
        .map(Value::Bool)
        .unwrap_or(Value::Null)
}
fn member(value: &Value, aliases: &Mutex<BTreeMap<String, String>>) -> Value {
    let Some(id) = value
        .get("member_openid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 256)
    else {
        return Value::Null;
    };
    let alias = participant_alias(id);
    if let Ok(mut known) = aliases.lock()
        && known.len() < 1000
    {
        known.insert(alias.clone(), id.to_owned());
    }
    json!({"participant":alias,"username":text(value,"username",80),"member_role":scalar(value,"member_role",16),"bot":boolean(value,"bot"),"joined_at":scalar(value,"joined_at",80)})
}

async fn run(
    qq: &QqApiClient,
    private: bool,
    target: &str,
    aliases: &Mutex<BTreeMap<String, String>>,
    raw: &str,
) -> Value {
    if raw.len() > 4096 {
        return error("invalid_arguments", "QQ资料查询参数过长。");
    }
    let Ok(arguments) = serde_json::from_str::<Arguments>(raw) else {
        return error(
            "invalid_arguments",
            "请使用固定的QQ资料查询操作，成员只能用本对话participant别名。",
        );
    };
    if private && !matches!(arguments, Arguments::SelfInfo {}) {
        return error(
            "not_available_in_private",
            "该操作只适用于当前QQ群；官方未提供此处可用的私聊完整资料卡接口。",
        );
    }
    let request = match &arguments {
        Arguments::SelfInfo {} => QqReadRequest::SelfInfo,
        Arguments::Group {} => QqReadRequest::GroupInfo,
        Arguments::BotState {} => QqReadRequest::BotState,
        Arguments::Members { cursor } => QqReadRequest::Members {
            cursor: cursor.clone(),
        },
        Arguments::Member { participant } => {
            let id = aliases
                .lock()
                .ok()
                .and_then(|known| known.get(participant).cloned());
            let Some(member_openid) = id else {
                return error(
                    "participant_unknown",
                    "当前群中没有这个成员别名；可先列出群成员。",
                );
            };
            QqReadRequest::Member { member_openid }
        }
    };
    let result = qq
        .read_profile(
            if private {
                MessageScope::Private
            } else {
                MessageScope::Group
            },
            target,
            request,
        )
        .await;
    let value = match result {
        Ok(value) => value,
        Err(QqReadError::PermissionDenied) => {
            return error(
                "platform_not_authorized",
                "QQ平台未授权此接口；群资料/成员等能力可能仍为白名单或内邀，不能通过本地设置获得。",
            );
        }
        Err(QqReadError::Rejected { status: 429, .. }) => {
            return error(
                "platform_rate_limited",
                "QQ资料接口频率已达限制，请稍后重试。",
            );
        }
        Err(QqReadError::InvalidRequest) => {
            return error("invalid_arguments", "QQ资料查询范围或参数无效。");
        }
        Err(_) => {
            return error(
                "platform_read_failed",
                "本次未能从QQ平台取得资料；没有使用猜测值替代。",
            );
        }
    };
    let data = match arguments {
        Arguments::SelfInfo {} => {
            json!({"username":text(&value,"username",80),"avatar":text(&value,"avatar",2048),"bot":boolean(&value,"bot"),"unsupported":["description","profile_write"]})
        }
        Arguments::Group {} => {
            json!({"group_name":text(&value,"group_name",200),"group_finger_memo":text(&value,"group_finger_memo",2000),"group_class_text":text(&value,"group_class_text",100),"group_tags":value.get("group_tags").and_then(Value::as_array).map(|tags|tags.iter().take(20).filter_map(Value::as_str).map(|tag|tag.chars().take(80).collect::<String>()).collect::<Vec<_>>()),"group_member_num":value.get("group_member_num").and_then(Value::as_u64)})
        }
        Arguments::BotState {} => {
            json!({"allow_proactive_msg":boolean(&value,"allow_proactive_msg"),"recv_msg_setting":scalar(&value,"recv_msg_setting",32),"member_role":scalar(&value,"member_role",16),"joined_at":scalar(&value,"joined_at",80)})
        }
        Arguments::Member { .. } => member(&value, aliases),
        Arguments::Members { .. } => {
            json!({"members":value.get("members").and_then(Value::as_array).map(|members|members.iter().take(30).map(|entry|member(entry,aliases)).filter(|entry|!entry.is_null()).collect::<Vec<_>>()),"next_cursor":text(&value,"next_cursor",512)})
        }
    };
    json!({"ok":true,"source":"qq_official_api","untrusted_data":true,"data":data})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projection_never_exposes_platform_ids_and_references_stay_local() {
        let aliases = Mutex::new(BTreeMap::new());
        let value = member(
            &json!({"member_openid":"secret-member-id","username":"昵称","union_openid":"secret-union-id","member_role":"member","avatar":"not-a-documented-field"}),
            &aliases,
        );
        let encoded = value.to_string();
        assert!(!encoded.contains("secret-member-id"));
        assert!(!encoded.contains("secret-union-id"));
        assert!(!encoded.contains("avatar"));
        assert_eq!(
            aliases
                .lock()
                .unwrap()
                .get(value["participant"].as_str().unwrap())
                .map(String::as_str),
            Some("secret-member-id")
        );
        assert!(
            serde_json::from_str::<Arguments>(r#"{"action":"member","member_openid":"other"}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<Arguments>(r#"{"action":"group","group_openid":"other"}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<Arguments>(r#"{"action":"profile_write","username":"rename"}"#)
                .is_err()
        );
    }
    #[tokio::test]
    async fn members_result_enables_same_scope_detail_without_exposing_ids() {
        use crate::config::QqConfig;
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"access_token":"EXAMPLE_QQ_ACCESS_TOKEN","expires_in":7200}),
                ),
            )
            .expect(1)
            .mount(&server)
            .await;
        let raw = "new-member-not-in-state";
        Mock::given(method("GET")).and(path("/v2/groups/current/members")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"members":[{"member_openid":raw,"username":"New member","member_role":"owner"}],"next_cursor":""}))).expect(1).mount(&server).await;
        Mock::given(method("GET"))
            .and(path(format!("/v2/groups/current/members/{raw}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"member_openid":raw,"username":"New member","member_role":"owner"}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let client = Arc::new(
            QqApiClient::new(QqConfig {
                app_id: "fixture-app".into(),
                app_secret: "EXAMPLE_QQ_APP_SECRET".into(),
                api_base_url: server.uri(),
                token_url: format!("{}/token", server.uri()),
            })
            .unwrap(),
        );
        let first = query(
            client.clone(),
            false,
            "current".into(),
            &GroupState::default(),
        );
        let second = query(
            client.clone(),
            false,
            "other".into(),
            &GroupState::default(),
        );
        let listing = first(r#"{"action":"members"}"#.into()).await;
        assert_eq!(listing["ok"], true);
        let alias = listing["data"]["members"][0]["participant"]
            .as_str()
            .unwrap();
        assert!(!listing.to_string().contains(raw));
        let detail = first(json!({"action":"member","participant":alias}).to_string()).await;
        assert_eq!(detail["data"]["username"], "New member");
        assert_eq!(detail["untrusted_data"], true);
        assert!(!detail.to_string().contains(raw));
        assert_eq!(
            second(json!({"action":"member","participant":alias}).to_string()).await["error"]["code"],
            "participant_unknown"
        );
        let private = query(
            client,
            true,
            "private-person".into(),
            &GroupState::default(),
        );
        assert_eq!(
            private(r#"{"action":"members"}"#.into()).await["error"]["code"],
            "not_available_in_private"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }
}
