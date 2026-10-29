//! Deterministic chat controls. This module changes state only; it performs no I/O.

use anyhow::{Result, ensure};

use crate::model::social::{
    Action, Bubble, Decision, DecisionStatus, DeliveryStatus, GroupState, NextModel,
    ParticipationMode, RequestRecord, RequestStatus,
};

use super::{
    command_menu::{self, MenuReply},
    commands::ChatCommand,
    config::SocialConfig,
};

const NEXT_MODEL_LIFETIME_MS: i64 = 10 * 60 * 1000;
const MAX_NEXT_MODELS: usize = 64;
const MAX_CONTROL_REPLIES: usize = 16;

/// `private` describes an already isolated private conversation, not an
/// authorization shortcut for modifying somebody else's group or requests.
pub fn handle_command(
    state: &mut GroupState,
    seq: u64,
    command: ChatCommand,
    config: &SocialConfig,
    private: bool,
    target: &str,
    now: i64,
) -> Result<()> {
    match state.record(seq)?.status {
        DecisionStatus::Decided | DecisionStatus::Bypassed => return Ok(()),
        DecisionStatus::Pending => state.begin_decision(seq)?,
        DecisionStatus::Running => {}
        DecisionStatus::Failed => anyhow::bail!("command_not_pending"),
    }
    state.next_models.retain(|_, next| next.expires_at_ms > now);
    let author = state.record(seq)?.message.author_id.clone();
    let administrator = !private && config.is_scope_admin(&author, private, target);
    let may_configure = private || administrator;
    let command = match command {
        ChatCommand::StatusCode(ref code) => super::task_identity::resolve(state, code)
            .map(ChatCommand::StatusRequest)
            .unwrap_or(ChatCommand::Invalid(
                "当前对话没有这个任务，或它已移至聊天存档。",
            )),
        ChatCommand::CancelCode(ref code) => super::task_identity::resolve(state, code)
            .map(|id| ChatCommand::Cancel(Some(id)))
            .unwrap_or(ChatCommand::Invalid(
                "当前对话没有这个任务，或它已移至聊天存档。",
            )),
        command => command,
    };

    let menu = match &command {
        ChatCommand::Help => Some(command_menu::help(config.reply_format == "markdown")),
        ChatCommand::Status => Some(command_menu::status(
            state,
            config,
            private,
            &author,
            state.requests.values().filter(|r| social_busy(r)).count(),
            now,
        )),
        ChatCommand::StatusRequest(id) => {
            Some(super::task_status::detail_menu(state, *id, config, now))
        }
        ChatCommand::Mode(None) => Some(command_menu::modes(state, config, private, may_configure)),
        ChatCommand::Models | ChatCommand::Model(None) => {
            Some(command_menu::models(state, config, may_configure, 1, false))
        }
        ChatCommand::ModelMenu { page, next } => Some(command_menu::models(
            state,
            config,
            may_configure,
            *page,
            *next,
        )),
        _ => None,
    };
    if let Some(menu) = menu {
        return control_menu_reply(state, seq, menu, config, now);
    }
    let reply = match command {
        ChatCommand::Help
        | ChatCommand::Status
        | ChatCommand::StatusRequest(_)
        | ChatCommand::StatusCode(_)
        | ChatCommand::CancelCode(_)
        | ChatCommand::Mode(None)
        | ChatCommand::Models
        | ChatCommand::Model(None)
        | ChatCommand::ModelMenu { .. } => unreachable!("menu handled above"),
        ChatCommand::Ping => "Pong！ClawMingle 收发正常。".into(),
        ChatCommand::Mode(Some(name)) => match parse_mode(&name) {
            None => "未知参与模式，请使用 /mode 1、2、3 或 all。".into(),
            Some(mode) if private && mode != ParticipationMode::All => {
                "私聊固定使用 mode-all，回应每条消息。".into()
            }
            Some(_) if !may_configure => "只有群命令管理员可以修改群参与模式。".into(),
            Some(mode) => {
                state.mode_override = Some(mode);
                format!("参与模式已设为 {}。", mode_label(config, mode))
            }
        },
        ChatCommand::Model(Some(alias)) => {
            if !may_configure {
                "只有群命令管理员可以修改群默认模型；你可以使用 /model next 或 /ask。".into()
            } else if !config.model_presets.contains_key(&alias) {
                "未知模型别名，请使用 /models 查看可用模型。".into()
            } else {
                state.model_alias = Some(alias.clone());
                format!("默认模型已设为 {alias}。")
            }
        }
        ChatCommand::ModelNext(None) => {
            state.next_models.remove(&author);
            "已取消你下一次请求的模型指定。".into()
        }
        ChatCommand::ModelNext(Some(alias)) => {
            if !config.model_presets.contains_key(&alias) {
                "未知模型别名，请使用 /models 查看可用模型。".into()
            } else if state.next_models.len() >= MAX_NEXT_MODELS
                && !state.next_models.contains_key(&author)
            {
                "下一次模型预设已达到数量上限，请稍后再试。".into()
            } else {
                state.next_models.insert(
                    author.clone(),
                    NextModel {
                        alias: alias.clone(),
                        expires_at_ms: now.saturating_add(NEXT_MODEL_LIFETIME_MS),
                    },
                );
                format!("你的下一个新请求将使用 {alias}，10 分钟内有效；补充消息不消耗此设置。")
            }
        }
        ChatCommand::Ask { model, prompt } => {
            if !config.model_presets.contains_key(&model) {
                "未知模型别名，请使用 /models 查看可用模型。".into()
            } else if prompt.trim().is_empty() {
                "问题正文不能为空，请使用 /ask 模型别名 问题。".into()
            } else {
                state.apply_decision(
                    seq,
                    Decision {
                        action: Action::NewRequest,
                        source: "command_ask".into(),
                        audience_id: Some(author.clone()),
                        style: "answer".into(),
                        policy_version: 1,
                        snapshot_through_seq: seq,
                        ..Default::default()
                    },
                    config.max_active_requests,
                )?;
                if let Some(id) = state.record(seq)?.applied_request {
                    let request = state.requests.get_mut(&id).expect("new request retained");
                    request.model_alias = Some(model);
                    request.model_stamped = true;
                    return Ok(());
                }
                "当前请求队列已满，请稍后重新提问。".into()
            }
        }
        ChatCommand::Cancel(id) => cancel_request(state, id, &author, administrator, now)?,
        ChatCommand::Reset => {
            if !may_configure {
                "只有群命令管理员可以重置群会话。".into()
            } else if state.requests.values().any(social_busy)
                || state.messages.iter().any(|r| {
                    r.seq != seq
                        && matches!(r.status, DecisionStatus::Pending | DecisionStatus::Running)
                })
            {
                "当前仍有请求未结束或发送结果待核对，请处理完成后再重置会话。".into()
            } else {
                let start = seq
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("context_sequence_exhausted"))?;
                state.thread_id = None;
                for request in state.requests.values_mut() {
                    request.codex_thread_id = None;
                    request.codex_context = None;
                }
                state.context_start_seq = start;
                "会话上下文已重置，下一次请求将开始新会话。".into()
            }
        }
        ChatCommand::Invalid(message) => message.into(),
    };
    control_reply(state, seq, reply, config, now)
}

