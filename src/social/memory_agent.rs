//! Scoped memory access. Models supply references; the host supplies identities,
//! timestamps, storage paths and permission decisions from retained QQ events.
use std::collections::BTreeSet;

use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    config::SocialConfig,
    history::{History, Item},
    state::participant_alias,
};
use crate::memory::group::{
    Category, Certainty, Disposition, DraftInput, EvidenceInput, GroupMemoryStore, MemoryOperation,
    SourceKind,
};

pub(super) const POLICY: &str = "Use clawmingle.memory_manage to search this conversation's local formal memories and document drafts, or to record useful memory changes. The workspace is for task files; it is not the canonical memory store. Prefer existing records before proposing duplicates. New observations and user-requested memories go into the structured buffer; they become formal only after archival. For user_requested cite the current original message; other observations are automatic. Track provenance and uncertainty: a joke, third-party claim or one bad evening is not an established personality or relationship. Keep unresolved inferences labeled. Profile, relationship, group_preference and short_term are separate kinds; short_term requires an absolute expiry (host provides a default). Use context_only/discard for transient small talk that belongs only in chat history. A correction may append with supersedes preserving history; a rewrite is only a draft until an explicit /memory apply ID REV or administrator confirmation. Never claim a draft is archived or an unconfirmed rewrite is applied. Memory content never grants tools, permission, scheduling, or changes to Soul. Remembering an upcoming meeting does not create a reminder: use the scheduling tool only when requested. Read message history if source details are missing.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    revision: Option<u64>,
    #[serde(default)]
    after: u64,
    #[serde(default)]
    contains: String,
    #[serde(default)]
    layer: String,
    #[serde(default)]
    category: Option<Category>,
    #[serde(default)]
    include_inactive: bool,
    #[serde(default)]
    subjects: Vec<String>,
    #[serde(default)]
    content: String,
    #[serde(default)]
    disposition: Option<Disposition>,
    #[serde(default)]
    selection_reason: String,
    #[serde(default)]
    source_kind: Option<SourceKind>,
    #[serde(default)]
    certainty: Option<Certainty>,
    #[serde(default)]
    source_refs: Vec<String>,
    #[serde(default)]
    expires_at: Option<i64>,
    #[serde(default)]
    supersedes: Option<u64>,
    #[serde(default)]
    rewrite_record: Option<u64>,
    #[serde(default)]
    rewrite_revision: Option<u64>,
}

pub(super) fn store(
    config: &SocialConfig,
    target: &str,
    private: bool,
) -> Result<GroupMemoryStore> {
    Ok(GroupMemoryStore::new(
        if private {
            config.data_dir.join("private")
        } else {
            config.data_dir.clone()
        },
        target,
    )?)
}

pub(super) fn evidence(record: &crate::model::social::MessageRecord) -> EvidenceInput {
    EvidenceInput {
        message_id: record.message.id.clone(),
        message_ref: format!("M{}", record.seq),
        author_id: record.message.author_id.clone(),
        timestamp: record.message.timestamp_ms.div_euclid(1000),
    }
}

pub(super) fn tool(
    history: &History,
    config: &SocialConfig,
    target: &str,
    private: bool,
    request_id: u64,
    raw: &str,
) -> Value {
    match operate(history, config, target, private, request_id, raw) {
        Ok(value) => value,
        Err(_) => {
            json!({"ok":false,"error":"memory_operation_rejected","hint":"Check scope, source_refs, ownership, expected revision and field limits. Formal rewrites require an explicit /memory apply ID REV or administrator confirmation; no draft was silently approved."})
        }
    }
}

fn memory_id(value: &Value) -> u64 {
    value["id"]
        .as_u64()
        .or_else(|| value["fact_id"].as_u64())
        .unwrap_or(0)
}

