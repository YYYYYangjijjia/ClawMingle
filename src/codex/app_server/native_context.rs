//! Conservative input receipts from the isolated runtime's persisted rollout.
//! No display transcript, model-provided path, model call or history mutation.

use std::{
    fs::File,
    io::{BufRead, BufReader, Read},
    path::{Component, Path},
};

use anyhow::{Result, ensure};
use serde_json::Value;
use sha1::{Digest, Sha1};

const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_LINE: usize = 8 * 1024 * 1024;

pub(super) fn contains_inputs(
    root: &Path,
    native_path: &str,
    thread_id: &str,
    fingerprints: &[String],
) -> Result<bool> {
    let relative = Path::new(native_path).strip_prefix(super::SOCIAL_CODEX_HOME)?;
    ensure!(
        relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
            && matches!(relative.components().next(), Some(Component::Normal(name)) if name == "sessions"),
        "context_history_path_invalid"
    );
    let root = root.canonicalize()?;
    let path = root.join(relative).canonicalize()?;
    ensure!(
        path.starts_with(root.join("sessions")),
        "context_history_path_outside_runtime"
    );
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_BYTES as u64,
        "context_history_size_limit"
    );
    retained_inputs(BufReader::new(file), thread_id, fingerprints)
}

fn retained_inputs(
    mut reader: impl BufRead,
    thread_id: &str,
    fingerprints: &[String],
) -> Result<bool> {
    let mut found = vec![false; fingerprints.len()];
    let mut identity_verified = false;
    let mut total = 0;
    let mut line = Vec::new();
    loop {
        line.clear();
        let count = reader
            .by_ref()
            .take((MAX_LINE + 1) as u64)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        total += count;
        ensure!(
            count <= MAX_LINE && total <= MAX_BYTES && line.last() == Some(&b'\n'),
            "context_history_incomplete_or_oversized"
        );
        let record: Value = serde_json::from_slice(&line)?;
        let payload = &record["payload"];
        match record["type"].as_str() {
            Some("session_meta") => {
                ensure!(
                    !identity_verified && payload["id"] == thread_id,
                    "context_history_thread_mismatch"
                );
                identity_verified = true;
            }
            Some("compacted") => found.fill(false),
            Some("event_msg") => {
                // Clear even without a compaction-completed display event.
                // Rollback may remove whole user turns: never infer survivors.
                if matches!(
                    payload["type"].as_str(),
                    Some("thread_rolled_back" | "context_compacted")
                ) {
                    found.fill(false);
                }
            }
            Some("response_item") => {
                if payload["type"] == "message"
                    && payload["role"] == "user"
                    && let Some(content) = payload["content"].as_array()
                {
                    for part in content {
                        if part["type"] == "input_text"
                            && let Some(text) = part["text"].as_str()
                        {
                            let hash = format!("{:x}", Sha1::digest(text.as_bytes()));
                            for (index, wanted) in fingerprints.iter().enumerate() {
                                found[index] |= &hash == wanted;
                            }
                        }
                    }
                }
            }
            // Native environment baseline and usage accounting do not remove
            // conversation inputs (Codex 0.161 persisted rollout schema).
            Some("turn_context" | "world_state" | "token_usage_record") => {}
            // A new native history operation has unknown retention semantics.
            _ => return Ok(false),
        }
    }
    Ok(identity_verified && !found.is_empty() && found.iter().all(|found| *found))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hash(text: &str) -> String {
        format!("{:x}", Sha1::digest(text.as_bytes()))
    }
    fn input(text: &str) -> Value {
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}})
    }
    fn rollout(items: Vec<Value>) -> Vec<u8> {
        let mut lines = vec![json!({"type":"session_meta","payload":{"id":"task"}})];
        lines.extend(items);
        lines
            .into_iter()
            .flat_map(|item| format!("{item}\n").into_bytes())
            .collect()
    }
    #[test]
    fn exact_persisted_input_not_ack_display_event_or_tool_output() {
        let wanted = vec![hash("policy"), hash("delta")];
        let data = rollout(vec![input("policy"), input("delta")]);
        assert!(retained_inputs(data.as_slice(), "task", &wanted).unwrap());
        assert!(retained_inputs(data.as_slice(), "another", &wanted).is_err());
        let data = rollout(vec![
            input("policy"),
            json!({"type":"world_state","payload":{"kind":"full"}}),
            json!({"type":"token_usage_record","payload":{"input_tokens":100}}),
            input("delta"),
        ]);
        assert!(retained_inputs(data.as_slice(), "task", &wanted).unwrap());
        for false_input in [
            input("altered"),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"delta"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","output":"delta"}}),
        ] {
            assert!(
                !retained_inputs(
                    rollout(vec![input("policy"), false_input]).as_slice(),
                    "task",
                    &wanted
                )
                .unwrap()
            );
        }
    }
    #[test]
    fn compaction_without_completion_event_and_rollback_invalidate_old_inputs() {
        let wanted = vec![hash("policy"), hash("delta")];
        for cut in [
            json!({"type":"compacted","payload":{"message":"summary","replacement_history":[]}}),
            json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}),
        ] {
            assert!(
                !retained_inputs(
                    rollout(vec![input("policy"), input("delta"), cut.clone()]).as_slice(),
                    "task",
                    &wanted
                )
                .unwrap()
            );
            assert!(
                !retained_inputs(
                    rollout(vec![input("policy"), cut.clone(), input("delta")]).as_slice(),
                    "task",
                    &wanted
                )
                .unwrap()
            );
            assert!(
                retained_inputs(
                    rollout(vec![cut, input("policy"), input("delta")]).as_slice(),
                    "task",
                    &wanted
                )
                .unwrap()
            );
        }
    }
    #[test]
    fn partial_corrupt_unknown_or_oversized_histories_do_not_deduplicate() {
        let wanted = vec![hash("input")];
        let mut data = rollout(vec![input("input")]);
        data.pop();
        assert!(retained_inputs(data.as_slice(), "task", &wanted).is_err());
        assert!(retained_inputs(b"invalid\n".as_slice(), "task", &wanted).is_err());
        assert!(
            !retained_inputs(
                rollout(vec![
                    input("input"),
                    json!({"type":"future_history_operation"})
                ])
                .as_slice(),
                "task",
                &wanted
            )
            .unwrap()
        );
        assert!(retained_inputs(vec![b' '; MAX_LINE + 1].as_slice(), "task", &wanted).is_err());
    }
    #[test]
    fn only_runtime_session_paths_are_read() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sessions")).unwrap();
        std::fs::write(
            root.path().join("sessions/rollout.jsonl"),
            rollout(vec![input("input")]),
        )
        .unwrap();
        let wanted = vec![hash("input")];
        assert!(
            contains_inputs(
                root.path(),
                "/state/codex/sessions/rollout.jsonl",
                "task",
                &wanted
            )
            .unwrap()
        );
        for path in [
            "/state/codex/auth.json",
            "/other/sessions/rollout.jsonl",
            "/state/codex/sessions/../auth.json",
        ] {
            assert!(contains_inputs(root.path(), path, "task", &wanted).is_err());
        }
        #[cfg(unix)]
        {
            let outside = tempfile::NamedTempFile::new().unwrap();
            std::os::unix::fs::symlink(outside.path(), root.path().join("sessions/link.jsonl"))
                .unwrap();
            assert!(
                contains_inputs(
                    root.path(),
                    "/state/codex/sessions/link.jsonl",
                    "task",
                    &wanted
                )
                .is_err()
            );
        }
    }
}
