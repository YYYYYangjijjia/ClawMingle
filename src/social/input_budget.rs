//! Soft text budgets for a complete invocation, not a tokenizer or billing meter.
//! Irreplaceable task sources, persona and image attribution always win over size.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputBudget {
    pub chat_text_tokens: usize,
    pub task_text_tokens: usize,
    pub auto_compact_tokens: usize,
    pub tool_output_tokens: usize,
}
impl Default for InputBudget {
    fn default() -> Self {
        Self {
            chat_text_tokens: 12_000,
            task_text_tokens: 30_000,
            auto_compact_tokens: 60_000,
            tool_output_tokens: 4_000,
        }
    }
}
impl InputBudget {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (4_000..=60_000).contains(&self.chat_text_tokens)
                && (self.chat_text_tokens..=100_000).contains(&self.task_text_tokens)
                && (self.task_text_tokens..=200_000).contains(&self.auto_compact_tokens)
                && (1_000..=16_000).contains(&self.tool_output_tokens),
            "invalid_input_budget"
        );
        Ok(())
    }
    pub fn goal(&self, chat: bool) -> usize {
        if chat {
            self.chat_text_tokens
        } else {
            self.task_text_tokens
        }
    }
}

/// Conservative text-only approximation. Actual native usage (including images,
/// history and provider instructions) is logged independently from this estimate.
pub(super) fn estimate(text: &str) -> usize {
    let ascii = text.bytes().filter(u8::is_ascii).count();
    ascii.div_ceil(4) + text.chars().filter(|c| !c.is_ascii()).count()
}

pub(super) fn trim(
    input: &mut Value,
    fixed_tokens: usize,
    target: usize,
    can_reload: bool,
) -> Result<()> {
    let mut omitted = [0usize; 2];
    if can_reload {
        while fixed_tokens.saturating_add(estimate(&serde_json::to_string(input)?)) > target {
            let context = input["group_context"]
                .as_array_mut()
                .expect("context array");
            // Preserve the latest conversational exchange and every visual source.
            let removable = context.len().saturating_sub(4);
            if let Some(index) = context.iter().take(removable).position(|r| {
                r["image_count"].as_u64().unwrap_or(0) == 0
                    && r["quotes"].as_array().is_none_or(Vec::is_empty)
                    && r["voice_inputs"].as_array().is_none_or(Vec::is_empty)
            }) {
                context.remove(index);
                omitted[0] += 1;
            } else {
                let replies = input["already_delivered_bot_messages"]
                    .as_array_mut()
                    .expect("reply array");
                if replies.len() <= 2 {
                    break;
                }
                replies.remove(0);
                omitted[1] += 1;
            }
        }
    }
    input["input_budget"] = json!({"text_token_soft_target":target,
        "older_context_omitted":omitted[0],"older_deliveries_omitted":omitted[1],
        "notice":"Whole older background entries may be omitted for the text budget. Use message_lookup/messages_list to retrieve exact history when needed. Current task sources, recent exchange and visual provenance remain; these are not total billed tokens."});
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn large_background_shrinks_without_cutting_task_quotes_or_visuals() {
        let current = json!({"messages":[{"text":"required constraint".repeat(2000)}]});
        let mut input = json!({"current_request":current,"group_context":[],"already_delivered_bot_messages":[]});
        for n in 0..20 {
            input["group_context"].as_array_mut().unwrap().push(
                json!({"seq":n,"text":"background".repeat(1000),"image_count":if n==0 {1}else{0}}),
            );
        }
        let before = serde_json::to_string(&input).unwrap().len();
        trim(&mut input, 10_000, 12_000, true).unwrap();
        assert_eq!(input["current_request"], current);
        assert_eq!(input["group_context"][0]["seq"], 0);
        assert_eq!(input["group_context"].as_array().unwrap().len(), 5);
        assert!(serde_json::to_string(&input).unwrap().len() < before / 2);
        let unchanged = input.clone();
        trim(&mut input, 10_000, 12_000, false).unwrap();
        assert_eq!(input["group_context"], unchanged["group_context"]);
    }
}
