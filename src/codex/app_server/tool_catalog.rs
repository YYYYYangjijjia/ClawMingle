//! Immutable host-owned declarations. Registration supplies schemas, never grants.
use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Result, ensure};
use serde_json::Value;

pub type StickerActionPolicy = fn(&str, &str) -> bool;

const HOST_NAMES: [&str; 5] = [
    "identity_manage",
    "group_profile_manage",
    "group_documents",
    "sticker_lookup",
    "sticker_manage",
];

/// A frozen set of application schemas and the synchronous sticker action policy.
/// An empty catalog supports turns without host tools. It cannot declare them.
#[derive(Clone, Default)]
pub struct ToolCatalog {
    schemas: Arc<BTreeMap<String, Value>>,
    sticker_action_policy: Option<StickerActionPolicy>,
}

impl ToolCatalog {
    pub fn new(schemas: Vec<Value>, sticker_action_policy: StickerActionPolicy) -> Result<Self> {
        ensure!(
            schemas.len() == HOST_NAMES.len(),
            "invalid_host_tool_catalog"
        );
        let mut registered = BTreeMap::new();
        for schema in schemas {
            let name = schema["name"]
                .as_str()
                .filter(|name| HOST_NAMES.contains(name))
                .ok_or_else(|| anyhow::anyhow!("invalid_host_tool_catalog"))?
                .to_owned();
            ensure!(
                schema["type"] == "function"
                    && schema["deferLoading"] == false
                    && schema["inputSchema"]["type"] == "object"
                    && schema["inputSchema"]["additionalProperties"] == false
                    && !registered.contains_key(&name),
                "invalid_host_tool_catalog"
            );
            registered.insert(name, schema);
        }
        Ok(Self {
            schemas: Arc::new(registered),
            sticker_action_policy: Some(sticker_action_policy),
        })
    }

    pub(crate) fn validate_for(&self, enabled: &[String]) -> Result<()> {
        ensure!(
            enabled
                .iter()
                .filter(|name| HOST_NAMES.contains(&name.as_str()))
                .all(|name| self.schemas.contains_key(name)),
            "host_tool_registration_missing"
        );
        Ok(())
    }

    pub(crate) fn definition(&self, name: &str) -> Option<&Value> {
        self.schemas.get(name)
    }

    pub(crate) fn sticker_action_policy(&self) -> Result<StickerActionPolicy> {
        self.sticker_action_policy
            .ok_or_else(|| anyhow::anyhow!("host_tool_registration_missing"))
    }

    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        Self::new(
            HOST_NAMES
                .iter()
                .map(|name| {
                    serde_json::json!({
                        "type":"function", "name":name, "deferLoading":false,
                        "inputSchema":{"type":"object", "properties":{"action":{"type":"string"}},
                            "required":["action"], "additionalProperties":false}
                    })
                })
                .collect(),
            |_, _| true,
        )
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::app_server::social_tool_policy;

    #[test]
    fn missing_registration_fails_closed_without_disabling_builtin_or_empty_turns() {
        let empty = ToolCatalog::default();
        assert!(empty.validate_for(&[]).is_ok());
        let builtin = vec!["time_now".into()];
        assert!(empty.validate_for(&builtin).is_ok());
        assert!(!social_tool_policy::definitions(&builtin, &empty).is_empty());
        for name in HOST_NAMES {
            let enabled = vec![name.into()];
            assert!(empty.validate_for(&enabled).is_err());
            assert!(social_tool_policy::definitions(&enabled, &empty).is_empty());
        }
        assert!(empty.sticker_action_policy().is_err());
    }

    #[test]
    fn registration_cannot_replace_builtin_names_duplicate_tools_or_bad_schemas() {
        let originals: Vec<_> = ToolCatalog::fixture().schemas.values().cloned().collect();
        let mut missing = originals.clone();
        missing.pop();
        assert!(ToolCatalog::new(missing, |_, _| true).is_err());
        let mut duplicate = originals.clone();
        duplicate[1] = duplicate[0].clone();
        assert!(ToolCatalog::new(duplicate, |_, _| true).is_err());
        for (field, value) in [
            ("name", serde_json::json!("workspace_exec")),
            ("name", serde_json::json!("unregistered_tool")),
            ("type", serde_json::json!("namespace")),
            ("deferLoading", serde_json::json!(true)),
            ("inputSchema", serde_json::json!({"type":"string"})),
        ] {
            let mut schemas = originals.clone();
            schemas[0][field] = value;
            assert!(ToolCatalog::new(schemas, |_, _| true).is_err());
        }
    }
}
