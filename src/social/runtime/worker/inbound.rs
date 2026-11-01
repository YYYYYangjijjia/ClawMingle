//! Original-message command dispatch, awaited inline by the receive branch.
use super::*;

pub(super) async fn route_received_command(
    state: &mut GroupState,
    seq: u64,
    config: &SocialConfig,
    target: &ConversationTarget,
    tasks: &crate::social::tasks::TaskStore,
) -> Result<bool> {
    let record = state.record(seq)?;
    // Commands, skill installation and memory confirmation use original text only.
    let mut command_text = record.message.text.trim();
    if record.message.at_bot
        && command_text.starts_with("<@")
        && let Some(end) = command_text.find('>')
    {
        command_text = command_text[end + 1..].trim_start();
    }
    let command_text = command_text.to_owned();
    let author = state.record(seq)?.message.author_id.clone();
    let source = state.record(seq)?.message.id.clone();
    let grants_before = crate::social::integration_control::grant(state);
    if crate::social::interaction::command(
        state,
        seq,
        &command_text,
        config,
        target.private(),
        target.id(),
        chrono::Utc::now().timestamp_millis(),
    )? || crate::social::task_status::handle_natural(
        state,
        seq,
        &command_text,
        config,
        target.private(),
        chrono::Utc::now().timestamp_millis(),
    )? {
    } else if let Some(reply) = tasks.command(
        &command_text,
        &author,
        config.is_scope_admin(&author, target.private(), target.id()),
        &source,
        config.tasks_enabled,
        chrono::Utc::now().timestamp_millis(),
    ) {
        state.begin_decision(seq)?;
        crate::social::control::control_reply(
            state,
            seq,
            reply,
            config,
            chrono::Utc::now().timestamp_millis(),
        )?;
    } else if !crate::social::integration_control::handle(
        state,
        seq,
        &command_text,
        config,
        target.private(),
        target.id(),
        chrono::Utc::now().timestamp_millis(),
    )? && !crate::social::tool_control::handle(
        state,
        seq,
        &command_text,
        config,
        target.private(),
        target.id(),
        chrono::Utc::now().timestamp_millis(),
    )? && !crate::social::memory_control::handle(
        state,
        seq,
        &command_text,
        config,
        target.id(),
        target.private(),
        chrono::Utc::now().timestamp_millis(),
    )
    .await?
        && !crate::social::skill_control::handle(
            state,
            seq,
            &command_text,
            config,
            target.private(),
            target.id(),
            chrono::Utc::now().timestamp_millis(),
        )
        .await?
        && let Some(command) = parse_command(&command_text)
    {
        handle_command(
            state,
            seq,
            command,
            config,
            target.private(),
            target.id(),
            chrono::Utc::now().timestamp_millis(),
        )?;
    }
    Ok(crate::social::integration_control::revoked(
        &grants_before,
        &crate::social::integration_control::grant(state),
    ))
}
