//! Explicit, identity-bound memory commands. Model proposals cannot approve rewrites.
use std::collections::BTreeMap;

use super::{config::SocialConfig, control::control_reply};
use crate::{
    memory::group::{GroupMemoryError, GroupMemoryStore},
    model::social::GroupState,
};
use anyhow::Result;

const HELP: &str = "群记忆：\n/memory add 内容：提交自己的资料草稿\n/memory confirm 编号 版本：本人确认，等待每日整理\n/memory withdraw 编号 版本：撤回尚未整理的草稿\n/memory replace 正式编号 新内容：提出本人资料更正\n/memory list：查看自己的记录\n正式记忆每日北京时间04:00整理，后台也可手动整理。";

pub async fn handle(
    state: &mut GroupState,
    seq: u64,
    text: &str,
    config: &SocialConfig,
    group_id: &str,
    private: bool,
    now: i64,
) -> Result<bool> {
    let Some(arguments) = text
        .trim()
        .strip_prefix("/memory")
        .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    else {
        return Ok(false);
    };
    let message = state.record(seq)?.message.clone();
    state.begin_decision(seq)?;
    let arguments = arguments.trim().to_owned();
    let data = if private {
        config.data_dir.join("private")
    } else {
        config.data_dir.clone()
    };
    let group = group_id.to_owned();
    let is_admin = config.is_scope_admin(&message.author_id, private, group_id);
    let enabled = config.memory.enabled;
    let reply = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        if !enabled {
            return Ok("本会话记忆功能已关闭，可在后台开启。".into());
        }
        let store = GroupMemoryStore::new(&data, &group)?;
        let labels = if matches!(
            arguments.split_whitespace().next(),
            Some("records" | "inbox")
        ) {
            let reader =
                super::history::snapshot_reader(&data.join("history/archive.sqlite"), &group)?;
            super::memory_context::member_labels(&store, &reader, &message, seq)?
        } else {
            BTreeMap::new()
        };
        if let Some(reply) = operate_ledger(&store, &message, &arguments, is_admin, seq, &labels)? {
            return Ok(reply);
        }
        Ok(operate(
            &store,
            &message.author_id,
            &arguments,
            &message.id,
            message.timestamp_ms.div_euclid(1000),
        )?)
    })
    .await
    .map_err(|_| anyhow::anyhow!("memory_worker_failed"))?
    .unwrap_or_else(|_| {
        "记忆操作未完成：请检查编号、版本、本人/管理员权限或在后台刷新；没有自动批准改写。".into()
    });
    // Chat previews are bounded; full records remain available in the private UI.
    let reply = if reply.chars().count() > config.max_bubble_chars {
        format!(
            "{}…\n完整记录请在后台查看。",
            reply
                .chars()
                .take(config.max_bubble_chars.saturating_sub(22))
                .collect::<String>()
        )
    } else {
        reply
    };
    control_reply(state, seq, reply, config, now)?;
    Ok(true)
}

fn operate(
    store: &GroupMemoryStore,
    author: &str,
    arguments: &str,
    message: &str,
    time: i64,
) -> std::result::Result<String, GroupMemoryError> {
    let (action, rest) = arguments
        .split_once(char::is_whitespace)
        .unwrap_or((arguments, ""));
    let rest = rest.trim();
    match action {
        "add" if !rest.is_empty() => {
            let change = store.propose(author, rest, None, message, time)?;
            Ok(format!(
                "已提交资料草稿，尚未作为长期记忆使用。\n确认：/memory confirm {} {}\n撤回：/memory withdraw {} {}",
                change.change_id, change.revision, change.change_id, change.revision
            ))
        }
        "replace" => {
            let Some((id, text)) = rest.split_once(char::is_whitespace) else {
                return Ok(HELP.into());
            };
            let Ok(id) = id.parse() else {
                return Ok(HELP.into());
            };
            let change = store.propose(author, text.trim(), Some(id), message, time)?;
            Ok(format!(
                "已提交更正草稿。\n本人确认：/memory confirm {} {}",
                change.change_id, change.revision
            ))
        }
        "confirm" | "withdraw" => {
            let fields = rest.split_whitespace().collect::<Vec<_>>();
            if fields.len() != 2 {
                return Ok(HELP.into());
            }
            let (Ok(id), Ok(revision)) = (fields[0].parse(), fields[1].parse()) else {
                return Ok(HELP.into());
            };
            if action == "confirm" {
                store.confirm(author, id, revision)?;
                Ok("已由本人确认；每日整理或后台手动整理后进入正式记忆。".into())
            } else {
                store.withdraw(author, id, revision)?;
                Ok("已撤回该草稿。".into())
            }
        }
        "list" if rest.is_empty() => {
            let facts = store.confirmed(author)?;
            let pending = store.list_pending(author)?;
            let mut lines = vec![format!(
                "我的正式资料 {} 条，待处理草稿 {} 条。",
                facts.len(),
                pending.len()
            )];
            for fact in facts.iter().take(10) {
                lines.push(format!(
                    "正式 {} v{}：{}",
                    fact.fact_id,
                    fact.revision,
                    fact.text.chars().take(48).collect::<String>()
                ));
            }
            for change in pending.iter().take(10) {
                lines.push(format!(
                    "草稿 {} v{}（{}）：{}",
                    change.change_id,
                    change.revision,
                    if change.confirmed_at.is_some() {
                        "已确认"
                    } else {
                        "待本人确认"
                    },
                    change.text.chars().take(48).collect::<String>()
                ));
            }
            Ok(lines.join("\n"))
        }
        _ => Ok(HELP.into()),
    }
}

