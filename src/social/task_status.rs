//! Immediate, conversation-scoped task queries. No model invocation or task rerun.
use anyhow::Result;

use super::{
    command_menu::{self, MenuReply},
    config::SocialConfig,
    control,
};
use crate::model::social::{
    BubbleOrigin, ChatMessage, DecisionStatus, DeliveryStatus, GroupState, ParticipationMode,
    RequestRecord, RequestStatus,
};

const OVERVIEW_LIMIT: usize = 3;

fn active(request: &RequestRecord) -> bool {
    !matches!(
        request.status,
        RequestStatus::Completed | RequestStatus::Cancelled | RequestStatus::Failed
    )
}

fn owns(request: &RequestRecord, author: &str) -> bool {
    request.owner_id == author || request.contributor_ids.iter().any(|id| id == author)
}

fn status_label(request: &RequestRecord) -> &'static str {
    if request.failure_code.is_some() && request.status != RequestStatus::Cancelled {
        return "处理失败，尚未生成完整结果";
    }
    match request.status {
        RequestStatus::Queued => "已接收，排队等待处理",
        RequestStatus::Generating => "正在处理，尚未完成",
        RequestStatus::Ready if request.awaiting_input => "正在准备发送补充问题",
        RequestStatus::Ready => "结果已生成，等待发送",
        RequestStatus::Sending => "正在发送回复",
        RequestStatus::Completed => "已完成，结果已送达",
        RequestStatus::Cancelled => "已取消",
        RequestStatus::WaitingForUser => "等待你补充信息",
        RequestStatus::AwaitingDelivery => "结果已生成，保留待取回",
        RequestStatus::UnknownDelivery => "发送结果待核对，请勿重复执行",
        RequestStatus::Failed
            if request.generated_revision >= request.revision
                && request.bubbles.iter().any(|bubble| {
                    bubble.revision == request.revision
                        && matches!(
                            bubble.origin,
                            BubbleOrigin::ModelAnswer | BubbleOrigin::Clarification
                        )
                }) =>
        {
            "结果已生成，但回复投递失败"
        }
        RequestStatus::Failed => "处理失败，尚未生成完整结果",
    }
}

