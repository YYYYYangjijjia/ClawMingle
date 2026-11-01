//! Apply a generated reply to its current request before the worker persists it.
use super::*;

pub(super) struct ReplyContext<'a> {
    pub config: &'a SocialConfig,
    pub target: &'a ConversationTarget,
    pub media: &'a crate::social::media::MediaStore,
    pub stickers: &'a crate::social::stickers::StickerStore,
    pub generation_mentions: &'a std::collections::BTreeSet<String>,
}

impl ReplyContext<'_> {
    pub fn apply(
        self,
        state: &mut GroupState,
        id: u64,
        revision: u64,
        generated: GeneratedReply,
    ) -> Result<()> {
        let Self {
            config,
            target,
            media,
            stickers,
            generation_mentions,
        } = self;
        let sticker_draw = generated.sticker_draw;
        let sticker_sends = generated
            .sticker_sends
            .into_iter()
            .filter(|s| s.request_revision == revision)
            .collect::<Vec<_>>();
        let result = generated.result;
        if let Some(thread) = result.session_id {
            bind_thread(state, id, thread);
        }
        let reply_config = crate::social::interaction::reply_config(
            config,
            state.requests.get(&id).is_some_and(|r| r.long_task),
        );
        let plan = crate::social::reply::parse_reply_with_queued(
            &result.text,
            &reply_config,
            generated.images.len(),
            !sticker_sends.is_empty(),
        );
        let reply = plan.and_then(|mut reply| {
            // An obsolete or cancelled turn must not publish images.
            if !state
                .requests
                .get(&id)
                .is_some_and(|r| r.revision == revision && r.status == RequestStatus::Generating)
            {
                return Ok(false);
            }
            media.cleanup(state)?;
            quote_selection(state, id, &reply, &reply_config)?;
            let mentions = crate::social::mentions::resolve(
                state,
                id,
                target.private(),
                &reply,
                generation_mentions,
            )?;
            let (quote_source, suppress) = crate::social::reply::quote_intent(&reply);
            let mut images = reply_images(state, media, &target.workspace(config)?, &reply.images)?;
            images.extend(generated.images);
            let files = media.snapshot_files(&target.workspace(config)?, &reply.files)?;
            if reply.sticker.as_ref().is_some_and(|selection| {
                !selection.requested
                    && !sticker_draw.as_ref().is_some_and(|draw| {
                        draw.matches(revision, &selection.id, selection.revision)
                    })
            }) {
                let selection = reply.sticker.take().expect("selection checked");
                if reply.bubbles.is_empty() && images.is_empty() && files.is_empty() {
                    reply.bubbles.push(
                        selection
                            .closing_text
                            .unwrap_or_else(|| "暂时没有选到适合的表情包。".into()),
                    );
                }
            }
            let selections = reply.sticker.iter().map(|s| (None, s)).chain(
                sticker_sends
                    .iter()
                    .filter(|s| s.existing_status.is_none())
                    .map(|s| (Some(s.operation_id.as_str()), &s.selection)),
            );
            for (operation, selection) in selections {
                // Each deliberate tool send has its own durable outbox operation.
                match prepare_sticker(
                    state,
                    stickers,
                    media,
                    &target.scope_tag(),
                    id,
                    operation,
                    selection,
                    &config.stickers,
                    chrono::Utc::now().timestamp(),
                ) {
                    Ok(Some(image)) => images.push(image),
                    Ok(None) | Err(_) => {
                        if reply.bubbles.is_empty() && images.is_empty() && files.is_empty() {
                            reply.bubbles.push(
                                selection
                                    .closing_text
                                    .clone()
                                    .unwrap_or_else(|| "这张贴图暂时无法发送。".into()),
                            );
                        }
                    }
                }
            }
            let record = state.requests.get_mut(&id).expect("request retained");
            record.quote_source_seq = quote_source;
            record.suppress_auto_quote = suppress;
            let first_bubble = record.bubbles.len();
            let waiting = reply.outcome == crate::social::reply::Outcome::NeedsInput;
            let done = if reply.bubbles.is_empty()
                && images.is_empty()
                && files.is_empty()
                && !sticker_sends.is_empty()
                && sticker_sends.iter().all(|s| s.existing_status.is_some())
            {
                finish_existing_sticker_reply(state, id, revision)?
            } else if files.is_empty() {
                if images.is_empty() {
                    state.finish_generation(id, revision, reply.bubbles)?
                } else {
                    state.finish_generation_with_images(id, revision, reply.bubbles, images)?
                }
            } else {
                state.finish_generation_with_media(id, revision, reply.bubbles, images, files)?
            };
            if done {
                crate::social::mentions::attach(
                    &mut state
                        .requests
                        .get_mut(&id)
                        .expect("request retained")
                        .bubbles[first_bubble..],
                    mentions,
                );
            }
            if done && waiting {
                let request = state.requests.get_mut(&id).expect("request retained");
                request.awaiting_input = true;
                for bubble in &mut request.bubbles {
                    if bubble.revision == revision
                        && bubble.origin == crate::model::social::BubbleOrigin::ModelAnswer
                    {
                        bubble.origin = crate::model::social::BubbleOrigin::Clarification;
                    }
                }
            }
            Ok(done)
        });
        match reply {
            Ok(_) => {
                if let Some(request) = state.requests.get_mut(&id) {
                    for bubble in &mut request.bubbles {
                        if bubble.revision == revision {
                            bubble.markdown = config.reply_format == "markdown";
                        }
                    }
                }
            }
            Err(error) => {
                let (code, notice) = if error.to_string().starts_with("mention_") {
                    (
                        "mention_preparation_failed",
                        "这次没能确认要 @ 的群成员，尚未发送提醒。请在群里直接 @ 一下那位成员，再告诉我要转达什么。",
                    )
                } else {
                    (
                        "reply_preparation_failed",
                        "这次回复准备失败，未能完成发送。请重试；若要处理的原图已过期，请重新发送图片。",
                    )
                };
                state.fail_generation_with_notice(id, revision, code, notice)?;
                println!("回复准备失败，已记录并排队发送失败提示。");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::social::{BubbleOrigin, RequestRecord};

    fn generating() -> GroupState {
        let mut state = GroupState::default();
        let seq = state
            .ingest(
                ChatMessage {
                    id: "source".into(),
                    author_id: "member".into(),
                    text: "help".into(),
                    ..Default::default()
                },
                50,
            )
            .unwrap()
            .unwrap();
        state.requests.insert(
            1,
            RequestRecord {
                id: 1,
                owner_id: "member".into(),
                source_seqs: vec![seq],
                revision: 1,
                status: RequestStatus::Generating,
                ..Default::default()
            },
        );
        state
    }

    fn complete(state: &mut GroupState, root: &std::path::Path, text: &str) {
        let config = SocialConfig {
            data_dir: root.join("data"),
            reply_format: "markdown".into(),
            ..Default::default()
        };
        private_directory(&config.data_dir).unwrap();
        let target = ConversationTarget::Group("fixture-group".into());
        let media =
            crate::social::media::MediaStore::new(&config.data_dir, &target.scope_tag()).unwrap();
        let stickers = crate::social::stickers::StickerStore::new(&config.data_dir).unwrap();
        ReplyContext {
            config: &config,
            target: &target,
            media: &media,
            stickers: &stickers,
            generation_mentions: &Default::default(),
        }
        .apply(
            state,
            1,
            1,
            GeneratedReply {
                sticker_draw: None,
                sticker_sends: Vec::new(),
                result: ExecutionResult {
                    session_id: Some("fixture-thread".into()),
                    text: text.into(),
                    changed_files: Vec::new(),
                    token_usage_info: None,
                    context_window: None,
                    generated_images: Vec::new(),
                },
                images: Vec::new(),
            },
        )
        .unwrap();
    }

    #[test]
    fn obsolete_or_cancelled_completion_never_prepares_or_enqueues_media() {
        for (revision, status) in [
            (2, RequestStatus::Generating),
            (1, RequestStatus::Cancelled),
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut state = generating();
            let request = state.requests.get_mut(&1).unwrap();
            request.revision = revision;
            request.status = status;
            // Resolving this unavailable image would create a workspace and fail.
            complete(
                &mut state,
                root.path(),
                r#"{"bubbles":[],"images":["sent:1"]}"#,
            );
            let request = &state.requests[&1];
            assert_eq!(request.status, status);
            assert!(request.bubbles.is_empty());
            assert!(request.failure_code.is_none());
            assert_eq!(request.accepted_revision, 0);
            assert!(!root.path().join("data/workspaces").exists());
        }
    }

    #[test]
    fn clarification_preserves_quote_format_and_pending_delivery() {
        let root = tempfile::tempdir().unwrap();
        let mut state = generating();
        complete(
            &mut state,
            root.path(),
            r#"{"bubbles":["Which date?"],"outcome":"needs_input","quote":{"mode":"source","seq":1}}"#,
        );
        let request = &state.requests[&1];
        assert_eq!(request.status, RequestStatus::Ready);
        assert!(request.awaiting_input);
        assert_eq!(request.accepted_revision, 1);
        assert_eq!(request.quote_source_seq, Some(1));
        assert_eq!(request.codex_thread_id.as_deref(), Some("fixture-thread"));
        assert_eq!(request.bubbles.len(), 1);
        let bubble = &request.bubbles[0];
        assert_eq!(bubble.origin, BubbleOrigin::Clarification);
        assert_eq!(bubble.status, DeliveryStatus::Pending);
        assert!(bubble.markdown);
    }

    #[test]
    fn preparation_errors_queue_failure_without_escaping_the_worker() {
        for (text, code) in [
            ("invalid JSON", "reply_preparation_failed"),
            (
                r#"{"bubbles":[],"images":["sent:1"]}"#,
                "reply_preparation_failed",
            ),
            (
                r#"{"bubbles":["hello"],"mentions":[{"participant":"unknown","bubble_index":0,"source_seq":1}]}"#,
                "mention_preparation_failed",
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut state = generating();
            complete(&mut state, root.path(), text);
            let request = &state.requests[&1];
            assert_eq!(request.status, RequestStatus::Ready);
            assert_eq!(request.failure_code.as_deref(), Some(code));
            assert_eq!(request.bubbles.len(), 1);
            assert_eq!(request.bubbles[0].origin, BubbleOrigin::FailureNotice);
            assert_eq!(request.bubbles[0].status, DeliveryStatus::Pending);
            assert_eq!(request.accepted_revision, 0);
        }
    }
}