fn operate_ledger(
    store: &GroupMemoryStore,
    message: &crate::model::social::ChatMessage,
    arguments: &str,
    admin: bool,
    seq: u64,
    labels: &BTreeMap<String, String>,
) -> anyhow::Result<Option<String>> {
    use crate::memory::group::{
        Category, Certainty, Disposition, DraftInput, EvidenceInput, MemoryOperation, SourceKind,
    };
    let now = chrono::Utc::now().timestamp();
    let (action, rest) = arguments
        .split_once(char::is_whitespace)
        .unwrap_or((arguments, ""));
    let source = EvidenceInput {
        message_id: message.id.clone(),
        message_ref: format!("M{seq}"),
        author_id: message.author_id.clone(),
        timestamp: message.timestamp_ms.div_euclid(1000),
    };
    if action == "add" && !rest.trim().is_empty() {
        let draft = store.buffer(
            DraftInput {
                category: Category::Profile,
                subjects: vec![message.author_id.clone()],
                content: rest.trim().into(),
                disposition: Disposition::Retain,
                selection_reason: "用户明确要求记住自己的资料".into(),
                source_kind: SourceKind::UserRequested,
                certainty: Certainty::Explicit,
                evidence: vec![source],
                expires_at: None,
                operation: MemoryOperation::Append { supersedes: None },
            },
            now,
        )?;
        return Ok(Some(format!(
            "已记入待归档草稿 {} v{}，按后台设定的时间整理。撤回：/memory reject {} {}",
            draft.id, draft.revision, draft.id, draft.revision
        )));
    }
    if matches!(action, "records" | "inbox") {
        let view = store.ledger_view()?;
        let lines = if action == "records" {
            view.records
                .iter()
                .take(8)
                .map(|r| {
                    format!(
                        "正式 {} v{}：{}\n{}",
                        r.id,
                        r.revision,
                        r.data.content,
                        subject_summary(&r.data.subjects, labels)
                    )
                })
                .collect::<Vec<_>>()
        } else {
            view.drafts
                .iter()
                .rev()
                .take(8)
                .map(|d| {
                    format!(
                        "草稿 {} v{}（{:?}）：{}\n{}",
                        d.id,
                        d.revision,
                        d.status,
                        d.data.content,
                        subject_summary(&d.data.subjects, labels)
                    )
                })
                .collect::<Vec<_>>()
        };
        return Ok(Some(if lines.is_empty() {
            "暂无记录。".into()
        } else {
            lines.join("\n")
        }));
    }
    if matches!(action, "apply" | "reject") {
        let values = rest.split_whitespace().collect::<Vec<_>>();
        anyhow::ensure!(values.len() == 2, "memory_command_invalid");
        let id: u64 = values[0].parse()?;
        let rev: u64 = values[1].parse()?;
        let view = store.ledger_view()?;
        let draft = view
            .drafts
            .iter()
            .find(|d| d.id == id)
            .ok_or_else(|| anyhow::anyhow!("memory_draft_missing"))?;
        let alias = store.subject_alias(&message.author_id)?;
        anyhow::ensure!(
            admin
                || draft.data.subjects.contains(&alias)
                || draft.data.evidence.iter().any(|e| e.author == alias),
            "memory_not_owner"
        );
        if action == "reject" {
            store.reject_draft(id, rev, now)?;
            return Ok(Some("已撤回草稿，未改写正式记忆。".into()));
        }
        let record = if admin {
            store.apply_operator(id, rev, now)?
        } else {
            store.apply_explicit(id, rev, source, now)?
        };
        return Ok(Some(format!(
            "已按你的明确指令保存为正式记忆 {} v{}，之前的版本仍可在后台查看。",
            record.id, record.revision
        )));
    }
    if action.is_empty() {
        return Ok(Some("记忆：\n/memory add 内容：记录自己的资料草稿\n/memory inbox：查看变更缓冲区\n/memory records：查看正式记忆\n/memory reject 编号 版本：撤回草稿\n/memory apply 编号 版本：明确确认保存或改写\n也可以直接用自然语言请机器人查询、记住或提出更正。归档时间、分类及版本在后台管理。旧版资料仍可用 /memory list 查看。".into()));
    }
    Ok(None)
}

