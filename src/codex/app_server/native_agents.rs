//! Native subagent ownership evidence from Codex 0.161 notifications.
//!
//! These parsers do not authorize a tool call. The caller must authenticate the
//! app-server connection and match the parent thread/turn to an active lease.
//! In particular, persisted ancestry and `sessionId` do not identify the current
//! turn. An old descendant must not acquire a new lease from metadata alone.

use serde_json::Value;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NativeSpawn<'a> {
    pub(crate) parent_thread_id: &'a str,
    pub(crate) parent_turn_id: &'a str,
    pub(crate) child_thread_ids: Vec<&'a str>,
}

/// Read completed native spawn events, never model-supplied tool arguments.
///
/// V1 emits `collabAgentToolCall`; V2 emits `subAgentActivity`. V2 collaborator
/// analytics contain a similar V1 item internally, but do not publish it on the
/// app-server notification stream. `interacted` is a follow-up, not a new spawn.
pub(crate) fn spawned_children<'a>(method: &str, params: &'a Value) -> Option<NativeSpawn<'a>> {
    if method != "item/completed" {
        return None;
    }
    let parent_thread_id = nonempty(params.get("threadId")?)?;
    let parent_turn_id = nonempty(params.get("turnId")?)?;
    let item = params.get("item")?;
    let child_thread_ids = match item.get("type")?.as_str()? {
        "collabAgentToolCall" => {
            if item.get("tool")?.as_str()? != "spawnAgent"
                || item.get("status")?.as_str()? != "completed"
                || item.get("senderThreadId")?.as_str()? != parent_thread_id
            {
                return None;
            }
            item.get("receiverThreadIds")?
                .as_array()?
                .iter()
                .map(nonempty)
                .collect::<Option<Vec<_>>>()?
        }
        "subAgentActivity" => {
            if item.get("kind")?.as_str()? != "started" {
                return None;
            }
            vec![nonempty(item.get("agentThreadId")?)?]
        }
        _ => return None,
    };
    if child_thread_ids.is_empty()
        || child_thread_ids.contains(&parent_thread_id)
        || child_thread_ids
            .iter()
            .enumerate()
            .any(|(index, child)| child_thread_ids[..index].contains(child))
    {
        return None;
    }
    Some(NativeSpawn {
        parent_thread_id,
        parent_turn_id,
        child_thread_ids,
    })
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn v1_spawn() -> Value {
        json!({
            "threadId": "root", "turnId": "root-turn",
            "item": {
                "type": "collabAgentToolCall", "tool": "spawnAgent",
                "status": "completed", "senderThreadId": "root",
                "receiverThreadIds": ["child"]
            }
        })
    }

    fn v2_spawn() -> Value {
        json!({
            "threadId": "root", "turnId": "root-turn",
            "item": {
                "type": "subAgentActivity", "kind": "started",
                "agentThreadId": "child", "agentPath": "/root/worker"
            }
        })
    }

    #[test]
    fn recognizes_both_native_protocol_versions() {
        for value in [v1_spawn(), v2_spawn()] {
            assert_eq!(
                spawned_children("item/completed", &value),
                Some(NativeSpawn {
                    parent_thread_id: "root",
                    parent_turn_id: "root-turn",
                    child_thread_ids: vec!["child"],
                })
            );
            assert!(spawned_children("item/started", &value).is_none());
            assert!(spawned_children("item/tool/call", &value).is_none());
        }
    }

    #[test]
    fn followups_and_terminal_activity_cannot_authorize_old_children() {
        for kind in ["interacted", "interrupted", "completed"] {
            let mut value = v2_spawn();
            value["item"]["kind"] = json!(kind);
            assert!(spawned_children("item/completed", &value).is_none());
        }
        for tool in ["resumeAgent", "sendInput", "followupTask", "wait"] {
            let mut value = v1_spawn();
            value["item"]["tool"] = json!(tool);
            assert!(spawned_children("item/completed", &value).is_none());
        }
    }

    #[test]
    fn requires_parent_turn_and_consistent_native_sender() {
        for (key, replacement) in [("threadId", json!("")), ("turnId", Value::Null)] {
            let mut value = v2_spawn();
            value[key] = replacement;
            assert!(spawned_children("item/completed", &value).is_none());
        }
        let mut value = v1_spawn();
        value["item"]["senderThreadId"] = json!("other-root");
        assert!(spawned_children("item/completed", &value).is_none());
        value["item"]["senderThreadId"] = json!("root");
        value["item"]["status"] = json!("failed");
        assert!(spawned_children("item/completed", &value).is_none());
    }

    #[test]
    fn rejects_missing_recursive_duplicate_and_malformed_child_ids() {
        for ids in [
            json!([]),
            json!(["root"]),
            json!(["child", "child"]),
            json!(["child", null]),
            json!([""]),
        ] {
            let mut value = v1_spawn();
            value["item"]["receiverThreadIds"] = ids;
            assert!(spawned_children("item/completed", &value).is_none());
        }
    }
}
