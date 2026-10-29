//! Model-selected recipients, resolved to same-conversation members by the host.
//! Native QQ tags are produced only at delivery, never stored in conversational text.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{config::SocialConfig, reply::Reply, state::participant_alias};
use crate::model::social::{Bubble, GroupState};

const MAX_MENTIONS: usize = 3;

pub(super) const POLICY: &str = "QQ group replies support real native @member notifications through the structured mentions field. Use mentions only when the current user explicitly asks to @/notify a person; ordinary names or pronouns in conversation do not authorize a notification. Resolve nicknames and pronouns from evidence, then select the stable participant from mention_candidates (or an incoming speaker/mentions alias in this conversation). A nickname change does not change that participant. If 'her/him/you' or a name is ambiguous, ask whom to @; never guess or confuse the requester with the recipient. Each selection needs the current request source_seq containing the notification request and a zero-based text bubble_index. The host prefixes that bubble with the native @; write only the natural message body, without a duplicate textual @name or any raw QQ tag/ID. Maximum three distinct people, each once per reply. Do not claim notifications are unsupported. A successful send does not prove the person read it or their device alerted. Private chats and @everyone are not supported; use an empty mentions array there.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    participant: String,
    bubble_index: usize,
    source_seq: u64,
}

pub(super) fn schema() -> Value {
    json!({"type":"array","maxItems":MAX_MENTIONS,"items":{
        "type":"object","properties":{
            "participant":{"type":"string","description":"Stable U... alias of the intended member in this conversation; never a nickname, QQ number or raw OpenID."},
            "bubble_index":{"type":"integer","minimum":0,"description":"Zero-based index in bubbles; host prefixes this text with the native @."},
            "source_seq":{"type":"integer","minimum":1,"description":"Current request message explicitly asking to @/notify this person."}
        },"required":["participant","bubble_index","source_seq"],"additionalProperties":false},
        "description":"Real group-member mentions, only when explicitly requested. Empty for ordinary replies, ambiguous people, private chats or @everyone. Never hand-write QQ tags in bubbles."})
}