/// Freeze a new request's model once. A supplement never consumes a preference.
pub fn stamp_request_model(
    state: &mut GroupState,
    seq: u64,
    config: &SocialConfig,
    now: i64,
) -> Result<()> {
    state.next_models.retain(|_, next| next.expires_at_ms > now);
    let record = state.record(seq)?;
    if record.decision.as_ref().map(|decision| decision.action) != Some(Action::NewRequest) {
        return Ok(());
    }
    let Some(id) = record.applied_request else {
        return Ok(());
    };
    let explicit_ask = record
        .decision
        .as_ref()
        .is_some_and(|decision| decision.source == "command_ask");
    let request = state
        .requests
        .get(&id)
        .ok_or_else(|| anyhow::anyhow!("request_missing"))?;
    if request.control || request.source_seqs.as_slice() != [seq] || request.model_stamped {
        return Ok(());
    }
    let owner = request.owner_id.clone();
    let existing_alias = request.model_alias.clone();
    ensure!(
        !explicit_ask || existing_alias.is_some(),
        "explicit_model_alias_missing"
    );
    let consume_next = existing_alias.is_none() && state.next_models.contains_key(&owner);
    let alias = existing_alias
        .or_else(|| state.next_models.get(&owner).map(|next| next.alias.clone()))
        .or_else(|| state.model_alias.clone());
    if let Some(alias) = &alias {
        ensure!(
            config.model_presets.contains_key(alias),
            "model_preset_unknown"
        );
    }
    let request = state.requests.get_mut(&id).expect("request retained");
    request.model_alias = alias;
    request.model_stamped = true;
    if consume_next {
        state.next_models.remove(&owner);
    }
    Ok(())
}

