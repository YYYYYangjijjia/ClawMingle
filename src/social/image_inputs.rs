//! Bind visual inputs to request messages, quotes, or the shared context window.
//! Failed quoted media leaves explicit context instead of replacing it with the
//! latest image or discarding an otherwise answerable text request.

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

use super::{
    images::{self, RemoteImage},
    media::MediaStore,
};
use crate::model::social::{GroupState, OutputImage, Quote};

enum Source {
    Remote(RemoteImage),
    Stored(OutputImage),
}

impl Source {
    fn key(&self) -> String {
        match self {
            Self::Remote(image) => format!("remote:{}", image.url),
            Self::Stored(image) => format!("stored:{}", image.file),
        }
    }
}

pub struct Input {
    source: Source,
    seq: u64,
    quote_path: Option<Vec<usize>>,
    context: bool,
}

impl Input {
    fn origin(&self) -> &'static str {
        if self.context {
            "context"
        } else if self.quote_path.is_some() {
            "quote"
        } else {
            "request"
        }
    }
}

/// Include bindings as well as source identity: an equal candidate count does
/// not mean that the same pixels belong to the same request messages.
pub fn identity(inputs: &[Input]) -> Value {
    json!(
        inputs
            .iter()
            .map(|input| {
                let source = match &input.source {
                    Source::Remote(image) => {
                        json!({"url":image.url,"content_type":image.content_type})
                    }
                    Source::Stored(image) => json!({"stored":image}),
                };
                json!({"seq":input.seq,"quote_path":input.quote_path,"origin":input.origin(),"source":source})
            })
            .collect::<Vec<_>>()
    )
}

pub fn current_message(state: &GroupState, seq: u64) -> Result<Vec<Input>> {
    Ok(state
        .record(seq)?
        .message
        .images
        .iter()
        .take(images::MAX_IMAGES)
        .map(|image| Input {
            source: Source::Remote(RemoteImage {
                url: image.url.clone(),
                content_type: image.content_type.clone(),
            }),
            seq,
            quote_path: None,
            context: false,
        })
        .collect())
}

/// Keep attribution even if the textual prompt later drops old records to fit
/// its byte budget. This is metadata, not an additional task source.
pub fn add_provenance(bindings: &mut [Value], state: &GroupState) -> Result<()> {
    for binding in bindings {
        let record = state.record(
            binding["source_seq"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("image_binding_source_missing"))?,
        )?;
        binding["source"] = json!({"speaker":super::state::participant_alias(&record.message.author_id),
            "timestamp_ms":record.message.timestamp_ms,"text":record.message.text.chars().take(500).collect::<String>()});
    }
    Ok(())
}

pub fn collect(
    state: &GroupState,
    request: u64,
    config: &super::config::SocialConfig,
) -> Result<Vec<Input>> {
    let request = state
        .requests
        .get(&request)
        .ok_or_else(|| anyhow::anyhow!("request_missing"))?;
    let mut inputs = Vec::new();
    for seq in &request.source_seqs {
        let message = &state.record(*seq)?.message;
        inputs.extend(message.images.iter().map(|image| Input {
            source: Source::Remote(RemoteImage {
                url: image.url.clone(),
                content_type: image.content_type.clone(),
            }),
            seq: *seq,
            quote_path: None,
            context: false,
        }));
    }
    fn quoted(
        inputs: &mut Vec<Input>,
        seq: u64,
        quotes: &[Quote],
        prefix: &[usize],
        context: bool,
    ) {
        if prefix.len() >= 4 || inputs.len() >= 24 {
            return;
        }
        for (index, quote) in quotes.iter().take(24).enumerate() {
            if inputs.len() >= 24 {
                break;
            }
            let mut path = prefix.to_vec();
            path.push(index);
            let room = (24 - inputs.len()).min(3);
            inputs.extend(quote.local_images.iter().take(room).map(|image| Input {
                source: Source::Stored(image.clone()),
                seq,
                quote_path: Some(path.clone()),
                context,
            }));
            // Prefer the exact acknowledged snapshot when both it and platform
            // image metadata describe the same bot message.
            if quote.local_images.is_empty() {
                let room = (24 - inputs.len()).min(3);
                inputs.extend(quote.images.iter().take(room).map(|image| Input {
                    source: Source::Remote(RemoteImage {
                        url: image.url.clone(),
                        content_type: image.content_type.clone(),
                    }),
                    seq,
                    quote_path: Some(path.clone()),
                    context,
                }));
            }
            quoted(inputs, seq, &quote.parts, &path, context);
        }
    }
    for seq in &request.source_seqs {
        quoted(
            &mut inputs,
            *seq,
            &state.record(*seq)?.message.quotes,
            &[],
            false,
        );
    }
    // Match the text window, including silent/bypassed messages and other human
    // participants. These pixels are context, never new task instructions.
    for record in super::message_context::request_window(state, request.id, config)
        .into_iter()
        .rev()
    {
        if request.source_seqs.contains(&record.seq) {
            continue;
        }
        inputs.extend(record.message.images.iter().map(|image| Input {
            source: Source::Remote(RemoteImage {
                url: image.url.clone(),
                content_type: image.content_type.clone(),
            }),
            seq: record.seq,
            quote_path: None,
            context: true,
        }));
        quoted(&mut inputs, record.seq, &record.message.quotes, &[], true);
    }
    Ok(inputs)
}