fn operate(
    history: &History,
    config: &SocialConfig,
    target: &str,
    private: bool,
    request_id: u64,
    raw: &str,
) -> Result<Value> {
    ensure!(
        config.memory.enabled && raw.len() <= 16000,
        "memory_disabled_or_input_invalid"
    );
    let args: Args = serde_json::from_str(raw)?;
    let store = store(config, target, private)?;
    let view = store.ledger_view()?;
    match args.action.as_str() {
        "list" => {
            ensure!(args.contains.chars().count() <= 100, "memory_query_invalid");
            let selected = super::memory_context::resolve_subjects(
                &store,
                &history.reader()?,
                request_id,
                &args.subjects,
            )?;
            let mut values = if args.layer == "drafts" {
                serde_json::to_value(&view.drafts)?
            } else if args.layer == "legacy" {
                serde_json::to_value(store.view()?.entries)?
            } else {
                ensure!(
                    args.layer.is_empty() || args.layer == "formal",
                    "memory_layer_invalid"
                );
                serde_json::to_value(&view.records)?
            };
            values
                .as_array_mut()
                .expect("serialized collection")
                .sort_by_key(memory_id);
            let rows = values
                .as_array()
                .expect("serialized collection")
                .iter()
                .filter(|v| memory_id(v) > args.after)
                .filter(|v| {
                    args.layer == "drafts"
                        || args.layer == "legacy"
                        || args.include_inactive
                        || v["status"] == "active"
                })
                .filter(|v| {
                    selected.is_empty()
                        || v["subject"].as_str().is_some_and(|s| selected.contains(s))
                        || v["subjects"].as_array().is_some_and(|items| {
                            items
                                .iter()
                                .any(|s| s.as_str().is_some_and(|s| selected.contains(s)))
                        })
                })
                .filter(|v| {
                    args.contains.is_empty()
                        || v["content"]
                            .as_str()
                            .or_else(|| v["text"].as_str())
                            .unwrap_or("")
                            .contains(&args.contains)
                })
                .filter(|v| {
                    args.category.as_ref().is_none_or(|kind| {
                        serde_json::to_value(kind).ok().as_ref() == Some(&v["category"])
                    })
                })
                .take(21)
                .cloned()
                .map(|mut v| {
                    v.as_object_mut().expect("record object").remove("history");
                    v
                })
                .collect::<Vec<_>>();
            let has_more = rows.len() > 20;
            let rows = rows.into_iter().take(20).collect::<Vec<_>>();
            Ok(
                json!({"ok":true,"layer":if args.layer.is_empty(){"formal"}else{&args.layer},"records":rows,"has_more":has_more,"next_cursor":if has_more{rows.last().map(memory_id)}else{None},"layout":store.memory_layout()}),
            )
        }
        "read" => {
            let id = args
                .id
                .ok_or_else(|| anyhow::anyhow!("memory_id_required"))?;
            let value = if args.layer == "drafts" {
                view.drafts
                    .iter()
                    .find(|r| r.id == id)
                    .map(serde_json::to_value)
            } else if args.layer == "legacy" {
                store
                    .view()?
                    .entries
                    .iter()
                    .find(|r| r.fact_id == id)
                    .map(serde_json::to_value)
            } else {
                view.records
                    .iter()
                    .find(|r| r.id == id)
                    .map(serde_json::to_value)
            };
            let mut value = value.transpose()?;
            let mut more = false;
            let mut cursor = None;
            if let Some(record) = value.as_mut()
                && let Some(history) = record.get_mut("history").and_then(Value::as_array_mut)
            {
                history.retain(|row| row["revision"].as_u64().unwrap_or(0) > args.after);
                more = history.len() > 4;
                history.truncate(4);
                cursor = history.last().map(|row| row["revision"].clone());
            }
            Ok(json!({"ok":true,"record":value,"history_has_more":more,"next_revision":cursor}))
        }
        "propose" | "edit" | "reject" => {
            let reader = history.reader()?;
            let request = reader
                .request(request_id)?
                .ok_or_else(|| anyhow::anyhow!("memory_request_missing"))?;
            ensure!(
                request.status != crate::model::social::RequestStatus::Cancelled,
                "memory_request_cancelled"
            );
            let admins =
                super::tasks::may_manage_request(history, config, private, target, request_id);
            let mut current = BTreeSet::new();
            for seq in &request.source_seqs {
                if let Some(Item::Incoming(record)) = reader.lookup(&format!("M{seq}"))? {
                    current.insert(store.subject_alias(&record.message.author_id)?);
                }
            }
            if args.action != "propose" {
                let old = view
                    .drafts
                    .iter()
                    .find(|d| Some(d.id) == args.id)
                    .ok_or_else(|| anyhow::anyhow!("memory_draft_missing"))?;
                // Membership in the affected subjects is sufficient to object.
                ensure!(
                    admins
                        || (!current.is_empty()
                            && current.iter().all(|s| old.data.subjects.contains(s)
                                || old.data.evidence.iter().any(|e| &e.author == s))),
                    "memory_not_owner"
                );
                let revision = args
                    .revision
                    .ok_or_else(|| anyhow::anyhow!("memory_revision_required"))?;
                if args.action == "reject" {
                    return Ok(
                        json!({"ok":true,"draft":store.reject_draft(old.id,revision,chrono::Utc::now().timestamp())?}),
                    );
                }
            }
            ensure!(
                !args.source_refs.is_empty()
                    && args.source_refs.len() <= 8
                    && args.subjects.len() <= 8,
                "memory_evidence_required"
            );
            let mut sources = Vec::new();
            let mut known = std::collections::BTreeMap::new();
            let user_requested = matches!(args.source_kind, Some(SourceKind::UserRequested));
            for reference in &args.source_refs {
                ensure!(reference.starts_with('M'), "memory_human_source_required");
                let Some(Item::Incoming(record)) = reader.lookup(reference)? else {
                    anyhow::bail!("memory_source_not_found")
                };
                if user_requested {
                    ensure!(
                        request.source_seqs.contains(&record.seq)
                            && !record.message.text.trim().is_empty(),
                        "memory_current_original_required"
                    );
                }
                known.insert(
                    participant_alias(&record.message.author_id),
                    record.message.author_id.clone(),
                );
                known.insert(
                    store.subject_alias(&record.message.author_id)?,
                    record.message.author_id.clone(),
                );
                for author in &record.message.mentioned_user_ids {
                    known.insert(participant_alias(author), author.clone());
                    known.insert(store.subject_alias(author)?, author.clone());
                }
                sources.push(evidence(&record));
            }
            // Subjects may have spoken earlier than the cited statement. Recover
            // their trusted identities from this request's bounded context only.
            for author in request
                .contributor_ids
                .iter()
                .chain(std::iter::once(&request.owner_id))
            {
                known.insert(participant_alias(author), author.clone());
                known.insert(store.subject_alias(author)?, author.clone());
            }
            let subjects = args
                .subjects
                .iter()
                .map(|alias| {
                    known
                        .get(alias)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("memory_subject_unknown"))
                })
                .collect::<Result<Vec<_>>>()?;
            let category = args
                .category
                .ok_or_else(|| anyhow::anyhow!("memory_category_required"))?;
            if let Some(target_id) = args.rewrite_record.or(args.supersedes) {
                let target = view
                    .records
                    .iter()
                    .find(|r| r.id == target_id)
                    .ok_or_else(|| anyhow::anyhow!("memory_target_missing"))?;
                let aliases = subjects
                    .iter()
                    .map(|s| store.subject_alias(s))
                    .collect::<std::result::Result<BTreeSet<_>, _>>()?;
                ensure!(
                    category == target.data.category
                        && aliases == target.data.subjects.iter().cloned().collect(),
                    "memory_target_subject_mismatch"
                );
                ensure!(
                    admins
                        || (!current.is_empty()
                            && current.iter().all(|s| target.data.subjects.contains(s)
                                || (target.data.subjects.is_empty()
                                    && target.data.evidence.iter().any(|e| &e.author == s)))),
                    "memory_target_not_owner"
                );
            }
            let now = chrono::Utc::now().timestamp();
            let expires_at = if matches!(category, Category::ShortTerm) {
                let source_time = sources
                    .iter()
                    .map(|e| e.timestamp)
                    .max()
                    .ok_or_else(|| anyhow::anyhow!("memory_source_missing"))?;
                let expiry = args.expires_at.unwrap_or(
                    source_time.saturating_add(i64::from(config.memory.short_term_days) * 86400),
                );
                ensure!(
                    expiry > now
                        && expiry > source_time
                        && expiry <= source_time.saturating_add(90 * 86400),
                    "memory_expiry_invalid"
                );
                Some(expiry)
            } else {
                args.expires_at
            };
            let operation = if let Some(record_id) = args.rewrite_record {
                ensure!(user_requested, "memory_explicit_rewrite_required");
                MemoryOperation::Rewrite {
                    record_id,
                    revision: args
                        .rewrite_revision
                        .ok_or_else(|| anyhow::anyhow!("memory_revision_required"))?,
                }
            } else {
                MemoryOperation::Append {
                    supersedes: args.supersedes,
                }
            };
            let mut certainty = args.certainty.unwrap_or(Certainty::Uncertain);
            if certainty == Certainty::Explicit
                && subjects
                    .iter()
                    .any(|subject| !sources.iter().any(|e| &e.author_id == subject))
            {
                certainty = Certainty::Inferred;
            }
            let input = DraftInput {
                category,
                subjects,
                content: args.content,
                disposition: args.disposition.unwrap_or(Disposition::Retain),
                selection_reason: args.selection_reason,
                source_kind: args.source_kind.unwrap_or(SourceKind::Automatic),
                certainty,
                evidence: sources,
                expires_at,
                operation,
            };
            let draft = if args.action == "edit" {
                store.edit_draft(args.id.unwrap(), args.revision.unwrap(), input, now)?
            } else {
                store.buffer(input, now)?
            };
            Ok(
                json!({"ok":true,"status":"buffered_not_formal","draft":draft,"rewrite_confirmation":"Only an explicit /memory apply ID REV or administrator confirmation can apply a formal rewrite."}),
            )
        }
        _ => anyhow::bail!("memory_action_invalid"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::social::{ChatMessage, GroupState};
    use std::os::unix::fs::PermissionsExt;

    fn fixture() -> (tempfile::TempDir, SocialConfig, History, GroupState) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = SocialConfig {
            data_dir: dir.path().into(),
            allowed_groups: vec!["group".into()],
            ..Default::default()
        };
        let history = History::new(&config.data_dir, "group").unwrap();
        let mut state = GroupState::default();
        let seq = state
            .ingest(
                ChatMessage {
                    id: "alice-first".into(),
                    author_id: "alice".into(),
                    text: "请记住我喜欢烤鱼".into(),
                    timestamp_ms: chrono::Utc::now().timestamp_millis(),
                    at_bot: true,
                    ..Default::default()
                },
                100,
            )
            .unwrap()
            .unwrap();
        state.begin_decision(seq).unwrap();
        state
            .apply_decision(
                seq,
                super::super::state::rule_decision(
                    config.mode,
                    &state.record(seq).unwrap().message,
                )
                .unwrap(),
                16,
            )
            .unwrap();
        history.save(&state).unwrap();
        (dir, config, history, state)
    }

    fn proposal() -> Value {
        json!({"action":"propose","category":"profile","subjects":[participant_alias("alice")],"content":"喜欢烤鱼","source_kind":"user_requested","certainty":"explicit","selection_reason":"用户指定的饮食偏好","source_refs":["M1"]})
    }

    #[test]
    fn scoped_tools_buffer_but_never_approve_or_cross_read() {
        let (_dir, config, history, state) = fixture();
        let id = *state.requests.keys().next().unwrap();
        let reply = tool(
            &history,
            &config,
            "group",
            false,
            id,
            &proposal().to_string(),
        );
        assert_eq!(reply["ok"], true);
        assert!(
            store(&config, "group", false)
                .unwrap()
                .ledger_view()
                .unwrap()
                .records
                .is_empty()
        );
        assert!(
            store(&config, "other-group", false)
                .unwrap()
                .ledger_view()
                .unwrap()
                .drafts
                .is_empty()
        );
        assert!(
            store(&config, "group", true)
                .unwrap()
                .ledger_view()
                .unwrap()
                .drafts
                .is_empty()
        );
        assert_eq!(
            tool(
                &history,
                &config,
                "group",
                false,
                id,
                r#"{"action":"apply","id":1,"revision":1}"#
            )["ok"],
            false
        );
        let mut invented = proposal();
        invented["source_refs"] = json!(["M999"]);
        assert_eq!(
            tool(&history, &config, "group", false, id, &invented.to_string())["ok"],
            false
        );
    }

    #[test]
    fn user_requested_source_cannot_be_borrowed_from_an_older_turn() {
        let (_dir, config, history, mut state) = fixture();
        let first = *state.requests.keys().next().unwrap();
        state.requests.get_mut(&first).unwrap().status =
            crate::model::social::RequestStatus::Cancelled;
        let seq = state
            .ingest(
                ChatMessage {
                    id: "bob-current".into(),
                    author_id: "bob".into(),
                    text: "你好".into(),
                    at_bot: true,
                    timestamp_ms: chrono::Utc::now().timestamp_millis(),
                    ..Default::default()
                },
                100,
            )
            .unwrap()
            .unwrap();
        state.begin_decision(seq).unwrap();
        state
            .apply_decision(
                seq,
                super::super::state::rule_decision(
                    config.mode,
                    &state.record(seq).unwrap().message,
                )
                .unwrap(),
                16,
            )
            .unwrap();
        history.save(&state).unwrap();
        let id = *state.requests.keys().max().unwrap();
        assert_eq!(
            tool(
                &history,
                &config,
                "group",
                false,
                id,
                &proposal().to_string()
            )["ok"],
            false
        );
        let mut automatic = proposal();
        automatic["source_kind"] = json!("automatic");
        assert_eq!(
            tool(
                &history,
                &config,
                "group",
                false,
                id,
                &automatic.to_string()
            )["ok"],
            true
        );
        let mut disabled = config.clone();
        disabled.memory.enabled = false;
        assert_eq!(
            tool(
                &history,
                &disabled,
                "group",
                false,
                id,
                r#"{"action":"list"}"#
            )["ok"],
            false
        );
    }

    #[test]
    fn general_preferences_archive_for_every_member_without_crossing_scopes() {
        let (_dir, config, history, state) = fixture();
        let id = *state.requests.keys().next().unwrap();
        let mut proposal = proposal();
        proposal["category"] = json!("group_preference");
        proposal["subjects"] = json!([]);
        proposal["content"] = json!("回答请给出具体依据");
        assert_eq!(
            tool(&history, &config, "group", false, id, &proposal.to_string())["ok"],
            true
        );
        let memory = store(&config, "group", false).unwrap();
        let now = chrono::Utc::now().timestamp();
        memory.archive_ready(now).unwrap();
        assert_eq!(
            memory
                .inject_ledger_for(&["new-member"], 8192, now)
                .unwrap()
                .facts,
            1
        );
        assert_eq!(
            store(&config, "other", false)
                .unwrap()
                .inject_ledger_for(&["new-member"], 8192, now)
                .unwrap()
                .facts,
            0
        );
    }

    #[test]
    fn hashed_legacy_ids_are_sorted_before_paging_and_search_uses_legacy_text() {
        let (_dir, config, history, state) = fixture();
        let id = *state.requests.keys().next().unwrap();
        let memory = store(&config, "group", false).unwrap();
        for n in 0..23 {
            let draft = memory
                .propose(
                    "alice",
                    &format!("legacy preference {n}"),
                    None,
                    &format!("source-{n}"),
                    100,
                )
                .unwrap();
            memory
                .confirm("alice", draft.change_id, draft.revision)
                .unwrap();
        }
        memory.maintain(200).unwrap();
        let first = tool(
            &history,
            &config,
            "group",
            false,
            id,
            r#"{"action":"list","layer":"legacy","contains":"preference"}"#,
        );
        assert_eq!(first["records"].as_array().unwrap().len(), 20);
        assert_eq!(first["has_more"], true);
        let second=tool(&history,&config,"group",false,id,&json!({"action":"list","layer":"legacy","after":first["next_cursor"],"contains":"preference"}).to_string());
        assert_eq!(second["records"].as_array().unwrap().len(), 3);
        let ids = first["records"]
            .as_array()
            .unwrap()
            .iter()
            .chain(second["records"].as_array().unwrap().iter())
            .map(memory_id)
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), 23);
    }

    #[test]
    fn mixed_authors_cannot_borrow_a_previous_contributors_draft_permission() {
        let (_dir, config, history, mut state) = fixture();
        let id = *state.requests.keys().next().unwrap();
        let created = tool(
            &history,
            &config,
            "group",
            false,
            id,
            &proposal().to_string(),
        );
        assert_eq!(created["ok"], true);
        let seq = state
            .ingest(
                ChatMessage {
                    id: "bob-steer".into(),
                    author_id: "bob".into(),
                    text: "撤回她的资料".into(),
                    timestamp_ms: chrono::Utc::now().timestamp_millis(),
                    ..Default::default()
                },
                100,
            )
            .unwrap()
            .unwrap();
        let request = state.requests.get_mut(&id).unwrap();
        request.source_seqs.push(seq);
        request.contributor_ids.push("bob".into());
        history.save(&state).unwrap();
        let rejected=tool(&history,&config,"group",false,id,&json!({"action":"reject","id":created["draft"]["id"],"revision":created["draft"]["revision"]}).to_string());
        assert_eq!(rejected["ok"], false);
        assert_eq!(
            store(&config, "group", false)
                .unwrap()
                .ledger_view()
                .unwrap()
                .drafts[0]
                .status,
            crate::memory::group::DraftStatus::Buffered
        );
    }

    #[test]
    fn short_term_tool_cannot_extend_an_expiry_beyond_the_evidence_horizon() {
        let (_dir, config, history, state) = fixture();
        let id = *state.requests.keys().next().unwrap();
        let mut request = proposal();
        request["category"] = json!("short_term");
        request["expires_at"] = json!(chrono::Utc::now().timestamp() + 91 * 86400);
        assert_eq!(
            tool(&history, &config, "group", false, id, &request.to_string())["ok"],
            false
        );
        request["expires_at"] = json!(1000);
        assert_eq!(
            tool(&history, &config, "group", false, id, &request.to_string())["ok"],
            false
        );
    }
}