/// A stamped request uses its own selection, including the base Codex model when
/// its alias is None. Later changes to the group default cannot change that turn.
pub fn effective_config(
    base: &SocialConfig,
    state: &GroupState,
    request_id: Option<u64>,
) -> Result<SocialConfig> {
    let mut config = base.clone();
    config.conversation_name = state.identity.as_ref().and_then(|i| i.display_name.clone());
    config.group_profile = state.group_profile.clone();
    if let Some(tools) = &state.tools_override {
        config.enabled_tools = tools.clone();
    }
    crate::codex::app_server::SocialTurnInput::validate_tools(&config.enabled_tools)?;
    super::integration_control::apply(&mut config, state)?;
    for skill in &state.enabled_skills {
        if !config.enabled_skills.contains(skill) {
            config.enabled_skills.push(skill.clone());
        }
    }
    ensure!(
        config.enabled_skills.len() <= 16
            && config.enabled_skills.iter().all(|name| !name.is_empty()
                && name.len() <= 32
                && name.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'_'
                    || b == b'-')),
        "invalid_enabled_skills"
    );
    config.mode = state.mode_override.unwrap_or(base.mode);
    let alias = match request_id {
        Some(id) => state
            .requests
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("request_missing"))?
            .model_alias
            .as_ref(),
        None => state.model_alias.as_ref(),
    };
    if let Some(alias) = alias {
        let preset = base
            .model_presets
            .get(alias)
            .ok_or_else(|| anyhow::anyhow!("model_preset_unknown"))?;
        config.codex.model = preset.model.clone();
        config.codex.reasoning_effort = preset.reasoning_effort.clone();
    }
    Ok(config)
}

pub(super) fn current_mode(
    state: &GroupState,
    config: &SocialConfig,
    private: bool,
) -> ParticipationMode {
    if private {
        ParticipationMode::All
    } else {
        state.mode_override.unwrap_or(config.mode)
    }
}

pub(super) fn mode_name(mode: ParticipationMode) -> &'static str {
    match mode {
        ParticipationMode::Mentions => "mode-1",
        ParticipationMode::Helpful => "mode-2",
        ParticipationMode::Social => "mode-3",
        ParticipationMode::All => "mode-all",
    }
}

fn parse_mode(name: &str) -> Option<ParticipationMode> {
    match name {
        "mode-1" => Some(ParticipationMode::Mentions),
        "mode-2" => Some(ParticipationMode::Helpful),
        "mode-3" => Some(ParticipationMode::Social),
        "mode-all" => Some(ParticipationMode::All),
        _ => None,
    }
}

fn social_busy(request: &RequestRecord) -> bool {
    !request.control
        && (!matches!(
            request.status,
            RequestStatus::Completed | RequestStatus::Cancelled | RequestStatus::Failed
        ) || request.bubbles.iter().any(|bubble| {
            matches!(
                bubble.status,
                DeliveryStatus::Pending | DeliveryStatus::Sending | DeliveryStatus::Unknown
            )
        }))
}

pub(super) fn cancel_request(
    state: &mut GroupState,
    id: Option<u64>,
    author: &str,
    administrator: bool,
    now: i64,
) -> Result<String> {
    let id = id.or_else(|| {
        state
            .requests
            .values()
            .rev()
            .find(|request| {
                social_busy(request)
                    && request.cancelled_revision.is_none()
                    && (request.owner_id == author || administrator)
            })
            .map(|request| request.id)
    });
    let Some(request) = id.and_then(|id| state.requests.get_mut(&id)) else {
        return Ok("当前没有可取消的请求。".into());
    };
    if request.control || request.owner_id != author && !administrator {
        return Ok("只能取消自己发起的请求；群命令管理员可以取消群内请求。".into());
    }
    if request.cancelled_revision.is_some() {
        return Ok("该请求的后续回复已经取消。".into());
    }
    if !social_busy(request) {
        return Ok("该请求已结束，无需取消。".into());
    }
    request.revision = request
        .revision
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("request_revision_exhausted"))?;
    request.cancelled_revision = Some(request.revision);
    request.updated_at_ms = now;
    for bubble in &mut request.bubbles {
        if bubble.status == DeliveryStatus::Pending {
            bubble.status = DeliveryStatus::Cancelled;
        }
    }
    if request.status == RequestStatus::UnknownDelivery
        || request
            .bubbles
            .iter()
            .any(|bubble| bubble.status == DeliveryStatus::Unknown)
    {
        request.status = RequestStatus::UnknownDelivery;
        Ok("已取消后续回复；已有消息的发送结果仍需核对。".into())
    } else if request.status == RequestStatus::Sending
        || request
            .bubbles
            .iter()
            .any(|bubble| bubble.status == DeliveryStatus::Sending)
    {
        request.status = RequestStatus::Sending;
        Ok("已取消后续回复；正在发送的消息可能仍会送达。".into())
    } else {
        request.status = RequestStatus::Cancelled;
        Ok("已取消该请求的后续回复。".into())
    }
}

