//! Reviewed provider schemas, discovered on demand rather than injected into
//! every conversation. Provider file targets are always supplied by the host.
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::sync::OnceLock;

fn catalog() -> &'static [Value] {
    static CATALOG: OnceLock<Value> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("catalog.json")).expect("document catalog")
    })["tools"]
        .as_array()
        .expect("document tools")
}

pub(crate) fn tool(name: &str) -> Result<&'static Value> {
    catalog()
        .iter()
        .find(|t| t["name"] == name)
        .ok_or_else(|| anyhow::anyhow!("documents_tool_denied"))
}

pub(crate) fn route(name: &str) -> Result<(&'static str, &'static str)> {
    let t = tool(name)?;
    let endpoint = match t["service"].as_str() {
        Some("doc") => "https://docs.qq.com/api/v6/doc/mcp",
        Some("sheet") => "https://docs.qq.com/api/v6/sheet/mcp",
        Some("slide") => "https://docs.qq.com/api/v6/slide/mcp",
        Some("form") => "https://docs.qq.com/api/v6/form/mcp",
        Some("main") => "https://docs.qq.com/openapi/mcp",
        _ => anyhow::bail!("documents_tool_denied"),
    };
    Ok((endpoint, t["provider_tool"].as_str().expect("catalog tool")))
}

pub(super) fn discover(kind: &str, start: usize) -> Value {
    let tools: Vec<_> = catalog()
        .iter()
        .filter(|t| kind.is_empty() || t["document_type"] == kind)
        .collect();
    let page: Vec<_> = tools
        .iter()
        .skip(start)
        .take(20)
        .map(|t| {
            json!({
                "name": t["name"], "description": t["description"],
                "mutates": t["mutation"], "creates": t["creates"]
            })
        })
        .collect();
    json!({"tools":page,"total":tools.len(),"next_start":
        if start.saturating_add(20)<tools.len(){json!(start+20)}else{Value::Null}})
}

pub(super) fn schema(name: &str) -> Result<Value> {
    let t = tool(name)?;
    Ok(json!({"name":t["name"],"description":t["description"],
        "document_type":t["document_type"],"mutates":t["mutation"],
        "creates":t["creates"],"arguments":t["inputSchema"],
        "usage":"Pass these arguments to action=invoke. The host supplies file_id. Mutations require a unique operation_id; edits require expected_modified_at from inspect. Use actual positions/IDs from fresh reads. Forms remain drafts until manually published."}))
}

pub(super) fn validate(name: &str, args: &Value) -> Result<()> {
    let t = tool(name)?;
    check_schema(&t["inputSchema"], args, 0)?;
    reject_targets(args, 0)?;
    Ok(())
}

fn reject_targets(value: &Value, depth: usize) -> Result<()> {
    ensure!(depth <= 32, "documents_arguments_too_deep");
    match value {
        Value::Object(map) => {
            for (key, v) in map {
                ensure!(
                    !matches!(
                        key.as_str(),
                        "file_id"
                            | "file_url"
                            | "other_file_url"
                            | "parent_id"
                            | "space_id"
                            | "target_folder_id"
                            | "task_id"
                            | "file_key"
                            | "authorization"
                            | "endpoint"
                    ),
                    "documents_target_override_denied"
                );
                reject_targets(v, depth + 1)?;
            }
        }
        Value::Array(items) => {
            for v in items {
                reject_targets(v, depth + 1)?;
            }
        }
        _ => (),
    }
    Ok(())
}

fn check_schema(schema: &Value, value: &Value, depth: usize) -> Result<()> {
    ensure!(depth <= 32, "documents_arguments_too_deep");
    let matches_type = |kind: &str| match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    };
    if let Some(kind) = schema["type"].as_str() {
        ensure!(matches_type(kind), "documents_argument_type_invalid");
    } else if let Some(kinds) = schema["type"].as_array() {
        ensure!(
            kinds.iter().any(|k| k.as_str().is_some_and(matches_type)),
            "documents_argument_type_invalid"
        );
    }
    if let Some(choices) = schema["enum"].as_array() {
        ensure!(choices.contains(value), "documents_argument_enum_invalid");
    }
    if let Some(map) = value.as_object() {
        if let Some(required) = schema["required"].as_array() {
            ensure!(
                required
                    .iter()
                    .all(|k| k.as_str().is_some_and(|k| map.contains_key(k))),
                "documents_argument_required"
            );
        }
        for (key, v) in map {
            if let Some(child) = schema["properties"].get(key) {
                check_schema(child, v, depth + 1)?;
            } else {
                ensure!(
                    schema["additionalProperties"] != false,
                    "documents_argument_unknown"
                );
                if schema["additionalProperties"].is_object() {
                    check_schema(&schema["additionalProperties"], v, depth + 1)?;
                }
            }
        }
    }
    if let Some(items) = value.as_array() {
        for v in items {
            check_schema(&schema["items"], v, depth + 1)?;
        }
    }
    Ok(())
}

pub(super) fn capabilities() -> Value {
    json!({
        "body_types":["smartcanvas","doc","sheet","slide","smartsheet","form","flowchart"],
        "native_tools":catalog().len(),
        "limited_types":{
            "mind":"Create from Markdown; inspect, rename, recycle. Node read/edit API unavailable.",
            "board":"Create blank; inspect, rename, recycle. Canvas editing API unavailable.",
            "pdf":"PDF import is verified. The export API is wired but current PDF fixtures return missing download objects; do not promise PDF export success. No native PDF text editing API."},
        "form_publish":"Manual publication in Tencent Docs required; no publish API.",
        "qq_group_online_list_integration":false,
        "discovery":"tools lists operations (document_type and start); tool returns one input schema; invoke executes it."
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn routes_and_catalog_have_fixed_endpoints_and_unique_names() {
        let mut seen = std::collections::BTreeSet::new();
        for t in catalog() {
            let name = t["name"].as_str().unwrap();
            assert!(seen.insert(name));
            assert!(route(name).unwrap().0.starts_with("https://docs.qq.com/"));
            assert_eq!(
                super::super::remote_route(name).unwrap(),
                route(name).unwrap()
            );
            assert!(t["inputSchema"]["properties"].get("file_id").is_none());
        }
        assert!(route("manage.delete_file").is_err());
        assert!(route("doc.batch_write").is_err());
    }
    #[test]
    fn native_arguments_validate_types_and_nested_target_overrides() {
        assert!(validate("doc.insert_markdown", &json!({"idx":0,"markdown":"hi"})).is_ok());
        for args in [
            json!({"idx":"0"}),
            json!({}),
            json!({"idx":0,"file_id":"other"}),
            json!({"idx":0,"version_info":{"file_id":"other"}}),
        ] {
            assert!(validate("doc.insert_markdown", &args).is_err());
        }
        assert_eq!(discover("flowchart", 0)["total"], 5);
        assert_eq!(discover("flowchart", 20)["tools"], json!([]));
    }
}
