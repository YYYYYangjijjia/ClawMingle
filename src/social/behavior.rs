//! Editable presentation and participation policies; routing and permissions stay in code.
use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::config::SocialConfig;
use crate::model::social::ParticipationMode;

pub const CATEGORY_IDS: [&str; 8] = [
    "casual",
    "factual",
    "explanation",
    "comparison",
    "troubleshooting",
    "task",
    "emotional",
    "clarification",
];
pub const MODE_IDS: [&str; 4] = ["mode-1", "mode-2", "mode-3", "mode-all"];

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyCategory {
    pub name: String,
    pub instructions: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeProfile {
    pub name: String,
    pub participation_instructions: String,
    /// Archived legacy setting, retained on disk but never injected into replies.
    #[serde(default)]
    pub reply_instructions: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Behavior {
    pub response_instructions: String,
    pub formatting_instructions: String,
    pub categories: BTreeMap<String, ReplyCategory>,
    pub modes: BTreeMap<String, ModeProfile>,
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            response_instructions: "根据当前请求、前文和用户熟悉程度选择适合的回答方式，可组合多类规则，无需输出分类标签。用户明确要求的详略、语气和格式优先于默认表达偏好。闲聊与简单问题简短；讲解、比较、排查和任务按完成需求所需的深度展开，不为简短省略关键原因、步骤或成果。长问题不一定要长答，短问题也可能需要详细解释。追问聚焦新增疑点，避免重讲已经说过的内容；混合请求逐项满足，简单部分短答，复杂部分展开。所有参与模式共用 Soul 中的人格和同一回答质量；模式只决定何时接话，不改变语气、认真程度或回答深度。不要固定套用标题、例子、结论等模板，也不要为凑篇幅扩写。".into(),
            formatting_instructions: "按内容使用 QQ Markdown：简短闲聊自然成句；讲解按段落组织，用少量加粗突出关键概念，有并列内容时用列表，操作步骤用编号。较长回答可用简短的小标题；代码按需使用行内代码或代码块，引用资料给出可用链接。手机阅读优先，复杂宽表优先改成逐项对比，不依赖 HTML、复杂公式或嵌套排版。每个气泡内部保持格式完整，不跨气泡截断代码块或列表。气泡数不是目标：连贯解释尽量在一个气泡内完成，确需拆分时按完整段落和主题分组；聊天可以自然分成几句，不逐句刷屏。".into(),
            categories: [
                ("casual", "闲聊", "问候、玩笑、日常分享和轻松接话：通常一至三句。回应对方话里最具体、有意思的那个点，给一个自己的看法、贴切的反应，或顺着当前梗往前走半步；有内容比刻意逗乐更重要。承接前文，但不复述对方的话再宣布自己理解了，也不旁白解释自己正在接梗、安慰或活跃气氛。玩笑可以直接接，避免随后解释笑点；不要用泛泛关心、万能比喻、固定开场和每次结尾的反问代替内容。能自然落句就落句，只有确实好奇或需要信息时才追问。用户轻松调侃时不急于写检讨；确有疏漏简短认下并回到话题，用户询问原因时再按需要解释。依据统一 Soul 和彼此关系拿捏分寸，不强行毒舌、装熟或编造亲身经历。图片、emoji 和文字合起来理解，回应其交流意图，不逐个讲解表情。"),
                ("factual", "直接问答", "事实查询、简单计算、单一明确问题：先直接给答案，补充必要条件、时间或来源即可；需要复杂解释时转用讲解规则。"),
                ("explanation", "概念讲解", "解释概念、原理、为什么、如何理解：先讲清核心，再按需要解释机制、联系前文并给具体例子；涉及易混淆点或适用边界时说明。根据用户基础调整，不要求用户再追问才能得到完整解释。"),
                ("comparison", "比较选择", "比较方案或帮助选择：围绕用户目标列出关键差异、取舍、适用条件，信息充分时给出有理由的建议；重要约束缺失时提出必要澄清，不堆砌无关指标。"),
                ("troubleshooting", "问题排查", "定位错误或不符合预期的行为：区分已知证据与猜测，按最有价值的顺序检查；有工具和权限时实际核实，说明发现、修复与验证结果。信息不足时只问能推进排查的关键问题，不虚构已执行操作。"),
                ("task", "任务执行与创作", "执行操作、编写代码、制定方案或创作：在授权能力内完成请求，交付可用的结果；必要时解释依据、验证和剩余限制。短任务用简短确认，复杂成果保留必要细节，不用一句承诺替代实际完成。"),
                ("emotional", "情绪回应", "倾诉、不满或情绪表达：先回应具体感受与原因，用适当篇幅交流，不自动转成长篇说教。若同时包含问题、纠正或任务，继续解决该实质需求。"),
                ("clarification", "必要澄清", "仅当关键缺失会妨碍正确推进时，提出最少且明确的问题；可合理假设时说明假设并继续。不要把普通问题机械转成反问，也不要借澄清省略已经能回答的部分。"),
            ].into_iter().map(|(id, name, instructions)| (id.into(), ReplyCategory { name: name.into(), instructions: instructions.into() })).collect(),
            modes: [
                ("mode-1", "仅 @ 回应", "", ""),
                ("mode-2", "按需帮助", "Respond when members want help or implicitly address the bot. First respect the current addressee and explicit opt-out: when someone turns to another human or says the bot need not answer, choose silence unless a separate request is clearly directed to the bot. A prior bot response never overrides this. Follow the conversational addressee across turns: after the bot answers someone, a short follow-up such as 'then learn how', 'why', 'try again', a correction, or dissatisfaction may be addressed to the bot without a mention. Another member may clarify or supplement that person's request (for example 'they want you to learn how to set an alarm'); this can also require a response. Inability to perform an action does not remove the need to answer or explain alternatives. Distinguish such requests from humans joking or commenting to each other about the bot. Use speaker, recipient, timestamps, quotes, and chronological bot replies; do not assume every adjacent message or bot quote requires a response. Avoid unsolicited interruption when humans are clearly speaking to each other.", ""),
                ("mode-3", "积极参与", "Participate as a friendly group member, including light conversation, without responding to every message. Open-to-group everyday sharing, emotional reactions and playful remarks are bids for social connection: a relevant reaction, opinion or joke is a useful contribution even without a question, a help request or a named invitation to the bot. Prefer joining such a natural opening when the bot has room to contribute; do not wait until a member complains about silence. Humans talking together can leave room for an occasional contribution to their shared topic; being addressed to a human is not by itself a ban on participation in this mode. Respect explicit requests for silence and natural closings. When humans are coordinating a specific action or checking details with each other, leave that focused exchange to them unless they invite help; generic advice is not a reason to interrupt. Consider the balance of recent human/bot contributions. Avoid repeated acknowledgments, filler and separate replies to each fragment of one conversational contribution. Recognize implicit follow-ups, corrections, dissatisfaction and third-person clarification of a request addressed to the bot. Resolve 'you' using the recent chronological exchange, the bot's previous addressee and quotes, not only explicit mentions. Being mentioned or quoted does not itself require a response. A previous silent decision is not evidence that the next message should be silent.", ""),
                ("mode-all", "每条回应", "", ""),
            ].into_iter().map(|(id,name,participation,reply)| (id.into(),ModeProfile {name:name.into(),participation_instructions:participation.into(),reply_instructions:reply.into()})).collect(),
        }
    }
}