pub async fn prepare(
    inputs: &[Input],
    media: &MediaStore,
    workspace: &std::path::Path,
) -> Result<(Vec<String>, Vec<Value>, Vec<String>)> {
    // A semantically appended burst may contain more than three attachments.
    // Retain the newest distinct direct inputs, with explicit omitted bindings
    // for older pixels; duplicate sources still share a single visual input.
    let mut preferred = Vec::new();
    for input in inputs
        .iter()
        .rev()
        .filter(|input| input.origin() == "request")
        .chain(inputs.iter().filter(|input| input.origin() == "quote"))
        .chain(inputs.iter().filter(|input| input.context))
    {
        let key = input.source.key();
        if !preferred.contains(&key) {
            preferred.push(key);
        }
        if preferred.len() == images::MAX_IMAGES {
            break;
        }
    }
    let mut data = Vec::new();
    let mut bindings = Vec::new();
    let mut keys = Vec::<String>::new();
    let mut failed = Vec::<String>::new();
    let mut attempts = 0;
    for input in inputs {
        let key = input.source.key();
        let mut binding = json!({"source_seq":input.seq,"quote_path":input.quote_path,"origin":input.origin(),"status":"unavailable","image_ref":null});
        if let Some(index) = keys.iter().position(|existing| existing == &key) {
            binding["status"] = json!("available");
            binding["image_ref"] = json!(format!("input:{}", index + 1));
        } else if failed.contains(&key) {
            // The same quote may occur in several request fragments. A failed
            // remote request is not retried implicitly through another fragment.
        } else if data.len() >= images::MAX_IMAGES
            || attempts >= images::MAX_IMAGES
            || !preferred.contains(&key)
        {
            binding["status"] = json!("omitted_limit");
        } else {
            attempts += 1;
            let result = match &input.source {
                Source::Remote(image) => match media.cached_input(workspace, &key) {
                    Ok(Some(data)) => Ok(data),
                    Ok(None) => images::fetch_data_urls(std::slice::from_ref(image))
                        .await
                        .map(|mut values| values.remove(0)),
                    Err(error) => Err(error),
                },
                Source::Stored(image) => media
                    .read(image)
                    .map(|bytes| format!("data:{};base64,{}", image.mime, STANDARD.encode(bytes))),
            };
            match result {
                Ok(value) => {
                    keys.push(key);
                    data.push(value);
                    binding["status"] = json!("available");
                    binding["image_ref"] = json!(format!("input:{}", data.len()));
                }
                Err(error) if input.origin() == "request" => return Err(error),
                Err(_) => {
                    failed.push(key);
                }
            }
        }
        bindings.push(binding);
    }
    images::validate_social_data_urls(&data)?;
    Ok((data, bindings, keys))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::social::{ChatMessage, RequestRecord};
    use crate::social::store::private_directory;

    fn setup() -> (tempfile::TempDir, std::path::PathBuf, MediaStore) {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        private_directory(&workspace).unwrap();
        let media = MediaStore::new(root.path(), &format!("p_{}", "a".repeat(40))).unwrap();
        (root, workspace, media)
    }
    fn snapshot(media: &MediaStore, workspace: &std::path::Path, label: &str) -> OutputImage {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(label.as_bytes());
        std::fs::write(workspace.join("fixture.png"), bytes).unwrap();
        media
            .snapshot(workspace, &["/workspace/fixture.png".into()])
            .unwrap()
            .remove(0)
    }

    #[tokio::test]
    async fn exact_quote_pixels_are_staged_before_the_recent_batch_is_replaced() {
        let (_root, workspace, media) = setup();
        let old = snapshot(&media, &workspace, "quoted-old");
        let recent = snapshot(&media, &workspace, "unrelated-new");
        let cached = format!(
            "data:image/png;base64,{}",
            STANDARD.encode(media.read(&recent).unwrap())
        );
        media.stage_inputs(&workspace, &[cached]).unwrap();
        let (data, bindings, _) = prepare(
            &[Input {
                source: Source::Stored(old.clone()),
                seq: 9,
                quote_path: Some(vec![0]),
                context: false,
            }],
            &media,
            &workspace,
        )
        .await
        .unwrap();
        assert_eq!(
            data[0],
            format!(
                "data:image/png;base64,{}",
                STANDARD.encode(media.read(&old).unwrap())
            )
        );
        let files = media.stage_inputs(&workspace, &data).unwrap();
        assert_eq!(bindings[0]["image_ref"], files[0]["image_ref"]);
        assert_eq!(bindings[0]["quote_path"], json!([0]));
        assert!(!bindings[0].to_string().contains(&old.file));
        assert!(!bindings[0].to_string().contains("unrelated-new"));
    }

    #[tokio::test]
    async fn unavailable_quoted_images_do_not_fail_or_reuse_recent_images() {
        let (_root, workspace, media) = setup();
        let old = snapshot(&media, &workspace, "previous");
        media
            .stage_inputs(
                &workspace,
                &[format!(
                    "data:image/png;base64,{}",
                    STANDARD.encode(media.read(&old).unwrap())
                )],
            )
            .unwrap();
        let inputs = [Input {
            source: Source::Remote(RemoteImage {
                url: "https://example.com/disallowed.png".into(),
                content_type: "image/png".into(),
            }),
            seq: 9,
            quote_path: Some(vec![0]),
            context: false,
        }];
        let (data, bindings, _) = prepare(&inputs, &media, &workspace).await.unwrap();
        assert!(data.is_empty());
        assert_eq!(bindings[0]["status"], "unavailable");
        assert!(bindings[0]["image_ref"].is_null());
        assert!(!bindings[0].to_string().contains("example.com"));
        assert!(
            prepare(
                &[Input {
                    source: Source::Remote(RemoteImage {
                        url: "https://example.com/disallowed.png".into(),
                        content_type: "image/png".into()
                    }),
                    seq: 9,
                    quote_path: None,
                    context: false,
                }],
                &media,
                &workspace
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn an_exact_cached_source_is_used_without_refetching_its_expired_url() {
        let (_root, workspace, media) = setup();
        let original = snapshot(&media, &workspace, "original-input");
        let data = format!(
            "data:image/png;base64,{}",
            STANDARD.encode(media.read(&original).unwrap())
        );
        let url = "https://example.com/no-network-request-allowed";
        let key = format!("remote:{url}");
        media
            .stage_bound_inputs(
                &workspace,
                std::slice::from_ref(&data),
                std::slice::from_ref(&key),
            )
            .unwrap();
        let (actual, bindings, _) = prepare(
            &[Input {
                source: Source::Remote(RemoteImage {
                    url: url.into(),
                    content_type: "image/png".into(),
                }),
                seq: 10,
                quote_path: Some(vec![0]),
                context: false,
            }],
            &media,
            &workspace,
        )
        .await
        .unwrap();
        assert_eq!(actual, vec![data]);
        assert_eq!(bindings[0]["status"], "available");
        assert!(
            media
                .cached_input(&workspace, "remote:another-image")
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn repeated_exact_assets_share_pixels_and_excess_images_are_explicit() {
        let (_root, workspace, media) = setup();
        let assets = (0..4)
            .map(|i| snapshot(&media, &workspace, &i.to_string()))
            .collect::<Vec<_>>();
        let mut inputs = assets
            .iter()
            .enumerate()
            .map(|(index, image)| Input {
                source: Source::Stored(image.clone()),
                seq: 9,
                quote_path: Some(vec![index]),
                context: false,
            })
            .collect::<Vec<_>>();
        inputs.push(Input {
            source: Source::Stored(assets[0].clone()),
            seq: 10,
            quote_path: Some(vec![0]),
            context: false,
        });
        let (data, bindings, _) = prepare(&inputs, &media, &workspace).await.unwrap();
        assert_eq!(data.len(), 3);
        assert_eq!(bindings[3]["status"], "omitted_limit");
        assert_eq!(bindings[4]["image_ref"], "input:1");
    }

    #[tokio::test]
    async fn appended_image_bursts_keep_recent_pixels_without_failing_the_request() {
        let (_root, workspace, media) = setup();
        let assets = (0..5)
            .map(|i| snapshot(&media, &workspace, &i.to_string()))
            .collect::<Vec<_>>();
        let mut inputs = assets
            .iter()
            .enumerate()
            .map(|(i, asset)| Input {
                source: Source::Stored(asset.clone()),
                seq: i as u64 + 1,
                quote_path: None,
                context: false,
            })
            .collect::<Vec<_>>();
        let (data, bindings, _) = prepare(&inputs, &media, &workspace).await.unwrap();
        assert_eq!(data.len(), 3);
        assert_eq!(bindings.len(), 5);
        assert!(
            bindings[..2]
                .iter()
                .all(|b| b["status"] == "omitted_limit" && b["image_ref"].is_null())
        );
        for (index, binding) in bindings[2..].iter().enumerate() {
            assert_eq!(binding["source_seq"], index + 3);
            assert_eq!(binding["image_ref"], format!("input:{}", index + 1));
            assert_eq!(
                data[index],
                format!(
                    "data:image/png;base64,{}",
                    STANDARD.encode(media.read(&assets[index + 2]).unwrap())
                )
            );
        }
        for input in &mut inputs {
            input.source = Source::Stored(assets[0].clone());
        }
        let (data, bindings, _) = prepare(&inputs, &media, &workspace).await.unwrap();
        assert_eq!(data.len(), 1);
        assert!(bindings.iter().all(|b| b["image_ref"] == "input:1"));
    }

    #[test]
    fn current_quotes_and_window_images_keep_distinct_provenance() {
        let mut state = GroupState::default();
        let seq = state
            .ingest(
                ChatMessage {
                    id: "current".into(),
                    author_id: "member".into(),
                    quotes: vec![Quote {
                        parts: vec![Quote {
                            images: vec![crate::model::social::ImageAttachment {
                                url: "https://example.com/quoted".into(),
                                content_type: "image/png".into(),
                            }],
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                500,
            )
            .unwrap()
            .unwrap();
        state
            .ingest(
                ChatMessage {
                    id: "unrelated".into(),
                    author_id: "member".into(),
                    images: vec![crate::model::social::ImageAttachment {
                        url: "https://example.com/unrelated".into(),
                        content_type: "image/png".into(),
                    }],
                    ..Default::default()
                },
                500,
            )
            .unwrap();
        state.requests.insert(
            1,
            RequestRecord {
                source_seqs: vec![seq],
                ..Default::default()
            },
        );
        let inputs = collect(&state, 1, &Default::default()).unwrap();
        assert_eq!(inputs.len(), 2);
        assert!(inputs[1].context);
        assert_eq!(inputs[0].quote_path, Some(vec![0, 0]));
        assert_eq!(inputs[0].seq, seq);
    }

    #[tokio::test]
    async fn silent_screenshot_is_available_to_later_voice_with_its_original_identity() {
        let (_root, workspace, media) = setup();
        let mut state = GroupState::default();
        let image = snapshot(&media, &workspace, "silent-screenshot");
        let pixels = format!(
            "data:image/png;base64,{}",
            STANDARD.encode(media.read(&image).unwrap())
        );
        let url = "https://example.com/cached-screenshot";
        media
            .stage_bound_inputs(
                &workspace,
                std::slice::from_ref(&pixels),
                &[format!("remote:{url}")],
            )
            .unwrap();
        let image_seq = state
            .ingest(
                ChatMessage {
                    id: "silent-image".into(),
                    author_id: "member-a".into(),
                    timestamp_ms: 1,
                    images: vec![crate::model::social::ImageAttachment {
                        url: url.into(),
                        content_type: "image/png".into(),
                    }],
                    ..Default::default()
                },
                500,
            )
            .unwrap()
            .unwrap();
        let image_record = state.messages.last_mut().unwrap();
        image_record.application = Some("silent".into());
        let voice_seq = state
            .ingest(
                ChatMessage {
                    id: "voice-follow-up".into(),
                    author_id: "member-b".into(),
                    timestamp_ms: 1_000_000,
                    text: "看看这张截图".into(),
                    attachment_kinds: vec!["voice".into()],
                    ..Default::default()
                },
                500,
            )
            .unwrap()
            .unwrap();
        state.requests.insert(
            1,
            RequestRecord {
                source_seqs: vec![voice_seq],
                ..Default::default()
            },
        );
        let config = super::super::config::SocialConfig::default();
        let inputs = collect(&state, 1, &config).unwrap();
        let (data, bindings, _) = prepare(&inputs, &media, &workspace).await.unwrap();
        assert_eq!(data, vec![pixels]);
        assert_eq!(bindings[0]["source_seq"], image_seq);
        assert_eq!(bindings[0]["origin"], "context");
        let bound = super::super::sticker_agent::bind(&data, &bindings, &state);
        assert_eq!(bound.len(), 1);
        assert!(!bound[0].current);
        assert_eq!(bound[0].source_id, "silent-image");
        assert_eq!(state.requests[&1].source_seqs, vec![voice_seq]);

        let mut config = config;
        config.context_windows.codex_messages = Some(1);
        assert!(collect(&state, 1, &config).unwrap().is_empty());
        config.context_windows.codex_messages = Some(80);
        state.context_start_seq = voice_seq;
        assert!(collect(&state, 1, &config).unwrap().is_empty());
        state.context_start_seq = 0;
        state.messages[0].application = Some("command".into());
        assert!(collect(&state, 1, &config).unwrap().is_empty());
    }

    #[tokio::test]
    async fn request_and_explicit_quotes_take_priority_over_context_without_substitution() {
        let (_root, workspace, media) = setup();
        let assets = (0..4)
            .map(|i| snapshot(&media, &workspace, &i.to_string()))
            .collect::<Vec<_>>();
        let mut inputs = vec![
            Input {
                source: Source::Stored(assets[0].clone()),
                seq: 3,
                quote_path: None,
                context: false,
            },
            Input {
                source: Source::Stored(assets[1].clone()),
                seq: 3,
                quote_path: Some(vec![0]),
                context: false,
            },
            Input {
                source: Source::Stored(assets[2].clone()),
                seq: 2,
                quote_path: None,
                context: true,
            },
            Input {
                source: Source::Stored(assets[3].clone()),
                seq: 1,
                quote_path: None,
                context: true,
            },
        ];
        let (data, bindings, _) = prepare(&inputs, &media, &workspace).await.unwrap();
        assert_eq!(data.len(), 3);
        assert_eq!(bindings[2]["source_seq"], 2);
        assert_eq!(bindings[2]["status"], "available");
        assert_eq!(bindings[3]["status"], "omitted_limit");
        inputs[2].source = Source::Remote(RemoteImage {
            url: "https://example.com/not-allowed".into(),
            content_type: "image/png".into(),
        });
        let (data, bindings, _) = prepare(&inputs, &media, &workspace).await.unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(bindings[2]["status"], "unavailable");
        assert_eq!(bindings[3]["status"], "omitted_limit");
    }
}