pub(super) fn control_reply(
    state: &mut GroupState,
    seq: u64,
    text: String,
    config: &SocialConfig,
    now: i64,
) -> Result<()> {
    let menu = command_menu::result(
        &state.record(seq)?.message.text,
        text,
        config.reply_format == "markdown",
    );
    control_menu_reply(state, seq, menu, config, now)
}

pub(super) fn control_menu_reply(
    state: &mut GroupState,
    seq: u64,
    menu: MenuReply,
    config: &SocialConfig,
    now: i64,
) -> Result<()> {
    let message = state.record(seq)?.message.clone();
    let decision = Decision {
        action: Action::Silent,
        source: "command".into(),
        policy_version: 1,
        snapshot_through_seq: seq,
        ..Default::default()
    };
    if state.record(seq)?.status == DecisionStatus::Running {
        state.apply_decision(seq, decision.clone(), config.max_active_requests)?;
    }
    // An /ask rejected by the queue limit has already advanced the cursor. It
    // becomes a control reply, rather than retaining a failed model request route.
    let record = state
        .messages
        .iter_mut()
        .find(|r| r.seq == seq)
        .expect("message retained");
    record.status = DecisionStatus::Bypassed;
    record.decision = Some(decision);
    record.application = Some("command".into());
    let pending = state
        .requests
        .values()
        .filter(|request| {
            request.control
                && request.bubbles.iter().any(|bubble| {
                    matches!(
                        bubble.status,
                        DeliveryStatus::Pending | DeliveryStatus::Sending | DeliveryStatus::Unknown
                    )
                })
        })
        .count();
    if pending >= MAX_CONTROL_REPLIES {
        state
            .messages
            .iter_mut()
            .find(|r| r.seq == seq)
            .expect("message retained")
            .application = Some("command_reply_queue_full".into());
        return Ok(());
    }
    let id = state.next_request;
    state.next_request = id
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("request_sequence_exhausted"))?;
    state.requests.insert(
        id,
        RequestRecord {
            id,
            owner_id: message.author_id.clone(),
            contributor_ids: vec![message.author_id.clone()],
            audience_id: Some(message.author_id),
            source_seqs: vec![seq],
            revision: 1,
            status: RequestStatus::Ready,
            updated_at_ms: now,
            control: true,
            bubbles: vec![Bubble {
                origin: crate::model::social::BubbleOrigin::ControlReply,
                revision: 1,
                markdown: config.reply_format == "markdown",
                command_buttons: if config.reply_format == "markdown" {
                    menu.buttons
                } else {
                    Vec::new()
                },
                text: menu.text,
                passive_message_id: message.id,
                msg_seq: 1,
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    let record = state
        .messages
        .iter_mut()
        .find(|r| r.seq == seq)
        .expect("message retained");
    record.applied_request = Some(id);
    record.applied_revision = Some(1);
    Ok(())
}

fn mode_label(config: &SocialConfig, mode: ParticipationMode) -> String {
    format!(
        "{}（{}）",
        mode_name(mode),
        config.behavior.profile(mode).name
    )
}

pub(super) fn effective_config_for_scope(
    base: &SocialConfig,
    state: &GroupState,
    request_id: Option<u64>,
    private: bool,
) -> Result<SocialConfig> {
    let mut config = effective_config(base, state, request_id)?;
    if private {
        config.mode = ParticipationMode::All;
        config.group_profile = None;
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::social::ChatMessage, social::commands::parse_command};

    fn config() -> SocialConfig {
        SocialConfig {
            command_admins: vec!["admin".into()],
            ..Default::default()
        }
    }

    fn ingest(state: &mut GroupState, author: &str, text: &str) -> u64 {
        state
            .ingest(
                ChatMessage {
                    id: format!("message-{}", state.next_seq),
                    author_id: author.into(),
                    text: text.into(),
                    timestamp_ms: 1000,
                    ..Default::default()
                },
                2000,
            )
            .unwrap()
            .unwrap()
    }

    fn command(state: &mut GroupState, author: &str, text: &str, private: bool, now: i64) -> u64 {
        let seq = ingest(state, author, text);
        handle_command(
            state,
            seq,
            parse_command(text).unwrap(),
            &config(),
            private,
            "test-group",
            now,
        )
        .unwrap();
        seq
    }

    fn new_request(state: &mut GroupState, author: &str) -> (u64, u64) {
        let seq = ingest(state, author, "普通请求");
        state.begin_decision(seq).unwrap();
        state
            .apply_decision(
                seq,
                Decision {
                    action: Action::NewRequest,
                    source: "mention".into(),
                    ..Default::default()
                },
                32,
            )
            .unwrap();
        (seq, state.record(seq).unwrap().applied_request.unwrap())
    }

    fn last_control(state: &GroupState) -> &RequestRecord {
        state.requests.values().rev().find(|r| r.control).unwrap()
    }

    #[test]
    fn ping_enters_delivery_without_any_model_progress() {
        let mut state = GroupState::default();
        let seq = command(&mut state, "user", "/ping", false, 1000);
        let request = last_control(&state);
        assert!(request.control);
        assert_eq!(request.status, RequestStatus::Ready);
        assert_eq!(request.revision, 1);
        assert_eq!(
            (
                request.submitted_revision,
                request.accepted_revision,
                request.generated_revision
            ),
            (0, 0, 0)
        );
        assert_eq!(request.bubbles.len(), 1);
        assert_eq!(request.bubbles[0].status, DeliveryStatus::Pending);
        assert_eq!(request.bubbles[0].passive_message_id, "message-1");
        assert_eq!(request.bubbles[0].msg_seq, 1);
        assert!(request.bubbles[0].text.contains("Pong"));
        let record = state.record(seq).unwrap();
        assert_eq!(record.status, DecisionStatus::Bypassed);
        assert_eq!(record.application.as_deref(), Some("command"));
        assert_eq!(record.decision.as_ref().unwrap().source, "command");
        assert_eq!(record.decision.as_ref().unwrap().action, Action::Silent);
        assert_eq!(state.decided_through_seq, seq);
    }

    #[test]
    fn help_status_and_model_queries_are_open_to_group_members() {
        let mut state = GroupState::default();
        for text in ["/help", "/status", "/models", "/mode", "/model"] {
            command(&mut state, "ordinary", text, false, 1000);
            assert!(
                !last_control(&state).bubbles[0]
                    .text
                    .contains("只有群命令管理员")
            );
        }
        assert_eq!(state.mode_override, None);
        assert_eq!(state.model_alias, None);
    }

    #[test]
    fn group_configuration_changes_require_an_administrator() {
        let mut state = GroupState {
            thread_id: Some("existing-thread".into()),
            ..Default::default()
        };
        for text in ["/mode 3", "/model luna", "/reset"] {
            command(&mut state, "ordinary", text, false, 1000);
            assert!(
                last_control(&state).bubbles[0]
                    .text
                    .contains("只有群命令管理员")
            );
        }
        assert_eq!(state.mode_override, None);
        assert_eq!(state.model_alias, None);
        assert_eq!(state.thread_id.as_deref(), Some("existing-thread"));
        command(&mut state, "admin", "/mode 3", false, 1000);
        command(&mut state, "admin", "/model luna", false, 1000);
        assert_eq!(state.mode_override, Some(ParticipationMode::Social));
        assert_eq!(state.model_alias.as_deref(), Some("luna"));
    }

    #[test]
    fn private_configuration_is_local_and_mode_is_always_all() {
        let mut private = GroupState::default();
        let group = GroupState::default();
        command(&mut private, "ordinary", "/mode 2", true, 1000);
        assert_eq!(private.mode_override, None);
        command(&mut private, "ordinary", "/mode all", true, 1000);
        command(&mut private, "ordinary", "/model luna", true, 1000);
        assert_eq!(private.mode_override, Some(ParticipationMode::All));
        assert_eq!(private.model_alias.as_deref(), Some("luna"));
        assert_eq!(group.model_alias, None);
        assert_eq!(group.mode_override, None);
    }

    #[test]
    fn one_time_selection_is_personal_and_consumed_only_once() {
        let mut state = GroupState {
            model_alias: Some("sol".into()),
            ..Default::default()
        };
        command(&mut state, "alice", "/model next luna", false, 1000);
        let (bob_seq, bob) = new_request(&mut state, "bob");
        stamp_request_model(&mut state, bob_seq, &config(), 1001).unwrap();
        assert_eq!(state.requests[&bob].model_alias.as_deref(), Some("sol"));
        assert!(state.next_models.contains_key("alice"));
        let (alice_seq, alice) = new_request(&mut state, "alice");
        stamp_request_model(&mut state, alice_seq, &config(), 1001).unwrap();
        assert_eq!(state.requests[&alice].model_alias.as_deref(), Some("luna"));
        assert!(!state.next_models.contains_key("alice"));
        command(&mut state, "alice", "/model next sol", false, 1002);
        stamp_request_model(&mut state, alice_seq, &config(), 1003).unwrap();
        assert_eq!(state.next_models["alice"].alias, "sol");
        assert_eq!(
            state.record(alice_seq).unwrap().application.as_deref(),
            Some("created")
        );
    }

    #[test]
    fn supplements_do_not_consume_the_next_request_selection() {
        let mut state = GroupState::default();
        let (_, request) = new_request(&mut state, "alice");
        command(&mut state, "alice", "/model next luna", false, 1000);
        let seq = ingest(&mut state, "alice", "追加要求");
        state.begin_decision(seq).unwrap();
        state
            .apply_decision(
                seq,
                Decision {
                    action: Action::Append,
                    request_id: Some(request),
                    source: "jev".into(),
                    ..Default::default()
                },
                16,
            )
            .unwrap();
        stamp_request_model(&mut state, seq, &config(), 1001).unwrap();
        assert_eq!(state.next_models["alice"].alias, "luna");
    }

    #[test]
    fn expiry_at_ten_minutes_falls_back_to_the_group_default() {
        let mut state = GroupState {
            model_alias: Some("sol".into()),
            ..Default::default()
        };
        command(&mut state, "alice", "/model next luna", false, 1000);
        let (seq, request) = new_request(&mut state, "alice");
        stamp_request_model(&mut state, seq, &config(), 601_000).unwrap();
        assert_eq!(state.requests[&request].model_alias.as_deref(), Some("sol"));
        assert!(!state.next_models.contains_key("alice"));
    }

    #[test]
    fn explicit_ask_preserves_raw_message_and_does_not_consume_next() {
        let mut state = GroupState::default();
        command(&mut state, "alice", "/model next luna", false, 1000);
        let seq = command(&mut state, "alice", "/ask sol 第一行\n第二行", false, 1001);
        let record = state.record(seq).unwrap();
        let request = state
            .requests
            .get(&record.applied_request.unwrap())
            .unwrap();
        assert!(!request.control);
        assert!(request.model_stamped);
        assert_eq!(request.model_alias.as_deref(), Some("sol"));
        assert_eq!(record.decision.as_ref().unwrap().source, "command_ask");
        assert_eq!(record.message.text, "/ask sol 第一行\n第二行");
        stamp_request_model(&mut state, seq, &config(), 1002).unwrap();
        assert_eq!(state.next_models["alice"].alias, "luna");
    }

    #[test]
    fn unknown_aliases_never_fall_back_or_replace_a_valid_selection() {
        let mut state = GroupState {
            model_alias: Some("sol".into()),
            ..Default::default()
        };
        command(&mut state, "alice", "/model next luna", false, 1000);
        for text in ["/model missing", "/model next missing", "/ask missing 问题"] {
            command(&mut state, "admin", text, false, 1001);
            assert!(
                last_control(&state).bubbles[0]
                    .text
                    .contains("未知模型别名")
            );
        }
        assert_eq!(state.model_alias.as_deref(), Some("sol"));
        assert_eq!(state.next_models["alice"].alias, "luna");
        let (seq, request) = new_request(&mut state, "alice");
        state.next_models.get_mut("alice").unwrap().alias = "removed-preset".into();
        assert!(stamp_request_model(&mut state, seq, &config(), 1002).is_err());
        assert!(!state.requests[&request].model_stamped);
        assert!(state.next_models.contains_key("alice"));
    }

    #[test]
    fn next_off_cancels_only_the_current_authors_preference() {
        let mut state = GroupState::default();
        command(&mut state, "alice", "/model next luna", false, 1000);
        command(&mut state, "bob", "/model next sol", false, 1000);
        command(&mut state, "alice", "/model next off", false, 1001);
        assert!(!state.next_models.contains_key("alice"));
        assert_eq!(state.next_models["bob"].alias, "sol");
    }

    #[test]
    fn effective_model_is_frozen_per_request_and_unknown_is_an_error() {
        let mut state = GroupState::default();
        let base = config();
        let (seq, request) = new_request(&mut state, "alice");
        stamp_request_model(&mut state, seq, &base, 1000).unwrap();
        state.model_alias = Some("luna".into());
        state.mode_override = Some(ParticipationMode::Social);
        let request_config = effective_config(&base, &state, Some(request)).unwrap();
        assert_eq!(request_config.codex.model, base.codex.model);
        assert_eq!(request_config.mode, ParticipationMode::Social);
        assert_eq!(
            effective_config(&base, &state, None).unwrap().codex.model,
            base.model_presets["luna"].model
        );
        state.requests.get_mut(&request).unwrap().model_alias = Some("missing".into());
        assert!(effective_config(&base, &state, Some(request)).is_err());
        assert!(effective_config(&base, &state, Some(999)).is_err());
    }

    #[test]
    fn cancellation_requires_owner_or_group_administrator() {
        let mut state = GroupState::default();
        let (_, request) = new_request(&mut state, "alice");
        command(
            &mut state,
            "bob",
            &format!("/cancel R{request}"),
            false,
            1000,
        );
        assert_eq!(state.requests[&request].status, RequestStatus::Queued);
        command(
            &mut state,
            "admin",
            &format!("/cancel R{request}"),
            true,
            1000,
        );
        assert_eq!(state.requests[&request].status, RequestStatus::Queued);
        command(
            &mut state,
            "admin",
            &format!("/cancel R{request}"),
            false,
            1000,
        );
        assert_eq!(state.requests[&request].status, RequestStatus::Cancelled);
        assert_eq!(state.requests[&request].cancelled_revision, Some(2));
    }

    #[test]
    fn timestamp_status_and_cancel_reuse_the_same_scoped_authorization() {
        let mut state = GroupState::default();
        let (_, id) = new_request(&mut state, "alice");
        let code = super::super::task_identity::label(&state.requests[&id]);
        assert!(!code.starts_with('R'));
        command(&mut state, "bob", &format!("/status {code}"), false, 2000);
        assert!(last_control(&state).bubbles[0].text.contains(&code));
        command(&mut state, "bob", &format!("/cancel {code}"), false, 3000);
        assert_eq!(state.requests[&id].status, RequestStatus::Queued);
        command(&mut state, "alice", &format!("/cancel {code}"), false, 4000);
        assert_eq!(state.requests[&id].status, RequestStatus::Cancelled);
        assert_eq!(
            super::super::task_identity::label(&state.requests[&id]),
            code
        );
    }

    #[test]
    fn cancel_without_id_targets_latest_owned_social_request() {
        let mut state = GroupState::default();
        let (_, first) = new_request(&mut state, "alice");
        let (_, latest) = new_request(&mut state, "alice");
        let (_, other) = new_request(&mut state, "bob");
        command(&mut state, "alice", "/ping", false, 1000);
        command(&mut state, "alice", "/cancel", false, 1000);
        assert_eq!(state.requests[&first].status, RequestStatus::Queued);
        assert_eq!(state.requests[&latest].status, RequestStatus::Cancelled);
        assert_eq!(state.requests[&other].status, RequestStatus::Queued);
    }

    #[test]
    fn cancel_preserves_unknown_and_sending_bubbles() {
        for (delivery, status) in [
            (DeliveryStatus::Unknown, RequestStatus::UnknownDelivery),
            (DeliveryStatus::Sending, RequestStatus::Sending),
        ] {
            let mut state = GroupState::default();
            let (_, request) = new_request(&mut state, "alice");
            state.requests.get_mut(&request).unwrap().status = status;
            state.requests.get_mut(&request).unwrap().bubbles = vec![
                Bubble {
                    revision: 1,
                    status: delivery,
                    ..Default::default()
                },
                Bubble {
                    revision: 1,
                    status: DeliveryStatus::Pending,
                    ..Default::default()
                },
                Bubble {
                    revision: 1,
                    status: DeliveryStatus::Sent,
                    ..Default::default()
                },
            ];
            command(&mut state, "alice", "/cancel", false, 1000);
            let request = &state.requests[&request];
            assert_eq!(request.status, status);
            assert_eq!(request.cancelled_revision, Some(2));
            assert_eq!(request.bubbles[0].status, delivery);
            assert_eq!(request.bubbles[1].status, DeliveryStatus::Cancelled);
            assert_eq!(request.bubbles[2].status, DeliveryStatus::Sent);
        }
    }

    #[test]
    fn reset_refuses_live_work_and_unknown_deliveries() {
        for status in [
            RequestStatus::Queued,
            RequestStatus::Generating,
            RequestStatus::Ready,
            RequestStatus::Sending,
            RequestStatus::UnknownDelivery,
        ] {
            let mut state = GroupState {
                thread_id: Some("old-thread".into()),
                ..Default::default()
            };
            let (_, request) = new_request(&mut state, "alice");
            state.requests.get_mut(&request).unwrap().status = status;
            command(&mut state, "admin", "/reset", false, 1000);
            assert_eq!(state.thread_id.as_deref(), Some("old-thread"));
            assert_eq!(state.context_start_seq, 0);
        }
    }

    #[test]
    fn idle_reset_keeps_history_settings_and_pending_control_replies() {
        let mut state = GroupState {
            group_profile: Some(crate::model::social::GroupProfile {
                revision: 4,
                ..Default::default()
            }),
            thread_id: Some("old-thread".into()),
            model_alias: Some("luna".into()),
            ..Default::default()
        };
        command(&mut state, "alice", "/ping", false, 1000);
        let original_messages = state.messages.len();
        let seq = command(&mut state, "admin", "/reset", false, 1001);
        assert_eq!(state.thread_id, None);
        assert_eq!(state.context_start_seq, seq + 1);
        assert_eq!(state.group_profile.as_ref().unwrap().revision, 4);
        assert_eq!(state.messages.len(), original_messages + 1);
        assert_eq!(state.model_alias.as_deref(), Some("luna"));
        assert_eq!(state.requests.len(), 2);
    }

    #[test]
    fn reset_refuses_an_undecided_message_even_before_request_creation() {
        let mut state = GroupState {
            thread_id: Some("old-thread".into()),
            ..Default::default()
        };
        state
            .ingest(
                crate::model::social::ChatMessage {
                    id: "pending-message".into(),
                    author_id: "alice".into(),
                    text: "hello".into(),
                    ..Default::default()
                },
                500,
            )
            .unwrap();
        command(&mut state, "admin", "/reset", false, 1000);
        assert_eq!(state.thread_id.as_deref(), Some("old-thread"));
        assert_eq!(state.context_start_seq, 0);
    }

    #[test]
    fn command_replay_does_not_duplicate_controls_or_effects() {
        let mut state = GroupState::default();
        let seq = command(&mut state, "alice", "/ping", false, 1000);
        handle_command(
            &mut state,
            seq,
            ChatCommand::Ping,
            &config(),
            false,
            "test-group",
            1001,
        )
        .unwrap();
        assert_eq!(state.requests.len(), 1);
        assert_eq!(state.record(seq).unwrap().attempts, 1);
    }

    #[test]
    fn preference_and_control_reply_limits_are_bounded() {
        let mut state = GroupState::default();
        for index in 0..MAX_NEXT_MODELS {
            state.next_models.insert(
                format!("user-{index}"),
                NextModel {
                    alias: "sol".into(),
                    expires_at_ms: 100_000,
                },
            );
        }
        command(&mut state, "extra", "/model next luna", false, 1000);
        assert_eq!(state.next_models.len(), MAX_NEXT_MODELS);
        assert!(!state.next_models.contains_key("extra"));
        command(&mut state, "user-0", "/model next luna", false, 1000);
        assert_eq!(state.next_models["user-0"].alias, "luna");
        for _ in 0..MAX_CONTROL_REPLIES {
            command(&mut state, "alice", "/ping", false, 1000);
        }
        assert_eq!(
            state.requests.values().filter(|r| r.control).count(),
            MAX_CONTROL_REPLIES
        );
        let record = state.messages.last().unwrap();
        assert_eq!(
            record.application.as_deref(),
            Some("command_reply_queue_full")
        );
        assert_eq!(state.decided_through_seq, record.seq);
    }
}
