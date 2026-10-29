//! Rate-limited, content-free evidence for social-turn progress tracking.
use std::time::{Duration, Instant};

use serde_json::Value;

#[derive(Default)]
pub(super) struct ActivityGate {
    last: Option<Instant>,
}
impl ActivityGate {
    pub(super) fn observe(
        &mut self,
        method: &str,
        params: &Value,
        thread: &str,
        turn: &str,
        now: Instant,
    ) -> bool {
        // The broader runner also accepts global error notifications. They are
        // not evidence that THIS turn is progressing, nor is another turn's I/O.
        let execution = matches!(
            method,
            "item/started"
                | "item/completed"
                | "item/agentMessage/delta"
                | "item/reasoning/textDelta"
                | "item/reasoning/summaryTextDelta"
                | "item/commandExecution/outputDelta"
                | "item/mcpToolCall/progress"
                | "turn/plan/updated"
                | "thread/tokenUsage/updated"
        );
        if !execution
            || params.get("threadId").and_then(Value::as_str) != Some(thread)
            || params.get("turnId").and_then(Value::as_str) != Some(turn)
            || self
                .last
                .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(5))
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn current_turn_activity_is_content_free_and_rate_limited() {
        let mut gate = ActivityGate::default();
        let now = Instant::now();
        let event = json!({"threadId":"thread","turnId":"turn","delta":"private model text"});
        assert!(gate.observe("item/agentMessage/delta", &event, "thread", "turn", now));
        assert!(!gate.observe(
            "item/completed",
            &event,
            "thread",
            "turn",
            now + Duration::from_secs(4)
        ));
        assert!(gate.observe(
            "item/completed",
            &event,
            "thread",
            "turn",
            now + Duration::from_secs(5)
        ));
    }

    #[test]
    fn supported_wire_notifications_include_thread_and_turn() {
        // Names and required IDs verified against the installed app-server's
        // generated ServerNotification schema; no legacy item/updated event.
        let event = json!({"threadId":"thread","turnId":"turn"});
        for method in [
            "item/started",
            "item/completed",
            "item/agentMessage/delta",
            "item/reasoning/textDelta",
            "item/reasoning/summaryTextDelta",
            "item/commandExecution/outputDelta",
            "item/mcpToolCall/progress",
            "turn/plan/updated",
            "thread/tokenUsage/updated",
        ] {
            assert!(ActivityGate::default().observe(
                method,
                &event,
                "thread",
                "turn",
                Instant::now()
            ));
        }
        assert!(!ActivityGate::default().observe(
            "item/updated",
            &event,
            "thread",
            "turn",
            Instant::now()
        ));
    }

    #[test]
    fn unrelated_or_unscoped_events_never_keep_a_task_alive() {
        let now = Instant::now();
        for event in [
            json!({}),
            json!({"threadId":"thread"}),
            json!({"turnId":"turn"}),
            json!({"threadId":"other","turnId":"turn"}),
            json!({"threadId":"thread","turnId":"other"}),
        ] {
            assert!(!ActivityGate::default().observe(
                "item/completed",
                &event,
                "thread",
                "turn",
                now
            ));
        }
        assert!(!ActivityGate::default().observe(
            "account/updated",
            &json!({"threadId":"thread","turnId":"turn"}),
            "thread",
            "turn",
            now
        ));
    }
}