fn subject_summary(subjects: &[String], labels: &BTreeMap<String, String>) -> String {
    if subjects.is_empty() {
        return "关联：通用（未绑定特定成员）".into();
    }
    format!(
        "关联成员：{}",
        subjects
            .iter()
            .map(|subject| labels
                .get(subject)
                .cloned()
                .unwrap_or_else(|| super::memory_context::short_subject(subject)))
            .collect::<Vec<_>>()
            .join("、")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn records_show_current_member_label_without_rewriting_old_name_in_memory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = GroupMemoryStore::new(dir.path(), "example-group").unwrap();
        let message = crate::model::social::ChatMessage {
            author_id: "alice".into(),
            author_name: Some("新昵称".into()),
            id: "source-message".into(),
            timestamp_ms: 1000,
            ..Default::default()
        };
        let labels = BTreeMap::from([(
            store.subject_alias("alice").unwrap(),
            "新昵称（M123456789012…）".into(),
        )]);
        operate_ledger(&store, &message, "add 旧昵称不吃辣", false, 1, &labels).unwrap();
        let draft = store.ledger_view().unwrap().drafts.remove(0);
        store
            .apply_operator(draft.id, draft.revision, chrono::Utc::now().timestamp())
            .unwrap();
        let before = std::fs::read(dir.path().join(store.memory_layout().canonical)).unwrap();
        let reply = operate_ledger(&store, &message, "records", false, 2, &labels)
            .unwrap()
            .unwrap();
        assert!(reply.contains("旧昵称不吃辣\n关联成员：新昵称（M123456789012…）"));
        assert_eq!(
            std::fs::read(dir.path().join(store.memory_layout().canonical)).unwrap(),
            before
        );
    }

    #[test]
    fn record_subject_display_preserves_global_scope_and_distinguishes_same_names() {
        let labels = BTreeMap::from([
            ("M1111111111111111".into(), "同名（M111111111111…）".into()),
            ("M2222222222222222".into(), "同名（M222222222222…）".into()),
        ]);
        assert_eq!(
            subject_summary(&[], &labels),
            "关联：通用（未绑定特定成员）"
        );
        assert_eq!(
            subject_summary(
                &["M1111111111111111".into(), "M2222222222222222".into()],
                &labels
            ),
            "关联成员：同名（M111111111111…）、同名（M222222222222…）"
        );
        assert_eq!(
            subject_summary(&["M3333333333333333".into()], &labels),
            "关联成员：M333333333333…"
        );
    }

    #[test]
    fn chat_confirmation_is_bound_to_actual_author_and_requires_a_second_message() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = GroupMemoryStore::new(dir.path(), "example-group").unwrap();
        operate(&store, "alice", "add 喜欢烤鱼", "msg1", 100).unwrap();
        let change = store.list_pending("alice").unwrap().remove(0);
        assert!(store.confirmed("alice").unwrap().is_empty());
        let command = format!("confirm {} {}", change.change_id, change.revision);
        assert_eq!(
            operate(&store, "bob", &command, "msg2", 200),
            Err(GroupMemoryError::NotOwner)
        );
        operate(&store, "alice", &command, "msg3", 300).unwrap();
        assert!(store.confirmed("alice").unwrap().is_empty());
        store.maintain(400).unwrap();
        assert_eq!(store.confirmed("alice").unwrap().len(), 1);
    }
}