/// Source and progress are user-facing prose, never backend errors or tool logs.
/// Suppress credential-bearing excerpts and links rather than repeat them in menus.
fn excerpt(value: &str, limit: usize) -> String {
    let lower = value.to_ascii_lowercase();
    if [
        "api_key",
        "apikey",
        "access_token",
        "refresh_token",
        "authorization",
        "cookie",
        "password",
        "secret",
        "token=",
        "密钥",
        "密码",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return "（含配置或凭据的内容已省略）".into();
    }
    let display = crate::util::text::display_mentions(value);
    let line = display
        .split_whitespace()
        .map(|word| {
            if word.contains("://")
                || word.starts_with("sk-")
                || word.starts_with("/home/")
                || word.starts_with("/root/")
                || word.starts_with("/etc/")
            {
                "[内容已省略]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let line = line
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>();
    let mut short = line.chars().take(limit).collect::<String>();
    if line.chars().count() > limit {
        short.push('…');
    }
    short
}

fn source(state: &GroupState, request: &RequestRecord, limit: usize) -> String {
    request
        .source_seqs
        .iter()
        .find_map(|seq| state.record(*seq).ok())
        .map(|record| {
            let text = excerpt(&record.message.text, limit);
            if !text.is_empty() {
                text
            } else if !record.message.voices.is_empty() {
                "语音请求".into()
            } else if !record.message.images.is_empty() {
                "图片请求".into()
            } else {
                "先前的请求".into()
            }
        })
        .unwrap_or_else(|| "原消息已移至聊天存档".into())
}

fn age(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    match seconds {
        0..=59 => format!("{seconds} 秒"),
        60..=3599 => format!("{} 分 {} 秒", seconds / 60, seconds % 60),
        3600..=86399 => format!("{} 小时 {} 分", seconds / 3600, seconds % 3600 / 60),
        _ => format!("{} 天 {} 小时", seconds / 86400, seconds % 86400 / 3600),
    }
}

fn timestamp(at: i64, now: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at)
        .map(|time| {
            format!(
                "{}（北京时间，{}前）",
                time.with_timezone(&chrono_tz::Asia::Shanghai)
                    .format("%m-%d %H:%M:%S"),
                age(now.saturating_sub(at))
            )
        })
        .unwrap_or_else(|| "时间未记录".into())
}

fn details(state: &GroupState, request: &RequestRecord, now: i64, brief: bool) -> String {
    let feedback = request
        .feedback
        .as_ref()
        .filter(|feedback| feedback.revision == request.revision);
    let accepted = feedback.map(|value| value.accepted_at_ms).or_else(|| {
        request
            .source_seqs
            .iter()
            .filter_map(|seq| state.record(*seq).ok())
            .map(|record| record.message.timestamp_ms)
            .min()
    });
    let elapsed = accepted
        .map(|at| {
            let end = if active(request) {
                now
            } else {
                request
                    .bubbles
                    .iter()
                    .filter_map(|b| b.sent_at_ms)
                    .chain(feedback.and_then(|f| f.last_activity_at_ms))
                    .max()
                    .unwrap_or(request.updated_at_ms)
                    .max(at)
                    .min(now)
            };
            age(end.saturating_sub(at))
        })
        .unwrap_or_else(|| "未记录".into());
    let mut text = format!(
        "{} · {}\n原请求：{}\n经过时间：{}",
        super::task_identity::label(request),
        status_label(request),
        source(state, request, if brief { 36 } else { 80 }),
        elapsed
    );
    if request.failure_code.is_some() {
        let delivery = request
            .bubbles
            .iter()
            .rev()
            .find(|bubble| {
                bubble.revision == request.revision && bubble.origin == BubbleOrigin::FailureNotice
            })
            .map(|bubble| match bubble.status {
                DeliveryStatus::Pending => "等待发送",
                DeliveryStatus::Sending => "正在发送",
                DeliveryStatus::Sent => "已送达",
                DeliveryStatus::Failed => "投递失败",
                DeliveryStatus::Unknown => "发送结果待核对",
                DeliveryStatus::Cancelled => "已取消发送",
            })
            .unwrap_or("尚未记录");
        text.push_str(&format!("\n失败通知：{delivery}"));
    }
    if !brief && let Some(at) = feedback.and_then(|value| value.started_at_ms) {
        text.push_str(&format!("\n开始处理：{}", timestamp(at, now)));
    }
    let progress = feedback
        .filter(|value| !value.last_progress_text.trim().is_empty())
        .map(|value| (value.last_progress_text.as_str(), value.last_progress_at_ms))
        .or_else(|| {
            request
                .bubbles
                .iter()
                .rev()
                .find(|bubble| {
                    bubble.revision == request.revision
                        && bubble.origin == BubbleOrigin::TaskProgress
                })
                .map(|bubble| (bubble.text.as_str(), bubble.sent_at_ms))
        });
    if let Some((body, at)) = progress {
        text.push_str(&format!(
            "\n最近进展：{}",
            excerpt(body, if brief { 80 } else { 240 })
        ));
        if !brief && let Some(at) = at {
            text.push_str(&format!("\n进展时间：{}", timestamp(at, now)));
        }
    } else {
        text.push_str("\n最近进展：尚未记录新的实质进展");
    }
    text.push_str(&format!(
        "\n最近执行动态：{}",
        feedback
            .and_then(|value| value.last_activity_at_ms)
            .map(|at| timestamp(at, now))
            .unwrap_or_else(|| "尚无记录".into())
    ));
    if request.status == RequestStatus::Generating
        && feedback
            .and_then(|value| value.last_activity_at_ms)
            .is_none()
    {
        text.push_str("\n尚无执行动态，暂不能确认当前执行是否正常。");
    }
    let remaining = request
        .bubbles
        .iter()
        .filter(|b| {
            b.revision == request.revision
                && !b.origin.is_progress()
                && b.status == DeliveryStatus::Pending
        })
        .count();
    if remaining > 0
        && matches!(
            request.status,
            RequestStatus::AwaitingDelivery | RequestStatus::Ready
        )
    {
        text.push_str(&format!("\n尚待发送：{remaining} 条"));
    }
    if let Some(issue) = request
        .bubbles
        .iter()
        .rev()
        .filter(|b| b.revision == request.revision && !b.origin.is_progress())
        .find_map(|b| b.delivery_issue.as_ref())
    {
        use crate::model::social::DeliveryIssueReason;
        let reason = match issue.reason {
            DeliveryIssueReason::PassiveWindowPushUnavailable => {
                "原消息的回复窗口已过期，当前会话未开启主动推送（或任务推送功能关闭）"
            }
            DeliveryIssueReason::PassiveQuotaPushUnavailable => {
                "原消息的可用回复额度不足，当前会话未开启主动推送（或任务推送功能关闭）"
            }
            DeliveryIssueReason::MediaNeedsFreshMessage => {
                "当前附件没有可用的主动投递通道，请用新消息取回结果"
            }
            DeliveryIssueReason::MediaPreparationFailed => {
                "附件读取或上传未成功，尚未向 QQ 发出附件消息"
            }
            DeliveryIssueReason::AuthenticationUnavailable => "QQ 发送认证暂不可用，本次未发出",
            DeliveryIssueReason::InvalidRequest => "消息发送参数无效，本次未发出",
            DeliveryIssueReason::QqRejected => "QQ 明确拒绝了本次发送，结果已保留",
            DeliveryIssueReason::Unknown => "QQ 发送结果未知，须核对后才能再次发送",
        };
        text.push_str(&format!("\n投递说明：{reason}"));
        if !brief && let Some(code) = issue.qq_code {
            text.push_str(&format!("（QQ 错误码 {code}）"));
        }
    } else if request.status == RequestStatus::AwaitingDelivery {
        text.push_str("\n投递说明：旧任务未记录具体原因，不能据此断言是平台拒绝或生成错误。");
    }
    if request.status == RequestStatus::AwaitingDelivery && request.failure_code.is_none() {
        text.push_str(&format!(
            "\n/continue {} 可取回结果，不会重新执行。",
            super::task_identity::label(request)
        ));
    } else if request.status == RequestStatus::UnknownDelivery {
        text.push_str("\nQQ 是否已收到尚未确认，需要先核对发送记录。");
    } else if request.status == RequestStatus::WaitingForUser && !brief {
        text.push_str(&format!(
            "\n请回复先前的问题，或使用 /continue {} 补充内容。",
            super::task_identity::label(request)
        ));
    }
    text
}

fn task_buttons(menu: &mut MenuReply, request: &RequestRecord, position: Option<usize>) {
    menu.button(
        &position.map_or_else(|| "查看任务进度".into(), |i| format!("任务{i}进度")),
        &format!("/status {}", super::task_identity::label(request)),
    );
    if request.status == RequestStatus::AwaitingDelivery && request.failure_code.is_none() {
        menu.button(
            &position.map_or_else(|| "取回任务结果".into(), |i| format!("取回任务{i}")),
            &format!("/continue {}", super::task_identity::label(request)),
        );
    }
}

pub(super) fn append_overview(
    menu: &mut MenuReply,
    state: &GroupState,
    author: &str,
    markdown: bool,
    now: i64,
) {
    let mut requests = state
        .requests
        .values()
        .filter(|request| !request.control)
        .collect::<Vec<_>>();
    requests.sort_by_key(|request| {
        (
            std::cmp::Reverse(active(request)),
            std::cmp::Reverse(owns(request, author)),
            std::cmp::Reverse(request.id),
        )
    });
    if requests.is_empty() {
        menu.text.push_str("\n\n暂时没有可查询的任务。");
        return;
    }
    menu.text.push_str("\n\n最近任务");
    for (index, request) in requests.iter().take(OVERVIEW_LIMIT).enumerate() {
        let text = format!("任务 {}\n{}", index + 1, details(state, request, now, true));
        menu.text.push_str("\n\n");
        menu.text.push_str(&if markdown {
            command_menu::escaped(&text)
        } else {
            text
        });
        task_buttons(menu, request, Some(index + 1));
    }
    if requests.len() > OVERVIEW_LIMIT {
        menu.text
            .push_str("\n\n仅列出部分任务；/status 任务编号 可查询指定任务。");
    }
}

pub(super) fn detail_menu(
    state: &GroupState,
    id: u64,
    config: &SocialConfig,
    now: i64,
) -> MenuReply {
    let Some(request) = state.requests.get(&id).filter(|request| !request.control) else {
        return command_menu::result(
            "/status",
            "当前对话没有这个任务，或它已移至聊天存档。".into(),
            config.reply_format == "markdown",
        );
    };
    let mut menu = command_menu::result(
        "/status",
        details(state, request, now, false),
        config.reply_format == "markdown",
    );
    task_buttons(&mut menu, request, None);
    menu
}

fn progress_only(text: &str) -> bool {
    let text = text
        .trim()
        .trim_end_matches(['?', '？', '!', '！', '.', '。']);
    matches!(
        text,
        "弄好了没"
            | "弄好了吗"
            | "做好了吗"
            | "做完了吗"
            | "好了没"
            | "好了吗"
            | "完成了吗"
            | "查完了吗"
            | "还在查吗"
            | "你还在查吗"
            | "进度怎么样"
            | "现在进度怎么样"
            | "任务进度怎么样"
            | "处理到哪了"
            | "有结果了吗"
            | "结果出来了吗"
            | "还在处理吗"
            | "你还在处理吗"
    )
}

fn quoted_request(state: &GroupState, request: &RequestRecord, message: &ChatMessage) -> bool {
    message
        .quotes
        .iter()
        .filter_map(|quote| quote.reference_idx.as_deref())
        .filter(|reference| !reference.is_empty())
        .any(|reference| {
            request
                .source_seqs
                .iter()
                .filter_map(|seq| state.record(*seq).ok())
                .any(|record| {
                    record.message.reference_idx.as_deref() == Some(reference)
                        || record.message.id == reference
                })
                || request.bubbles.iter().any(|bubble| {
                    bubble.status == DeliveryStatus::Sent
                        && (bubble.reference_idx.as_deref() == Some(reference)
                            || bubble.platform_message_id.as_deref() == Some(reference))
                })
        })
}

/// Call before participation judgment, using only original typed message text.
pub(super) fn handle_natural(
    state: &mut GroupState,
    seq: u64,
    text: &str,
    config: &SocialConfig,
    private: bool,
    now: i64,
) -> Result<bool> {
    let record = state.record(seq)?;
    let message = &record.message;
    let mut original = message.text.trim();
    if message.at_bot
        && original.starts_with("<@")
        && let Some(end) = original.find('>')
    {
        original = original[end + 1..].trim_start();
    }
    if text != original
        || !progress_only(text)
        || !message.images.is_empty()
        || !message.voices.is_empty()
        || !message.attachment_kinds.is_empty()
    {
        return Ok(false);
    }
    if !private
        && control::current_mode(state, config, false) == ParticipationMode::Mentions
        && !message.at_bot
    {
        return Ok(false);
    }
    let author = message.author_id.clone();
    let candidates = state
        .requests
        .values()
        .filter(|request| !request.control)
        .collect::<Vec<_>>();
    let quoted = candidates
        .iter()
        .filter(|request| quoted_request(state, request, message))
        .collect::<Vec<_>>();
    let owned_active = candidates
        .iter()
        .filter(|request| active(request) && owns(request, &author))
        .collect::<Vec<_>>();
    if !private
        && !message.at_bot
        && (!message.mentioned_user_ids.is_empty()
            || (!message.quotes.is_empty() && quoted.is_empty()))
    {
        return Ok(false);
    }
    if !private && !message.at_bot && quoted.is_empty() && owned_active.len() != 1 {
        return Ok(false);
    }
    let selected = if quoted.len() == 1 {
        Some(quoted[0].id)
    } else if !quoted.is_empty() {
        None
    } else if owned_active.len() == 1 {
        Some(owned_active[0].id)
    } else if owned_active.is_empty() {
        candidates
            .iter()
            .rev()
            .find(|request| owns(request, &author))
            .map(|request| request.id)
    } else {
        None
    };
    let menu = if let Some(id) = selected {
        detail_menu(state, id, config, now)
    } else {
        let mut menu = command_menu::result(
            "/status",
            "可以直接查询任务状态；有多个任务时，请选择对应编号。".into(),
            config.reply_format == "markdown",
        );
        append_overview(
            &mut menu,
            state,
            &author,
            config.reply_format == "markdown",
            now,
        );
        menu
    };
    match state.record(seq)?.status {
        DecisionStatus::Decided | DecisionStatus::Bypassed => return Ok(true),
        DecisionStatus::Pending => state.begin_decision(seq)?,
        DecisionStatus::Running => {}
        DecisionStatus::Failed => return Ok(false),
    }
    control::control_menu_reply(state, seq, menu, config, now)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_excerpts_hide_opaque_mentions_but_keep_plain_names() {
        assert_eq!(
            excerpt("<@opaque_bot>查一下 @Codex", 80),
            "@成员查一下 @Codex"
        );
    }

    use crate::model::social::{Bubble, Quote, TaskFeedback, VoiceInput};

    fn fixture(status: RequestStatus) -> (GroupState, SocialConfig) {
        let mut state = GroupState::default();
        let seq = state
            .ingest(
                ChatMessage {
                    id: "source-message".into(),
                    author_id: "owner".into(),
                    reference_idx: Some("source-ref".into()),
                    text: "帮我查清几个方案的区别".into(),
                    timestamp_ms: 1000,
                    ..Default::default()
                },
                100,
            )
            .unwrap()
            .unwrap();
        state.next_request = 11;
        state.requests.insert(
            10,
            RequestRecord {
                id: 10,
                owner_id: "owner".into(),
                contributor_ids: vec!["helper".into()],
                source_seqs: vec![seq],
                revision: 2,
                updated_at_ms: 61_000,
                status,
                feedback: Some(TaskFeedback {
                    revision: 2,
                    accepted_at_ms: 1000,
                    started_at_ms: Some(2000),
                    last_activity_at_ms: Some(60_000),
                    last_progress_text: "已核对两份官方说明".into(),
                    last_progress_at_ms: Some(50_000),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        (
            state,
            SocialConfig {
                mode: ParticipationMode::Helpful,
                reply_format: "markdown".into(),
                ..Default::default()
            },
        )
    }

    fn query(state: &mut GroupState, author: &str, text: &str, at_bot: bool) -> u64 {
        state
            .ingest(
                ChatMessage {
                    id: format!("query-{}", state.next_seq),
                    author_id: author.into(),
                    text: text.into(),
                    at_bot,
                    timestamp_ms: 90_000,
                    ..Default::default()
                },
                100,
            )
            .unwrap()
            .unwrap()
    }

    #[test]
    fn progress_only_never_matches_compound_requests_or_quoted_or_asr_text() {
        for text in ["弄好了没", "你还在查吗？", "进度怎么样！"] {
            assert!(progress_only(text));
        }
        for text in [
            "弄好了没，顺便查一下价格",
            "进度怎么样\n再加一个方案",
            "他说：弄好了没",
            "请解释‘进度怎么样’",
            "再帮我查一下",
        ] {
            assert!(!progress_only(text));
        }
        let (mut state, config) = fixture(RequestStatus::Generating);
        let seq = query(&mut state, "owner", "", false);
        state
            .messages
            .last_mut()
            .unwrap()
            .message
            .voices
            .push(VoiceInput::default());
        assert!(!handle_natural(&mut state, seq, "弄好了没", &config, true, 90_000).unwrap());
        assert_eq!(state.record(seq).unwrap().status, DecisionStatus::Pending);
    }

    #[test]
    fn natural_query_creates_only_control_reply_and_keeps_current_task_untouched() {
        let (mut state, config) = fixture(RequestStatus::Generating);
        let seq = query(&mut state, "owner", "你还在查吗？", false);
        assert!(handle_natural(&mut state, seq, "你还在查吗？", &config, false, 90_000).unwrap());
        assert_eq!(state.requests[&10].revision, 2);
        assert_eq!(state.requests[&10].status, RequestStatus::Generating);
        let reply = &state.requests[&state.record(seq).unwrap().applied_request.unwrap()];
        assert!(reply.control);
        assert!(reply.bubbles[0].text.contains("R10"));
        assert!(reply.bubbles[0].text.contains("已核对两份官方说明"));
        assert_eq!(state.record(seq).unwrap().status, DecisionStatus::Bypassed);
    }

    #[test]
    fn overview_buttons_distinguish_tasks_and_carry_full_time_codes() {
        let (mut state, _) = fixture(RequestStatus::Generating);
        state.requests.get_mut(&10).unwrap().display_id = Some("20261025-143205".into());
        let mut second = state.requests[&10].clone();
        second.id = 11;
        second.display_id = Some("20261025-143205-11".into());
        state.requests.insert(11, second);
        let mut menu = MenuReply::new(String::new());
        append_overview(&mut menu, &state, "owner", true, 90_000);
        assert!(menu.text.contains("任务 1") && menu.text.contains("任务 2"));
        assert_eq!(menu.buttons[0].label, "任务1进度");
        assert_eq!(menu.buttons[1].label, "任务2进度");
        assert_eq!(menu.buttons[0].command, "/status 20261025-143205-11");
        assert_eq!(menu.buttons[1].command, "/status 20261025-143205");
        assert!(crate::qq::message_content::command_keyboard(&menu.buttons).is_ok());
    }

    #[test]
    fn conservative_mode_and_explicit_human_addressing_are_preserved() {
        let (mut state, mut config) = fixture(RequestStatus::Generating);
        config.mode = ParticipationMode::Mentions;
        let seq = query(&mut state, "owner", "弄好了没", false);
        assert!(!handle_natural(&mut state, seq, "弄好了没", &config, false, 90_000).unwrap());
        state.messages.last_mut().unwrap().message.at_bot = true;
        assert!(handle_natural(&mut state, seq, "弄好了没", &config, false, 90_000).unwrap());
        config.mode = ParticipationMode::Helpful;
        let seq = query(&mut state, "owner", "弄好了没", false);
        state
            .messages
            .last_mut()
            .unwrap()
            .message
            .mentioned_user_ids
            .push("another-human".into());
        assert!(!handle_natural(&mut state, seq, "弄好了没", &config, false, 90_000).unwrap());
    }

    #[test]
    fn exact_quote_can_query_other_request_but_unmatched_quote_is_not_rerouted() {
        let (mut state, config) = fixture(RequestStatus::Completed);
        let seq = query(&mut state, "another-member", "有结果了吗", false);
        state
            .messages
            .last_mut()
            .unwrap()
            .message
            .quotes
            .push(Quote {
                reference_idx: Some("source-ref".into()),
                ..Default::default()
            });
        assert!(handle_natural(&mut state, seq, "有结果了吗", &config, false, 90_000).unwrap());
        state.requests.get_mut(&10).unwrap().status = RequestStatus::Generating;
        let seq = query(&mut state, "owner", "有结果了吗", false);
        state
            .messages
            .last_mut()
            .unwrap()
            .message
            .quotes
            .push(Quote {
                reference_idx: Some("unrelated-human-message".into()),
                ..Default::default()
            });
        assert!(!handle_natural(&mut state, seq, "有结果了吗", &config, false, 90_000).unwrap());
    }

    #[test]
    fn group_query_requires_unambiguous_owned_or_contributor_task_without_explicit_address() {
        let (mut state, config) = fixture(RequestStatus::Generating);
        let seq = query(&mut state, "outsider", "弄好了没", false);
        assert!(!handle_natural(&mut state, seq, "弄好了没", &config, false, 90_000).unwrap());
        let seq = query(&mut state, "helper", "弄好了没", false);
        assert!(handle_natural(&mut state, seq, "弄好了没", &config, false, 90_000).unwrap());
        let mut other = state.requests[&10].clone();
        other.id = 20;
        state.requests.insert(20, other);
        let seq = query(&mut state, "owner", "弄好了没", false);
        assert!(!handle_natural(&mut state, seq, "弄好了没", &config, false, 90_000).unwrap());
        state.messages.last_mut().unwrap().message.at_bot = true;
        assert!(handle_natural(&mut state, seq, "弄好了没", &config, false, 90_000).unwrap());
        let reply =
            &state.requests[&state.record(seq).unwrap().applied_request.unwrap()].bubbles[0];
        assert!(reply.text.contains("多个任务"));
        assert!(
            reply
                .command_buttons
                .iter()
                .any(|button| button.command == "/status R10")
        );
        assert!(
            reply
                .command_buttons
                .iter()
                .any(|button| button.command == "/status R20")
        );
    }

    #[test]
    fn stale_feedback_is_not_presented_as_current_progress_or_backend_activity() {
        let (mut state, config) = fixture(RequestStatus::Generating);
        state
            .requests
            .get_mut(&10)
            .unwrap()
            .feedback
            .as_mut()
            .unwrap()
            .revision = 1;
        let text = detail_menu(&state, 10, &config, 90_000).text;
        assert!(!text.contains("已核对两份官方说明"));
        assert!(text.contains("尚未记录新的实质进展"));
        assert!(text.contains("尚无记录"));
        assert!(text.contains("1 分 29 秒"));
    }

    #[test]
    fn generation_failure_delivery_failure_and_unknown_delivery_are_distinct() {
        let (mut state, config) = fixture(RequestStatus::Failed);
        let request = state.requests.get_mut(&10).unwrap();
        request.failure_code = Some("private_internal_exception_do_not_display".into());
        let text = detail_menu(&state, 10, &config, 90_000).text;
        assert!(text.contains("处理失败"));
        assert!(!text.contains("private_internal_exception"));
        let request = state.requests.get_mut(&10).unwrap();
        request.failure_code = None;
        request.generated_revision = 2;
        request.bubbles.push(Bubble {
            revision: 2,
            origin: BubbleOrigin::ModelAnswer,
            status: DeliveryStatus::Failed,
            ..Default::default()
        });
        assert!(
            detail_menu(&state, 10, &config, 90_000)
                .text
                .contains("投递失败")
        );
        state.requests.get_mut(&10).unwrap().status = RequestStatus::UnknownDelivery;
        let menu = detail_menu(&state, 10, &config, 90_000);
        assert!(menu.text.contains("发送结果待核对"));
        assert!(
            menu.buttons
                .iter()
                .all(|button| !button.command.starts_with("/continue"))
        );
    }

    #[test]
    fn retrieval_button_exists_only_for_retained_results_and_overview_is_bounded() {
        let (mut state, config) = fixture(RequestStatus::AwaitingDelivery);
        assert!(
            detail_menu(&state, 10, &config, 90_000)
                .buttons
                .iter()
                .any(|button| button.command == "/continue R10")
        );
        for status in [
            RequestStatus::Queued,
            RequestStatus::Generating,
            RequestStatus::WaitingForUser,
            RequestStatus::Completed,
            RequestStatus::Failed,
            RequestStatus::UnknownDelivery,
        ] {
            state.requests.get_mut(&10).unwrap().status = status;
            assert!(
                detail_menu(&state, 10, &config, 90_000)
                    .buttons
                    .iter()
                    .all(|button| !button.command.starts_with("/continue"))
            );
        }
        state.requests.get_mut(&10).unwrap().status = RequestStatus::AwaitingDelivery;
        for id in 20..30 {
            let mut request = state.requests[&10].clone();
            request.id = id;
            state.requests.insert(id, request);
        }
        let menu = command_menu::status(&state, &config, false, "owner", 11, 90_000);
        assert!(menu.text.contains("默认模型"));
        assert!(menu.text.contains("仅列出部分任务"));
        assert_eq!(
            menu.buttons
                .iter()
                .filter(|button| button.command.starts_with("/status R"))
                .count(),
            3
        );
        assert!(menu.buttons.len() <= 10);
        assert!(menu.text.chars().count() < 4000);
    }

    #[test]
    fn sensitive_excerpts_and_links_do_not_leak_into_status_menus() {
        assert!(!excerpt("api_key=private-value", 80).contains("private-value"));
        assert!(
            !excerpt("查看 https://example.com/?token=private-value", 80).contains("private-value")
        );
        assert_eq!(
            excerpt("看看 https://example.com/file", 80),
            "看看 [内容已省略]"
        );
    }
}