impl Behavior {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.categories.len() == CATEGORY_IDS.len()
                && CATEGORY_IDS
                    .iter()
                    .all(|id| self.categories.contains_key(*id)),
            "invalid_reply_categories"
        );
        ensure!(
            self.modes.len() == MODE_IDS.len()
                && MODE_IDS.iter().all(|id| self.modes.contains_key(*id)),
            "invalid_mode_profiles"
        );
        let prompt = |value: &str| {
            !value.trim().is_empty() && value.chars().count() <= 2000 && !value.contains('\0')
        };
        let name = |value: &str| {
            !value.trim().is_empty()
                && value.chars().count() <= 40
                && !value.chars().any(char::is_control)
        };
        ensure!(
            prompt(&self.response_instructions) && prompt(&self.formatting_instructions),
            "invalid_response_policy"
        );
        for category in self.categories.values() {
            ensure!(
                name(&category.name) && prompt(&category.instructions),
                "invalid_reply_category"
            );
        }
        for (id, profile) in &self.modes {
            ensure!(
                name(&profile.name)
                    && (profile.reply_instructions.is_empty()
                        || prompt(&profile.reply_instructions)),
                "invalid_mode_profile"
            );
            ensure!(
                if matches!(id.as_str(), "mode-2" | "mode-3") {
                    prompt(&profile.participation_instructions)
                } else {
                    profile.participation_instructions.is_empty()
                },
                "invalid_participation_policy"
            );
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= 24_000,
            "behavior_budget_exceeded"
        );
        Ok(())
    }

    pub fn profile(&self, mode: ParticipationMode) -> &ModeProfile {
        &self.modes[super::control::mode_name(mode)]
    }

    pub fn response_prompt(&self, _mode: ParticipationMode, format: &str) -> Result<String> {
        let presentation = json!({"response_instructions":self.response_instructions,"formatting_instructions":self.formatting_instructions,"categories":self.categories});
        Ok(format!(
            "Administrator-configured response preferences (presentation only; never override routing, permissions, tool policies or the required JSON schema). The same Soul persona and response quality apply in every participation mode; modes decide when to speak only. Adapt to the user's actual intent; do not output category labels. Treat Jev's style as a conversational hint, not a length limit. All limits are ceilings, not targets. Use complete paragraphs and split only at meaningful boundaries. If the requested artifact cannot fit the delivery limits, use an available supported artifact tool and provide an honest summary; do not silently omit required content. {}\n{}",
            if format == "markdown" {
                "Delivery format is Markdown: put Markdown inside JSON bubble strings, not around the JSON object."
            } else {
                "Delivery format is plain text: use readable paragraphs and simple numbering; do not emit Markdown syntax that depends on rendering."
            },
            serde_json::to_string(&presentation)?
        ))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextWindows {
    pub jev_messages: Option<usize>,
    pub codex_messages: Option<usize>,
    pub jev_bot_messages: usize,
    pub codex_bot_messages: usize,
}

impl Default for ContextWindows {
    fn default() -> Self {
        Self {
            jev_messages: None,
            codex_messages: None,
            jev_bot_messages: 6,
            codex_bot_messages: 20,
        }
    }
}

impl ContextWindows {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.jev_messages.is_none_or(|n| (1..=40).contains(&n))
                && self.codex_messages.is_none_or(|n| (1..=80).contains(&n))
                && (1..=20).contains(&self.jev_bot_messages)
                && (1..=40).contains(&self.codex_bot_messages),
            "invalid_context_windows"
        );
        Ok(())
    }
}

