//! The same immutable declarations feed wire registration and prompt budgeting.
use std::sync::OnceLock;

use crate::codex::app_server::ToolCatalog;

pub(super) fn catalog() -> &'static ToolCatalog {
    static CATALOG: OnceLock<ToolCatalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        ToolCatalog::new(
            vec![
                super::persona_control::schema(),
                super::group_profile::schema(),
                super::documents::schema(),
                super::sticker_agent::schema(false),
                super::sticker_agent::schema(true),
            ],
            super::sticker_agent::tool_action_allowed,
        )
        .expect("static social tool catalog")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::app_server::social_tool_policy::{self, DEFAULT_NAMES};
    use serde_json::json;

    #[test]
    fn host_declarations_keep_the_wire_contract_and_sticker_policy_in_sync() {
        let names: Vec<_> = DEFAULT_NAMES.iter().map(|name| (*name).into()).collect();
        let declarations = social_tool_policy::definitions(&names, catalog());
        let tools = declarations[0]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), DEFAULT_NAMES.len());
        assert_eq!(declarations[0]["name"], social_tool_policy::NAMESPACE);
        for (tool, name) in tools.iter().zip(DEFAULT_NAMES) {
            assert_eq!(tool["name"], *name);
            assert_eq!(tool["type"], "function");
            assert_eq!(tool["deferLoading"], false);
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        }
        for original in [
            super::super::persona_control::schema(),
            super::super::group_profile::schema(),
            super::super::documents::schema(),
            super::super::sticker_agent::schema(false),
            super::super::sticker_agent::schema(true),
        ] {
            assert_eq!(
                tools.iter().find(|tool| tool["name"] == original["name"]),
                Some(&original)
            );
        }
        let policy = catalog().sticker_action_policy().unwrap();
        for name in ["sticker_lookup", "sticker_manage"] {
            let tool = tools.iter().find(|tool| tool["name"] == name).unwrap();
            assert_eq!(tool["inputSchema"]["required"], json!(["action"]));
            for action in tool["inputSchema"]["properties"]["action"]["enum"]
                .as_array()
                .unwrap()
            {
                let args = json!({"action":action}).to_string();
                assert!(policy(name, &args));
                let other = if name == "sticker_lookup" {
                    "sticker_manage"
                } else {
                    "sticker_lookup"
                };
                assert!(!policy(other, &args));
            }
            assert!(!policy(name, "not JSON"));
            assert!(!policy(name, r#"{"action":"unknown"}"#));
        }
        assert!(!policy("workspace_exec", r#"{"action":"search"}"#));
        assert_eq!(
            social_tool_policy::definitions(
                &names.into_iter().rev().collect::<Vec<_>>(),
                catalog()
            ),
            declarations
        );
    }
}
