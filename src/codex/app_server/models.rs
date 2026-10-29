//! Bounded discovery of picker-visible model metadata. No thread or turn is
//! created. Wire fields not needed by the model picker are never returned.

use std::{collections::BTreeSet, future::Future, sync::Arc, time::Duration};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

use super::client::JsonRpcClient;

const PAGE_SIZE: u32 = 100;
const MAX_PAGES: usize = 10;
const PAGE_TIMEOUT: Duration = Duration::from_secs(10);
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AvailableModel {
    pub id: String,
    pub model: String,
    pub display_name: String,
    pub description: String,
    /// Preserve the server's advertised values and order; efforts are an open
    /// non-empty string type in the installed app-server protocol schema.
    pub reasoning_efforts: Vec<String>,
    pub default_effort: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<String>,
    limit: u32,
    include_hidden: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelListPage {
    data: Vec<WireModel>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireModel {
    id: String,
    model: String,
    display_name: String,
    description: String,
    hidden: bool,
    default_reasoning_effort: String,
    supported_reasoning_efforts: Vec<WireEffort>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireEffort {
    reasoning_effort: String,
}

impl TryFrom<WireModel> for AvailableModel {
    type Error = anyhow::Error;

    fn try_from(wire: WireModel) -> Result<Self> {
        ensure!(
            valid_token(&wire.id, 256)
                && valid_token(&wire.model, 256)
                && !wire.display_name.is_empty()
                && wire.display_name.len() <= 1024
                && !wire.display_name.chars().any(char::is_control)
                && wire.description.len() <= 16_384
                && !wire.description.contains('\0')
                && valid_token(&wire.default_reasoning_effort, 128)
                && wire.supported_reasoning_efforts.len() <= 32,
            "model_catalog_metadata_invalid"
        );
        let mut reasoning_efforts = Vec::new();
        for effort in wire.supported_reasoning_efforts {
            ensure!(
                valid_token(&effort.reasoning_effort, 128),
                "model_catalog_effort_invalid"
            );
            if !reasoning_efforts.contains(&effort.reasoning_effort) {
                reasoning_efforts.push(effort.reasoning_effort);
            }
        }
        Ok(Self {
            id: wire.id,
            model: wire.model,
            display_name: wire.display_name,
            description: wire.description,
            reasoning_efforts,
            default_effort: wire.default_reasoning_effort,
        })
    }
}

fn valid_token(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && !value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
}

pub(super) async fn available_models(client: Arc<JsonRpcClient>) -> Result<Vec<AvailableModel>> {
    timeout(
        QUERY_TIMEOUT,
        collect_pages(move |params| {
            let client = client.clone();
            async move {
                timeout(
                    PAGE_TIMEOUT,
                    client.request::<_, ModelListPage>("model/list", &params),
                )
                .await
                .map_err(|_| anyhow::anyhow!("model_list_page_timeout"))?
                .map_err(|_| anyhow::anyhow!("model_list_request_failed"))
            }
        }),
    )
    .await
    .map_err(|_| anyhow::anyhow!("model_list_query_timeout"))?
}

async fn collect_pages<F, Query>(mut fetch: F) -> Result<Vec<AvailableModel>>
where
    F: FnMut(ModelListParams) -> Query,
    Query: Future<Output = Result<ModelListPage>>,
{
    let mut cursor = None;
    let mut seen_cursors = BTreeSet::new();
    let mut seen_models = BTreeSet::new();
    let mut models = Vec::new();
    for _ in 0..MAX_PAGES {
        let page = fetch(ModelListParams {
            cursor,
            limit: PAGE_SIZE,
            include_hidden: false,
        })
        .await?;
        ensure!(
            page.data.len() <= PAGE_SIZE as usize,
            "model_list_page_too_large"
        );
        for wire in page.data {
            // Respect the picker filter even if a server unexpectedly includes
            // hidden entries despite includeHidden=false.
            if wire.hidden {
                continue;
            }
            let model = AvailableModel::try_from(wire)?;
            ensure!(
                seen_models.insert(model.id.clone()),
                "model_list_duplicate_id"
            );
            models.push(model);
        }
        match page.next_cursor {
            None => return Ok(models),
            Some(next) => {
                ensure!(
                    !next.is_empty() && next.len() <= 4096,
                    "model_list_cursor_invalid"
                );
                ensure!(
                    seen_cursors.insert(next.clone()),
                    "model_list_cursor_repeated"
                );
                cursor = Some(next);
            }
        }
    }
    anyhow::bail!("model_list_page_limit_reached")
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque, future::ready};

    use super::*;
    use serde_json::json;

    fn wire_model(id: &str) -> serde_json::Value {
        json!({
            "id": id, "model": id, "displayName": "Example model", "description": "Picker description",
            "hidden": false, "isDefault": false, "defaultReasoningEffort": "future_effort",
            "supportedReasoningEfforts": [
                {"reasoningEffort": "none", "description": "Fast"},
                {"reasoningEffort": "future_effort", "description": "An advertised future option"}
            ],
            "unconsumed": {"private_data": "private-marker"}
        })
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn facade_discovers_pages_without_issuing_threads_or_turns() {
        use super::super::{AppServerHandle, ClientInfo};
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("fake-model-catalog");
        std::fs::write(&binary, r#"#!/usr/bin/python3
import json,sys
for line in sys.stdin:
    message=json.loads(line)
    method=message.get('method')
    if method=='initialize':
        result={}
    elif method=='initialized':
        continue
    elif method=='model/list':
        params=message['params']
        assert params.get('limit')==100 and params.get('includeHidden') is False
        cursor=params.get('cursor')
        assert cursor in (None,'second-page')
        name='first-model' if cursor is None else 'second-model'
        result={'data':[{'id':name,'model':name,'displayName':name,'description':'Catalog fixture','hidden':False,'defaultReasoningEffort':'future_effort','supportedReasoningEfforts':[{'reasoningEffort':'future_effort'}]}],'nextCursor':'second-page' if cursor is None else None}
    else:
        raise AssertionError('discovery created a non-catalog request')
    print(json.dumps({'id':message['id'],'result':result}),flush=True)
"#).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let handle = AppServerHandle::start(
            binary,
            directory.path().to_owned(),
            directory.path().join("sqlite"),
            None,
            ClientInfo {
                name: "catalog-fixture".into(),
                version: "test".into(),
                title: None,
            },
        )
        .await
        .unwrap();
        let result = handle.available_models().await;
        handle.shutdown().await;
        assert_eq!(
            result
                .unwrap()
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["first-model", "second-model"]
        );
    }

    #[test]
    fn translation_uses_advertised_efforts_and_exports_only_picker_fields() {
        let mut wire = wire_model("example-model");
        wire["supportedReasoningEfforts"]
            .as_array_mut()
            .unwrap()
            .push(json!({"reasoningEffort":"none"}));
        let model =
            AvailableModel::try_from(serde_json::from_value::<WireModel>(wire).unwrap()).unwrap();
        assert_eq!(model.reasoning_efforts, ["none", "future_effort"]);
        assert_eq!(model.default_effort, "future_effort");
        let output = serde_json::to_value(&model).unwrap();
        assert_eq!(output.as_object().unwrap().len(), 6);
        assert_eq!(output["display_name"], "Example model");
        assert!(!output.to_string().contains("private-marker"));
    }

    #[tokio::test]
    async fn pagination_sends_explicit_limits_and_aggregates_only_visible_models() {
        let mut hidden = wire_model("hidden");
        hidden["hidden"] = true.into();
        let pages = RefCell::new(VecDeque::from([
            json!({"data":[wire_model("first"), hidden],"nextCursor":"cursor-two"}),
            json!({"data":[wire_model("second")],"nextCursor":null}),
        ]));
        let calls = RefCell::new(Vec::new());
        let models = collect_pages(|params| {
            calls
                .borrow_mut()
                .push(serde_json::to_value(params).unwrap());
            ready(Ok(serde_json::from_value(
                pages.borrow_mut().pop_front().unwrap(),
            )
            .unwrap()))
        })
        .await
        .unwrap();
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(
            calls.borrow().as_slice(),
            [
                json!({"limit":100,"includeHidden":false}),
                json!({"limit":100,"includeHidden":false,"cursor":"cursor-two"}),
            ]
        );
    }

    #[tokio::test]
    async fn repeated_cursor_and_more_than_ten_pages_fail_without_partial_catalog() {
        let count = RefCell::new(0);
        let error = collect_pages(|_| {
            *count.borrow_mut() += 1;
            ready(Ok(ModelListPage {
                data: vec![],
                next_cursor: Some("same-cursor".into()),
            }))
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "model_list_cursor_repeated");
        assert_eq!(*count.borrow(), 2);
        *count.borrow_mut() = 0;
        let error = collect_pages(|_| {
            *count.borrow_mut() += 1;
            ready(Ok(ModelListPage {
                data: vec![],
                next_cursor: Some(format!("cursor-{}", *count.borrow())),
            }))
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "model_list_page_limit_reached");
        assert_eq!(*count.borrow(), 10);
    }

    #[tokio::test]
    async fn duplicate_model_ids_and_oversized_pages_are_rejected() {
        let error = collect_pages(|_| {
            ready(Ok(ModelListPage {
                data: vec![
                    serde_json::from_value(wire_model("same")).unwrap(),
                    serde_json::from_value(wire_model("same")).unwrap(),
                ],
                next_cursor: None,
            }))
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "model_list_duplicate_id");
        let error = collect_pages(|_| {
            ready(Ok(ModelListPage {
                data: (0..101)
                    .map(|index| {
                        serde_json::from_value(wire_model(&format!("model-{index}"))).unwrap()
                    })
                    .collect(),
                next_cursor: None,
            }))
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "model_list_page_too_large");
    }

    #[test]
    fn malformed_metadata_and_effort_values_fail_without_echoing_values() {
        for (field, value) in [
            ("id", json!("private marker")),
            ("defaultReasoningEffort", json!("")),
            ("displayName", json!("name\nprivate-marker")),
        ] {
            let mut wire = wire_model("example-model");
            wire[field] = value;
            let error =
                AvailableModel::try_from(serde_json::from_value::<WireModel>(wire).unwrap())
                    .unwrap_err();
            assert!(!error.to_string().contains("private-marker"));
        }
        let mut wire = wire_model("example-model");
        wire["supportedReasoningEfforts"][0]["reasoningEffort"] = "".into();
        assert!(
            AvailableModel::try_from(serde_json::from_value::<WireModel>(wire).unwrap()).is_err()
        );
    }
}