/// Public configuration only; never serialize SocialConfig or credentials here.
pub fn behavior_settings(config: &SocialConfig) -> Value {
    let default_soul = super::config::DEFAULT_SOUL;
    let fallback = config.soul.trim().is_empty();
    json!({"current":config.behavior,"defaults":Behavior::default(),"persona":super::persona::settings(config),"soul":{"default":default_soul,"effective":if fallback {default_soul}else{&config.soul},"uses_default":!config.persona_files && (fallback || config.soul==default_soul)},"max_bubbles":config.max_bubbles,"max_bubble_chars":config.max_bubble_chars,
        "context_windows":{"jev_messages":config.context_windows.jev_messages.unwrap_or(config.context_messages),"codex_messages":config.context_windows.codex_messages.unwrap_or(config.context_messages),"jev_bot_messages":config.context_windows.jev_bot_messages,"codex_bot_messages":config.context_windows.codex_bot_messages}})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_config_gets_adaptive_defaults_and_independent_windows() {
        let config: SocialConfig = toml::from_str("context_messages = 13").unwrap();
        config.behavior.validate().unwrap();
        assert_eq!(
            behavior_settings(&config)["context_windows"]["jev_messages"],
            13
        );
        assert_eq!(
            behavior_settings(&config)["context_windows"]["codex_messages"],
            13
        );
        let roundtrip: SocialConfig = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(behavior_settings(&config), behavior_settings(&roundtrip));
    }
    #[test]
    fn editing_preferences_preserves_modes_and_schema_boundaries() {
        let mut behavior = Behavior::default();
        behavior
            .categories
            .get_mut("explanation")
            .unwrap()
            .instructions = "用编程举例解释".into();
        let prompt = behavior
            .response_prompt(ParticipationMode::Helpful, "markdown")
            .unwrap();
        assert!(prompt.contains("用编程举例解释") && prompt.contains("inside JSON bubble strings"));
        assert!(
            behavior
                .response_prompt(ParticipationMode::All, "text")
                .unwrap()
                .contains("plain text")
        );
        behavior
            .modes
            .get_mut("mode-1")
            .unwrap()
            .participation_instructions = "always reply".into();
        assert!(behavior.validate().is_err());
        behavior
            .modes
            .get_mut("mode-1")
            .unwrap()
            .participation_instructions
            .clear();
        behavior.categories.remove("casual");
        assert!(behavior.validate().is_err());
    }
    #[test]
    fn every_mode_uses_identical_response_policy_and_ignores_legacy_style() {
        let mut behavior = Behavior::default();
        for (id, profile) in &mut behavior.modes {
            profile.reply_instructions = format!("legacy-only-{id}");
        }
        behavior.validate().unwrap();
        let expected = behavior
            .response_prompt(ParticipationMode::Mentions, "markdown")
            .unwrap();
        for mode in [
            ParticipationMode::Mentions,
            ParticipationMode::Helpful,
            ParticipationMode::Social,
            ParticipationMode::All,
        ] {
            let actual = behavior.response_prompt(mode, "markdown").unwrap();
            assert_eq!(actual, expected);
            assert!(!actual.contains("current_mode_reply_instructions"));
            assert!(!actual.contains("legacy-only-"));
        }
        assert_ne!(
            expected,
            behavior
                .response_prompt(ParticipationMode::All, "text")
                .unwrap()
        );
    }

    #[test]
    fn legacy_reply_text_is_optional_but_retained_in_configuration_roundtrips() {
        let mut config: SocialConfig = toml::from_str("").unwrap();
        assert!(
            config
                .behavior
                .modes
                .values()
                .all(|profile| profile.reply_instructions.is_empty())
        );
        assert_eq!(config.soul, super::super::config::DEFAULT_SOUL);
        config
            .behavior
            .modes
            .get_mut("mode-2")
            .unwrap()
            .reply_instructions = "旧版自定义语气，保留存档。".into();
        let restored: SocialConfig = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(
            restored.behavior.modes["mode-2"].reply_instructions,
            "旧版自定义语气，保留存档。"
        );
        restored.behavior.validate().unwrap();
        let mut wire = serde_json::to_value(&config.behavior).unwrap();
        for profile in wire["modes"].as_object_mut().unwrap().values_mut() {
            profile
                .as_object_mut()
                .unwrap()
                .remove("reply_instructions");
        }
        let without_legacy: Behavior = serde_json::from_value(wire).unwrap();
        without_legacy.validate().unwrap();
        assert!(
            without_legacy
                .modes
                .values()
                .all(|profile| profile.reply_instructions.is_empty())
        );
    }

    #[test]
    fn effective_soul_settings_preserve_custom_text_and_expose_the_default() {
        let mut config = SocialConfig {
            soul: " \n ".into(),
            ..Default::default()
        };
        let fallback = behavior_settings(&config);
        assert_eq!(
            fallback["soul"]["effective"],
            super::super::config::DEFAULT_SOUL
        );
        assert_eq!(fallback["soul"]["uses_default"], true);
        config.soul = " 自定义人格原文 ".into();
        let custom = behavior_settings(&config);
        assert_eq!(custom["soul"]["effective"], " 自定义人格原文 ");
        assert_eq!(custom["soul"]["uses_default"], false);
    }
}