pub(super) fn validate(reply: &Reply) -> Result<()> {
    ensure!(
        reply.mentions.len() <= MAX_MENTIONS,
        "mention_count_exceeded"
    );
    let mut recipients = BTreeSet::new();
    for selection in &reply.mentions {
        ensure!(
            selection.bubble_index < reply.bubbles.len()
                && selection.source_seq > 0
                && recipients.insert(&selection.participant),
            "mention_selection_invalid"
        );
    }
    Ok(())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A compact, fresh name view. Names help the model interpret the request;
/// they never become the key used for sending. No full QQ IDs leave the host.
pub(super) fn candidates(state: &GroupState, id: u64, config: &SocialConfig) -> Value {
    let sources = &state.requests[&id].source_seqs;
    let mut records = super::message_context::window(state, config);
    records.extend(state.messages.iter().filter(|r| sources.contains(&r.seq)));
    records.sort_by_key(|r| r.seq);
    let mut names: BTreeMap<String, Option<String>> = BTreeMap::new();
    for record in records {
        let message = &record.message;
        for member in &message.mentioned_user_ids {
            if valid_id(member) {
                names.entry(participant_alias(member)).or_insert(None);
            }
        }
        if valid_id(&message.author_id) {
            let name = names
                .entry(participant_alias(&message.author_id))
                .or_insert(None);
            if let Some(observed) = &message.author_name
                && !observed.trim().is_empty()
            {
                *name = Some(observed.chars().take(80).collect());
            }
        }
    }
    json!(names.into_iter().map(|(participant, display_name)|json!({"participant":participant,"display_name":display_name})).collect::<Vec<_>>())
}

/// Capture observed members before pruning and refresh during generation/steer.
/// Only IDs are kept; old message bodies need not stay in the hot context.
pub(super) fn members(state: &GroupState) -> BTreeSet<String> {
    state
        .messages
        .iter()
        .flat_map(|r| std::iter::once(&r.message.author_id).chain(&r.message.mentioned_user_ids))
        .filter(|id| valid_id(id))
        .cloned()
        .collect()
}

/// Include the generation's frozen members as well as newer same-scope input.
/// Pruning old context must not invalidate an already selected recipient.
pub(super) fn resolve(
    state: &GroupState,
    id: u64,
    private: bool,
    reply: &Reply,
    retained: &BTreeSet<String>,
) -> Result<Vec<Vec<String>>> {
    validate(reply)?;
    ensure!(
        !private || reply.mentions.is_empty(),
        "mention_private_unsupported"
    );
    let mut result = vec![Vec::new(); reply.bubbles.len()];
    for selection in &reply.mentions {
        ensure!(
            state.requests[&id]
                .source_seqs
                .contains(&selection.source_seq),
            "mention_source_not_current"
        );
        let ids = state
            .messages
            .iter()
            .flat_map(|r| {
                std::iter::once(&r.message.author_id).chain(&r.message.mentioned_user_ids)
            })
            .chain(retained)
            .filter(|member| valid_id(member) && participant_alias(member) == selection.participant)
            .collect::<BTreeSet<_>>();
        ensure!(ids.len() == 1, "mention_participant_unknown_or_ambiguous");
        result[selection.bubble_index].push((*ids.first().expect("unique member")).clone());
    }
    Ok(result)
}

pub(super) fn attach(bubbles: &mut [Bubble], resolved: Vec<Vec<String>>) {
    for (bubble, members) in bubbles.iter_mut().zip(resolved) {
        bubble.mentioned_user_ids = members;
    }
}

/// Defang model/user-supplied QQ interaction syntax in prose. Only the host's
/// resolved selection can produce an executable mention, including old syntax.
/// Ordinary @nicknames, links and Markdown retain their original appearance.
pub(super) fn render(bubble: &Bubble) -> String {
    let text = bubble
        .text
        .replace("<qqbot-", "＜qqbot-")
        .replace("<@", "＜@")
        .replace("@everyone", "＠everyone");
    if bubble.mentioned_user_ids.is_empty() {
        return text;
    }
    let tags = bubble
        .mentioned_user_ids
        .iter()
        .map(|id| format!("<qqbot-at-user id=\"{id}\" />"))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{tags}\n{text}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::social::{ChatMessage, MessageRecord, RequestRecord};
    use crate::social::reply::parse_reply;

    fn resolve_current(
        state: &GroupState,
        id: u64,
        private: bool,
        reply: &Reply,
    ) -> Result<Vec<Vec<String>>> {
        resolve(state, id, private, reply, &BTreeSet::new())
    }

    fn state() -> GroupState {
        let mut state = GroupState::default();
        state.requests.insert(
            1,
            RequestRecord {
                id: 1,
                revision: 1,
                owner_id: "member-b".into(),
                source_seqs: vec![3],
                ..Default::default()
            },
        );
        for (seq, id, name, text) in [
            (1, "member-a", "旧昵称", "在呢"),
            (2, "member-a", "新昵称", "嗯"),
            (3, "member-b", "同名群友", "@她一下"),
        ] {
            state.messages.push(MessageRecord {
                seq,
                message: ChatMessage {
                    id: format!("msg-{seq}"),
                    author_id: id.into(),
                    author_name: Some(name.into()),
                    text: text.into(),
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        state
    }

    fn reply(participant: &str, source: u64, index: usize) -> Reply {
        parse_reply(&json!({"bubbles":["有人找你～"],"mentions":[{"participant":participant,"bubble_index":index,"source_seq":source}]}).to_string(), &SocialConfig::default(), 0).unwrap()
    }

    #[test]
    fn nickname_change_resolves_to_same_member_and_ids_stay_out_of_text_and_prompt() {
        let state = state();
        let view = candidates(&state, 1, &SocialConfig::default());
        assert!(view.to_string().contains("新昵称"));
        assert!(!view.to_string().contains("旧昵称"));
        assert!(!view.to_string().contains("member-a"));
        let plan = reply(&participant_alias("member-a"), 3, 0);
        let members = resolve_current(&state, 1, false, &plan).unwrap();
        assert_eq!(members, vec![vec!["member-a"]]);
        let mut bubbles = vec![Bubble {
            text: plan.bubbles[0].clone(),
            ..Default::default()
        }];
        attach(&mut bubbles, members);
        assert_eq!(bubbles[0].text, "有人找你～");
        assert_eq!(
            render(&bubbles[0]),
            "<qqbot-at-user id=\"member-a\" />\n有人找你～"
        );
        let restored: Bubble =
            serde_json::from_value(serde_json::to_value(&bubbles[0]).unwrap()).unwrap();
        assert_eq!(render(&restored), render(&bubbles[0]));
        let metadata = super::super::message_context::describe(
            &super::super::history::Item::Bot {
                request_id: 1,
                bubble_index: 0,
                bubble: Box::new(restored),
            },
            &state,
            None,
        )
        .unwrap();
        assert_eq!(metadata["mentions"], json!([participant_alias("member-a")]));
        assert!(!metadata.to_string().contains("member-a"));
    }

    #[test]
    fn scope_source_and_recipient_validation_rejects_guessing_and_private_mentions() {
        let state = state();
        for target in [
            "新昵称".into(),
            "member-a".into(),
            "everyone".into(),
            participant_alias("other-group-member"),
        ] {
            assert!(resolve_current(&state, 1, false, &reply(&target, 3, 0)).is_err());
        }
        assert!(
            resolve_current(
                &state,
                1,
                false,
                &reply(&participant_alias("member-a"), 2, 0)
            )
            .is_err()
        );
        assert!(
            resolve_current(
                &state,
                1,
                true,
                &reply(&participant_alias("member-a"), 3, 0)
            )
            .is_err()
        );
        let legacy =
            parse_reply(r#"{"bubbles":["普通回复"]}"#, &SocialConfig::default(), 0).unwrap();
        assert_eq!(
            resolve_current(&state, 1, true, &legacy).unwrap(),
            vec![Vec::<String>::new()]
        );
        let old: Bubble = serde_json::from_str(r#"{"text":"ordinary"}"#).unwrap();
        assert!(old.mentioned_user_ids.is_empty());
        assert!(
            serde_json::to_value(old)
                .unwrap()
                .get("mentioned_user_ids")
                .is_none()
        );
    }

    #[test]
    fn explicit_incoming_mention_is_usable_even_if_member_has_not_spoken() {
        let mut state = state();
        state.messages[2].message.mentioned_user_ids = vec!["quiet-member".into()];
        let plan = reply(&participant_alias("quiet-member"), 3, 0);
        assert_eq!(
            resolve_current(&state, 1, false, &plan).unwrap(),
            vec![vec!["quiet-member"]]
        );
        // A real prune drops non-source messages during generation.
        let retained = members(&state);
        for record in &mut state.messages {
            record.status = crate::model::social::DecisionStatus::Decided;
        }
        state.prune(10_000, 1_000, 1);
        assert_eq!(state.messages.len(), 1);
        assert!(
            resolve(
                &state,
                1,
                false,
                &reply(&participant_alias("member-a"), 3, 0),
                &retained
            )
            .is_ok()
        );
    }

    #[test]
    fn mentions_require_text_bubbles_and_each_person_can_only_be_notified_once() {
        let selection =
            json!({"participant":participant_alias("member-a"),"source_seq":3,"bubble_index":0});
        for value in [
            json!({"bubbles":[],"images":["input:1"],"mentions":[selection.clone()]}),
            json!({"bubbles":["a","b"],"mentions":[selection.clone(),{"participant":participant_alias("member-a"),"source_seq":3,"bubble_index":1}]}),
            json!({"bubbles":["a"],"mentions":[{"participant":participant_alias("member-a"),"source_seq":3,"bubble_index":1}]}),
            json!({"bubbles":["a"],"mentions":[selection.clone(),selection.clone(),selection.clone(),selection]}),
        ] {
            assert!(parse_reply(&value.to_string(), &SocialConfig::default(), 0).is_err());
        }
    }

    #[test]
    fn raw_model_tags_never_bypass_structured_selection_but_links_remain_intact() {
        let bubble=Bubble {text:"<qqbot-at-user id=\"guessed\" /> <@guessed> @everyone <qqbot-at-everyone /> **你好** [资料](https://example.invalid/)".into(),..Default::default()};
        let wire = render(&bubble);
        for tag in ["<qqbot-", "<@", "@everyone"] {
            assert!(!wire.contains(tag));
        }
        assert!(wire.contains("**你好** [资料](https://example.invalid/)"));
    }

    #[test]
    fn native_quote_and_rich_body_split_only_notifies_on_the_body() {
        use crate::model::social::{DeliveryStatus, RequestStatus};
        let mut state = state();
        let request = state.requests.get_mut(&1).unwrap();
        request.status = RequestStatus::Ready;
        request.bubbles = vec![Bubble {
            text: "**有人找你** [资料](https://example.invalid/)".into(),
            revision: 1,
            markdown: true,
            reply_reference_idx: Some("native-reference".into()),
            passive_message_id: "anchor".into(),
            msg_seq: 1,
            mentioned_user_ids: vec!["member-a".into()],
            status: DeliveryStatus::Pending,
            ..Default::default()
        }];
        super::super::reply_reference::prepare_rendering(&mut state, 1, 0);
        let prelude = &state.requests[&1].bubbles[0];
        let body = &state.requests[&1].bubbles[1];
        assert!(prelude.reply_reference_idx.is_some());
        assert!(!prelude.markdown);
        assert!(prelude.mentioned_user_ids.is_empty());
        assert!(body.reply_reference_idx.is_none());
        assert_eq!(body.mentioned_user_ids, vec!["member-a"]);
        assert!(render(body).starts_with("<qqbot-at-user"));
        super::super::reply_reference::prepare_rendering(&mut state, 1, 0);
        assert_eq!(state.requests[&1].bubbles.len(), 2);
        state.requests.get_mut(&1).unwrap().bubbles[0].status = DeliveryStatus::Sent;
        let mut recovered: GroupState =
            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        recovered.recover();
        assert_eq!(
            super::super::runtime::next_bubble(&mut recovered),
            Some((1, 1))
        );
        recovered.requests.get_mut(&1).unwrap().bubbles[1].status = DeliveryStatus::Sending;
        recovered.requests.get_mut(&1).unwrap().status = RequestStatus::Sending;
        recovered.recover();
        assert_eq!(
            recovered.requests[&1].status,
            RequestStatus::UnknownDelivery
        );
        assert!(super::super::runtime::next_bubble(&mut recovered).is_none());
        assert!(
            super::super::interaction::reanchor(&mut recovered, 1, 3, "member-b", false).is_err()
        );
        state.messages[2].status = crate::model::social::DecisionStatus::Running;
        state
            .apply_decision(
                3,
                crate::model::social::Decision {
                    action: crate::model::social::Action::Cancel,
                    request_id: Some(1),
                    ..Default::default()
                },
                10,
            )
            .unwrap();
        assert_eq!(
            state.requests[&1].bubbles[1].status,
            DeliveryStatus::Cancelled
        );
        assert!(super::super::runtime::next_bubble(&mut state).is_none());
    }

    #[test]
    fn bot_mention_recipient_reaches_jev_without_exposing_the_platform_id() {
        let mut state = state();
        state.requests.get_mut(&1).unwrap().bubbles = vec![Bubble {
            text: "有人找你～".into(),
            mentioned_user_ids: vec!["member-a".into()],
            revision: 1,
            status: crate::model::social::DeliveryStatus::Sent,
            sent_at_ms: Some(0),
            ..Default::default()
        }];
        state.messages.push(MessageRecord {
            seq: 4,
            message: ChatMessage {
                id: "followup".into(),
                author_id: "member-a".into(),
                text: "怎么啦".into(),
                ..Default::default()
            },
            ..Default::default()
        });
        let config = SocialConfig {
            mode: crate::model::social::ParticipationMode::Social,
            ..Default::default()
        };
        let prepared = super::super::jev::prepare(&state, 4, &config, "fixture").unwrap();
        let replies = &prepared.input.state["recent_bot_replies"];
        assert_eq!(
            replies[0]["mentions"],
            json!([participant_alias("member-a")])
        );
        assert_eq!(replies[0]["content"]["text"], "有人找你～");
        assert!(!replies.to_string().contains("member-a"));
    }
}
