//! Stable public request codes, separate from persisted routing IDs/revisions.
use chrono::{DateTime, FixedOffset, NaiveDateTime};

use crate::model::social::{GroupState, RequestRecord};

pub(super) fn label(request: &RequestRecord) -> String {
    request
        .display_id
        .clone()
        .unwrap_or_else(|| format!("R{}", request.id))
}

pub(super) fn allocate(state: &mut GroupState, id: u64, timestamp_ms: i64) -> Option<String> {
    if timestamp_ms <= 0 {
        return None;
    }
    let time = DateTime::from_timestamp_millis(timestamp_ms)?;
    let second = timestamp_ms / 1000;
    let base = time
        .with_timezone(&FixedOffset::east_opt(8 * 3600)?)
        .format("%Y%m%d-%H%M%S")
        .to_string();
    // A monotonically allocated internal ID disambiguates same-second requests,
    // late events and wall-clock rollback, including previously pruned requests.
    let code = if state
        .request_time_high_watermark
        .is_some_and(|last| second <= last)
    {
        format!("{base}-{id}")
    } else {
        base
    };
    state.request_time_high_watermark = Some(
        state
            .request_time_high_watermark
            .unwrap_or(second)
            .max(second),
    );
    Some(code)
}

/// Upgrade retained records from their first source, never from a later edit.
/// Missing old evidence retains the legacy ID rather than fabricating a date.
pub(super) fn migrate(state: &mut GroupState) {
    let pending: Vec<_> = state
        .requests
        .values()
        .filter(|r| !r.control && r.display_id.is_none())
        .filter_map(|r| {
            r.source_seqs
                .first()
                .and_then(|seq| state.record(*seq).ok())
                .map(|m| (r.id, m.message.timestamp_ms))
        })
        .collect();
    for (id, at) in pending {
        let code = allocate(state, id, at);
        state
            .requests
            .get_mut(&id)
            .expect("retained request")
            .display_id = code;
    }
}

pub(super) fn valid_code(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 15
        || bytes[8] != b'-'
        || !bytes[..8]
            .iter()
            .chain(bytes[9..15].iter())
            .all(u8::is_ascii_digit)
    {
        return false;
    }
    if NaiveDateTime::parse_from_str(&value[..15], "%Y%m%d-%H%M%S").is_err() {
        return false;
    }
    bytes.len() == 15
        || (bytes[15] == b'-'
            && value[16..]
                .parse::<u64>()
                .is_ok_and(|v| v > 0 && v.to_string() == value[16..]))
}

pub(super) fn resolve(state: &GroupState, value: &str) -> Option<u64> {
    if valid_code(value) {
        return state
            .requests
            .values()
            .find(|r| !r.control && r.display_id.as_deref() == Some(value))
            .map(|r| r.id);
    }
    // Also retain the historical numeric /continue spelling.
    let digits = value.strip_prefix('R').unwrap_or(value);
    if digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|id| *id > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timestamp_is_beijing_and_collision_survives_pruning_restart() {
        let mut state = GroupState::default();
        let at = DateTime::parse_from_rfc3339("2026-10-25T06:32:05Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            allocate(&mut state, 94, at).as_deref(),
            Some("20261025-143205")
        );
        assert_eq!(
            allocate(&mut state, 95, at + 200).as_deref(),
            Some("20261025-143205-95")
        );
        let mut restored: GroupState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(
            allocate(&mut restored, 96, at).as_deref(),
            Some("20261025-143205-96")
        );
        assert_eq!(
            allocate(&mut restored, 97, at + 1000).as_deref(),
            Some("20261025-143206")
        );
    }
    #[test]
    fn strict_codes_and_scope_local_resolution() {
        for code in ["20261025-143205", "20261025-143205-95"] {
            assert!(valid_code(code));
        }
        for code in [
            "20260230-143205",
            "20261025-253205",
            "20261025-143205-0",
            "20261025-143205-01",
            "20261025-143205-",
            "20261025-143205-😀",
            "20261025-14320😀",
        ] {
            assert!(!valid_code(code));
        }
        let mut state = GroupState::default();
        state.requests.insert(
            94,
            RequestRecord {
                id: 94,
                display_id: Some("20261025-143205".into()),
                ..Default::default()
            },
        );
        assert_eq!(resolve(&state, "20261025-143205"), Some(94));
        assert_eq!(resolve(&GroupState::default(), "20261025-143205"), None);
        assert_eq!(resolve(&state, "R94"), Some(94));
    }

    #[test]
    fn migration_uses_initial_source_and_never_renames_after_revision_or_reload() {
        use crate::model::social::{ChatMessage, MessageRecord};
        let mut state = GroupState::default();
        let at = DateTime::parse_from_rfc3339("2026-10-25T06:32:05Z")
            .unwrap()
            .timestamp_millis();
        state.messages.push(MessageRecord {
            seq: 1,
            message: ChatMessage {
                timestamp_ms: at,
                ..Default::default()
            },
            ..Default::default()
        });
        state.requests.insert(
            94,
            RequestRecord {
                id: 94,
                source_seqs: vec![1],
                updated_at_ms: at + 300_000,
                ..Default::default()
            },
        );
        migrate(&mut state);
        assert_eq!(label(&state.requests[&94]), "20261025-143205");
        state.requests.get_mut(&94).unwrap().revision += 1;
        state.messages.clear();
        let mut restored: GroupState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        migrate(&mut restored);
        assert_eq!(label(&restored.requests[&94]), "20261025-143205");
        restored.requests.insert(
            95,
            RequestRecord {
                id: 95,
                source_seqs: vec![2],
                updated_at_ms: at,
                ..Default::default()
            },
        );
        migrate(&mut restored);
        assert_eq!(label(&restored.requests[&95]), "R95");
    }
}
