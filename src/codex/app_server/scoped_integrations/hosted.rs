//! Explicitly registered, read-only hosted Apps through a disposable native
//! worker. No model turn, personal profile, refresh token or model-selected
//! app-server method is exposed. Catalog reads metadata, never account data.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, ensure};
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use super::{McpRegistration, mcp};
use crate::codex::app_server::{
    AppServerSupervisor, ClientInfo, IsolatedAppServerConfig,
    client::JsonRpcClient,
    native_images::{WorkerLifetime, authority_auth},
    protocol::JsonRpcError,
};

const ENDPOINT: &str = "https://agent.tinyfish.ai/mcp";
const APP_ENDPOINT: &str = "https://chatgpt.com/";
const TINYFISH_APP_ID: &str = "app-695325bae7348191b58ae9349a963d22";
const TOOLS: &[&str] = &["search", "fetch_content"];
static WORKER_SLOT: Semaphore = Semaphore::const_new(1);

pub(super) fn validate(registration: &McpRegistration) -> Result<()> {
    ensure!(
        registration.codex_app
            && registration.credential_file.is_none()
            && !registration.allowed_tools.is_empty()
            && registration.allowed_tools.len() <= 32
            && registration.allowed_tools.iter().all(|name| {
                if registration.app_id.is_some() && registration.tool_namespace.is_none() {
                    super::valid_tool_name(name)
                } else {
                    valid_tool(name)
                }
            }),
        "hosted_connector_registration_invalid"
    );
    if let Some(app_id) = &registration.app_id {
        ensure!(
            valid_app_id(app_id)
                && registration.personal_account
                && registration.endpoint == APP_ENDPOINT
                && registration
                    .tool_namespace
                    .as_deref()
                    .is_none_or(valid_namespace),
            "hosted_app_registration_invalid"
        );
    } else {
        ensure!(
            registration.endpoint == ENDPOINT
                && !registration.personal_account
                && registration.tool_namespace.is_none()
                && registration
                    .allowed_tools
                    .iter()
                    .all(|name| TOOLS.contains(&name.as_str())),
            "hosted_connector_registration_invalid"
        );
    }
    Ok(())
}

fn valid_app_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 160
        && value != "_default"
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}

fn valid_namespace(value: &str) -> bool {
    value.len() <= 80
        && value.bytes().next().is_some_and(|c| c.is_ascii_lowercase())
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
}

fn valid_tool(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}

/// New catalog registrations retain the exact MCP tool name. Older
/// registrations keep their explicit namespace plus short-name representation.
/// Native callable_namespace is a model-facing label, not a tools/call prefix.
fn native_tool_name(registration: &McpRegistration, name: &str) -> String {
    match (&registration.app_id, registration.tool_namespace.as_deref()) {
        (Some(_), None) => name.to_owned(),
        (_, namespace) => format!("{}.{name}", namespace.unwrap_or("tinyfish")),
    }
}

fn app_config(app_id: Option<&str>) -> Value {
    let mut config = json!({"apps._default.enabled":app_id.is_none(),
        format!("apps.{TINYFISH_APP_ID}.enabled"):app_id.is_none()});
    if let Some(id) = app_id {
        config[format!("apps.{id}.enabled")] = json!(true);
    }
    config
}

/// The enclosing scoped integration owns cancellation and the overall deadline.
/// WorkerLifetime survives every await and kills/reaps before releasing its
/// directory and slot, including when that enclosing future is dropped.
pub(super) async fn run(
    registration: &McpRegistration,
    call: Option<(&str, Value)>,
    authority: Option<&Arc<AppServerSupervisor>>,
) -> Result<Value> {
    validate(registration)?;
    if let Some((name, _)) = &call {
        ensure!(
            registration
                .allowed_tools
                .iter()
                .any(|s| s.as_str() == *name),
            "plugin_tool_not_allowed"
        );
    }
    let authority = authority.ok_or_else(|| anyhow::anyhow!("hosted_authority_unavailable"))?;
    let (mut lifecycle, client) = start_worker(authority).await?;
    let operation = async {
        let thread_id = start_thread(&client, app_config(Some(registration.app_id.as_deref().unwrap_or(TINYFISH_APP_ID)))).await?;
        let status = tool_status(&client, &thread_id).await?;
        let tools = filtered_tools(registration, &status)?;
        if let Some((name, arguments)) = call {
            ensure!(tools.iter().any(|t| t["name"] == name), "mcp_tool_not_available");
            let result: Value = client.request("mcpServer/tool/call",
                &json!({"threadId":thread_id,"server":"codex_apps","tool":native_tool_name(registration, name),"arguments":arguments})).await?;
            mcp::text_result(result)
        } else {
            Ok(json!({"ok":true,"tools":tools,"untrusted_external_content":true}))
        }
    }.await;
    lifecycle.shutdown().await;
    operation
}

async fn start_worker(
    authority: &Arc<AppServerSupervisor>,
) -> Result<(WorkerLifetime, Arc<JsonRpcClient>)> {
    let parent = authority
        .isolation_config()
        .filter(|c| !c.image_worker && !c.codex_app_worker)
        .ok_or_else(|| anyhow::anyhow!("hosted_authority_invalid"))?;
    let permit = WORKER_SLOT.acquire().await?;
    let directory = prepare_directory()?;
    let worker = AppServerSupervisor::new_isolated(
        IsolatedAppServerConfig {
            bubblewrap_binary: parent.bubblewrap_binary,
            codex_binary: parent.codex_binary,
            runtime_root: directory.path().to_owned(),
            provider_environment: Default::default(),
            selected_provider: Some("openai".into()),
            image_worker: false,
            codex_app_worker: true,
        },
        ClientInfo {
            name: "clawmingle-apps-worker".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: None,
        },
    );
    let mut lifecycle = WorkerLifetime::new(worker.clone(), directory, permit);
    let operation = async {
        authority.register_auxiliary_worker(&worker).await?;
        let auth = authority_auth(authority, false).await?;
        worker.start().await?;
        let client = worker.client().await?;
        deny_server_requests(&worker, &client).await?;
        let mut login = auth.external_tokens();
        login["type"] = json!("chatgptAuthTokens");
        let _: Value = client.request("account/login/start", &login).await?;
        Ok(client)
    }
    .await;
    match operation {
        Ok(client) => Ok((lifecycle, client)),
        Err(error) => {
            lifecycle.shutdown().await;
            Err(error)
        }
    }
}

async fn start_thread(client: &JsonRpcClient, config: Value) -> Result<String> {
    let thread: Value = client.request("thread/start", &json!({"ephemeral":true,"cwd":"/workspace","approvalPolicy":"never","sandbox":"read-only","config":config})).await?;
    thread
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("hosted_thread_invalid"))
}

async fn tool_status(client: &JsonRpcClient, thread_id: &str) -> Result<Value> {
    client.request("mcpServerStatus/list", &json!({"threadId":thread_id,"serverName":"codex_apps","detail":"toolsAndAuthOnly","limit":1})).await
}

/// Operator-only inventory. Enabling all apps applies only to this ephemeral
/// no-model worker, solely so native metadata can describe the installed tools.
pub(crate) async fn catalog(authority: &Arc<AppServerSupervisor>) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(90), catalog_inner(authority))
        .await
        .map_err(|_| anyhow::anyhow!("hosted_catalog_timeout"))?
}

async fn catalog_inner(authority: &Arc<AppServerSupervisor>) -> Result<Value> {
    let (mut lifecycle, client) = start_worker(authority).await?;
    let operation = async {
        let thread_id = start_thread(&client, app_config(None)).await?;
        let installed: Value = client
            .request(
                "app/installed",
                &json!({"threadId":thread_id,"forceRefresh":true}),
            )
            .await?;
        let apps = installed
            .get("apps")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("hosted_catalog_invalid"))?;
        ensure!(apps.len() <= 512, "hosted_catalog_limit");
        let ids = apps
            .iter()
            .filter_map(|app| app["id"].as_str())
            .filter(|id| valid_app_id(id))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut metadata = Vec::new();
        for ids in ids.chunks(100) {
            let batch: Value = client
                .request(
                    "app/read",
                    &json!({"threadId":thread_id,"appIds":ids,"includeTools":true}),
                )
                .await?;
            metadata.extend(
                batch
                    .get("apps")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow::anyhow!("hosted_metadata_invalid"))?
                    .iter()
                    .cloned(),
            );
        }
        let status = tool_status(&client, &thread_id).await?;
        catalog_entries(&installed, &metadata, &status)
    }
    .await;
    lifecycle.shutdown().await;
    operation
}

/// This worker never prompts, performs browser OAuth or refreshes credentials.
/// Its next invocation obtains fresh access-only auth from the main authority.
async fn deny_server_requests(
    worker: &Arc<AppServerSupervisor>,
    client: &Arc<JsonRpcClient>,
) -> Result<()> {
    let mut requests = worker
        .take_server_request_receiver()
        .await
        .ok_or_else(|| anyhow::anyhow!("hosted_request_receiver_unavailable"))?;
    let mut shutdown = worker.shutdown_receiver();
    let client = client.clone();
    tokio::spawn(async move {
        loop {
            if *shutdown.borrow() {
                break;
            }
            let request = tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                request = requests.recv() => request,
            };
            let Some(request) = request else { break };
            if request.connection_id == client.connection_id {
                let _ = client
                    .respond_err(
                        request.id,
                        JsonRpcError {
                            code: -32001,
                            message: "hosted_worker_request_denied".into(),
                            data: None,
                        },
                    )
                    .await;
            }
        }
    });
    Ok(())
}

fn status_entries(status: &Value) -> Result<&serde_json::Map<String, Value>> {
    let servers = status
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("hosted_status_invalid"))?;
    ensure!(
        servers.len() == 1 && status.get("nextCursor").is_none_or(Value::is_null),
        "hosted_status_invalid"
    );
    let server = &servers[0];
    ensure!(
        server["name"] == "codex_apps"
            && server["runtimeStatus"] == "connected"
            && server.get("toolsError").is_none_or(Value::is_null),
        "hosted_tools_unavailable"
    );
    let entries = server
        .get("tools")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("hosted_tools_invalid"))?;
    ensure!(entries.len() <= 8192, "hosted_tools_limit");
    Ok(entries)
}

fn read_only(entry: &Value) -> bool {
    entry.pointer("/annotations/readOnlyHint") == Some(&json!(true))
        && entry.pointer("/annotations/destructiveHint") != Some(&json!(true))
        && entry.get("inputSchema").is_some_and(Value::is_object)
}

fn filtered_tools(registration: &McpRegistration, status: &Value) -> Result<Vec<Value>> {
    validate(registration)?;
    let entries = status_entries(status)?;
    let mut tools = Vec::new();
    for name in &registration.allowed_tools {
        let qualified = native_tool_name(registration, name);
        let entry = entries
            .get(&qualified)
            .ok_or_else(|| anyhow::anyhow!("mcp_tool_not_available"))?;
        if let Some(app_id) = &registration.app_id {
            ensure!(
                entry.pointer("/_meta/connector_id").and_then(Value::as_str)
                    == Some(app_id.as_str()),
                "hosted_tool_app_mismatch"
            );
            ensure!(
                entry.get("name").and_then(Value::as_str) == Some(qualified.as_str()),
                "hosted_tool_name_mismatch"
            );
        }
        ensure!(
            entry.pointer("/annotations/readOnlyHint") == Some(&json!(true))
                && entry.pointer("/annotations/destructiveHint") != Some(&json!(true)),
            "mcp_write_tool_denied"
        );
        ensure!(
            entry.get("inputSchema").is_some_and(Value::is_object),
            "mcp_schema_missing"
        );
        tools.push(json!({"name":name,"description":entry.get("description").and_then(Value::as_str).unwrap_or("").chars().take(1600).collect::<String>(),"inputSchema":entry["inputSchema"]}));
    }
    ensure!(
        json!({"ok":true,"tools":tools,"untrusted_external_content":true})
            .to_string()
            .len()
            <= 48 * 1024,
        "mcp_schema_budget"
    );
    Ok(tools)
}

fn catalog_entries(installed: &Value, metadata: &[Value], status: &Value) -> Result<Value> {
    let entries = status_entries(status)?;
    let installed = installed
        .get("apps")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("hosted_catalog_invalid"))?;
    let metadata = metadata
        .iter()
        .filter_map(|app| app["id"].as_str().map(|id| (id, app)))
        .collect::<BTreeMap<_, _>>();
    let mut apps = Vec::new();
    let mut unavailable = Vec::new();
    for installed in installed {
        let Some(id) = installed["id"].as_str().filter(|id| valid_app_id(id)) else {
            continue;
        };
        let display = metadata.get(id);
        let name = display
            .and_then(|app| app["name"].as_str())
            .unwrap_or(id)
            .chars()
            .take(160)
            .collect::<String>();
        let mut tools = Vec::new();
        for (raw_name, entry) in entries {
            // Native status keys and tools/call both use the original MCP name;
            // callable_namespace and display names are not routing authority.
            if entry.pointer("/_meta/connector_id").and_then(Value::as_str) != Some(id)
                || !read_only(entry)
                || !super::valid_tool_name(raw_name)
                || entry.get("name").and_then(Value::as_str) != Some(raw_name.as_str())
            {
                continue;
            }
            tools.push(json!({"name":raw_name,"description":entry.get("description").and_then(Value::as_str).unwrap_or("").chars().take(1600).collect::<String>()}));
        }
        let reason = if display.is_none() {
            Some("metadata_unavailable")
        } else if installed["enabled"] != true || installed["callable"] != true {
            Some("app_unavailable")
        } else if tools.is_empty() {
            Some("read_only_namespace_unavailable")
        } else {
            None
        };
        if let Some(reason) = reason {
            unavailable.push(json!({"id":id,"name":name,"reason":reason}));
            continue;
        }
        apps.push(json!({"id":id,"name":name,"description":display.and_then(|app| app["description"].as_str()).unwrap_or("").chars().take(1600).collect::<String>(),"tool_namespace":null,"tools":tools}));
    }
    let result = json!({"apps":apps,"unavailable":unavailable});
    ensure!(
        result.to_string().len() <= 512 * 1024,
        "hosted_catalog_budget"
    );
    Ok(result)
}

fn prepare_directory() -> Result<tempfile::TempDir> {
    let directory = tempfile::Builder::new()
        .prefix("clawmingle-apps-")
        .tempdir()?;
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    for name in ["codex", "sqlite", "workspace"] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(directory.path().join(name))?;
    }
    let mut config = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.path().join("codex/config.toml"))?;
    config.write_all(b"model_provider = \"openai\"\nweb_search = \"disabled\"\n")?;
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registration() -> McpRegistration {
        McpRegistration {
            enabled: true,
            name: "TinyFish".into(),
            endpoint: ENDPOINT.into(),
            codex_app: true,
            allowed_tools: vec!["search".into(), "fetch_content".into()],
            credential_file: None,
            app_id: None,
            tool_namespace: None,
            personal_account: false,
        }
    }

    fn status() -> Value {
        let read = json!({"inputSchema":{"type":"object"},"description":"Public web evidence","annotations":{"readOnlyHint":true,"destructiveHint":false}});
        json!({"data":[{"name":"codex_apps","runtimeStatus":"connected","toolsError":null,"tools":{"tinyfish.search":read,"tinyfish.fetch_content":read,"tinyfish.run_web_automation":{"inputSchema":{"type":"object"}},"other.private_data":{"inputSchema":{"type":"object"}}}}],"nextCursor":null})
    }

    fn personal_registration() -> McpRegistration {
        McpRegistration {
            app_id: Some("app-fixture".into()),
            tool_namespace: Some("fixture".into()),
            personal_account: true,
            endpoint: APP_ENDPOINT.into(),
            name: "Fixture app".into(),
            allowed_tools: vec!["read".into()],
            ..registration()
        }
    }

    fn personal_status() -> Value {
        let mut status = status();
        status["data"][0]["tools"]["fixture.read"] = json!({
            "name":"fixture.read","inputSchema":{"type":"object","properties":{"query":{"type":"string"}}},
            "description":"Read fixture records","annotations":{"readOnlyHint":true,"destructiveHint":false},
            "_meta":{"connector_id":"app-fixture"}});
        status["data"][0]["tools"]["fixture.write"] = json!({
            "name":"fixture.write","inputSchema":{"type":"object"},
            "annotations":{"readOnlyHint":false,"destructiveHint":true},"_meta":{"connector_id":"app-fixture"}});
        status
    }

    #[test]
    fn personal_app_requires_explicit_marker_and_safe_exact_routing() {
        let registration = personal_registration();
        validate(&registration).unwrap();
        let mut invalid = registration.clone();
        invalid.personal_account = false;
        assert!(validate(&invalid).is_err());
        for id in ["_default", "app.other", "app[unsafe]", ""] {
            let mut invalid = registration.clone();
            invalid.app_id = Some(id.into());
            assert!(validate(&invalid).is_err());
        }
        for namespace in ["Other", "nested.tools", "", "app-name"] {
            let mut invalid = registration.clone();
            invalid.tool_namespace = Some(namespace.into());
            assert!(validate(&invalid).is_err());
        }
        let mut invalid = registration;
        invalid.allowed_tools = vec!["fixture.read".into()];
        assert!(validate(&invalid).is_err());
    }

    #[test]
    fn selected_apps_never_enable_other_apps_or_inherit_tinyfish() {
        let selected = app_config(Some("app-fixture"));
        assert_eq!(selected["apps._default.enabled"], false);
        assert_eq!(selected[format!("apps.{TINYFISH_APP_ID}.enabled")], false);
        assert_eq!(selected["apps.app-fixture.enabled"], true);
        assert_eq!(app_config(None)["apps._default.enabled"], true);
        assert_eq!(
            app_config(Some(TINYFISH_APP_ID))[format!("apps.{TINYFISH_APP_ID}.enabled")],
            true
        );
    }

    #[test]
    fn personal_tool_identity_and_native_read_only_metadata_are_required() {
        let registration = personal_registration();
        let mut status = personal_status();
        let tools = filtered_tools(&registration, &status).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "read");
        assert!(tools[0]["inputSchema"]["properties"]["query"].is_object());
        status["data"][0]["tools"]["fixture.read"]["_meta"]["connector_id"] = json!("app-other");
        assert!(filtered_tools(&registration, &status).is_err());
        let mut registration = registration;
        registration.allowed_tools = vec!["write".into()];
        assert!(filtered_tools(&registration, &personal_status()).is_err());
    }

    #[test]
    fn installed_catalog_uses_identity_metadata_instead_of_display_names() {
        let installed = json!({"apps":[{"id":"app-fixture","runtimeName":"misleading_name","enabled":true,"callable":true}]});
        let metadata = vec![
            json!({"id":"app-fixture","name":"Another display name","description":"Fixture","toolSummaries":[{"name":"fake.read","description":"not authoritative","isReadOnly":true}]}),
        ];
        let catalog = catalog_entries(&installed, &metadata, &personal_status()).unwrap();
        assert!(catalog["apps"][0]["tool_namespace"].is_null());
        assert_eq!(catalog["apps"][0]["tools"].as_array().unwrap().len(), 1);
        assert_eq!(catalog["apps"][0]["tools"][0]["name"], "fixture.read");
        assert!(catalog["unavailable"].as_array().unwrap().is_empty());
    }

    #[test]
    fn raw_app_catalog_preserves_bare_and_dotted_names_without_guessing_a_namespace() {
        let installed = json!({"apps":[{"id":"app-fixture","enabled":true,"callable":true}]});
        let metadata = vec![json!({"id":"app-fixture","name":"Unrelated display name"})];
        let mut status = personal_status();
        let mut bare = status["data"][0]["tools"]["fixture.read"].clone();
        bare["name"] = json!("capture_file_upload");
        bare["_meta"]["connector_name"] = json!("Different Display Name");
        status["data"][0]["tools"]["capture_file_upload"] = bare.clone();
        bare["name"] = json!("other.namespace.read");
        status["data"][0]["tools"]["other.namespace.read"] = bare;
        let catalog = catalog_entries(&installed, &metadata, &status).unwrap();
        assert!(catalog["apps"][0]["tool_namespace"].is_null());
        assert!(catalog["unavailable"].as_array().unwrap().is_empty());
        let names = catalog["apps"][0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "capture_file_upload",
                "fixture.read",
                "other.namespace.read"
            ]
        );
        let registration = McpRegistration {
            tool_namespace: None,
            allowed_tools: names,
            ..personal_registration()
        };
        validate(&registration).unwrap();
        let tools = filtered_tools(&registration, &status).unwrap();
        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            assert_eq!(native_tool_name(&registration, name), name);
        }
        assert_eq!(
            native_tool_name(&personal_registration(), "read"),
            "fixture.read"
        );
        assert_eq!(
            native_tool_name(&self::registration(), "search"),
            "tinyfish.search"
        );
    }

    #[test]
    fn raw_app_routing_rechecks_exact_identity_and_name_before_calling() {
        let registration = McpRegistration {
            tool_namespace: None,
            allowed_tools: vec!["fixture.read".into()],
            ..personal_registration()
        };
        assert_eq!(
            filtered_tools(&registration, &personal_status()).unwrap()[0]["name"],
            "fixture.read"
        );
        let installed = json!({"apps":[{"id":"app-fixture","enabled":true,"callable":true}]});
        let metadata = vec![json!({"id":"app-fixture","name":"Fixture"})];
        for replacement in [json!("other.read"), Value::Null] {
            let mut status = personal_status();
            status["data"][0]["tools"]["fixture.read"]["name"] = replacement;
            assert!(filtered_tools(&registration, &status).is_err());
            assert!(
                catalog_entries(&installed, &metadata, &status).unwrap()["apps"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        let mut status = personal_status();
        status["data"][0]["tools"]["fixture.read"]["_meta"]["connector_id"] = json!("app-other");
        assert!(filtered_tools(&registration, &status).is_err());
        for name in ["", "tool with spaces", "tool/path", "tool\nname"] {
            let mut invalid = registration.clone();
            invalid.allowed_tools = vec![name.into()];
            assert!(validate(&invalid).is_err());
        }
        let mut invalid = registration;
        invalid.allowed_tools = vec!["x".repeat(129)];
        assert!(validate(&invalid).is_err());
    }

    #[test]
    fn registration_is_exact_and_rejects_credentials_and_other_tools() {
        let mut r = registration();
        validate(&r).unwrap();
        r.endpoint = "https://example.com/mcp".into();
        assert!(validate(&r).is_err());
        r = registration();
        r.allowed_tools.push("run_web_automation".into());
        assert!(validate(&r).is_err());
        r = registration();
        r.credential_file = Some("/private/token".into());
        assert!(validate(&r).is_err());
    }

    #[test]
    fn catalog_exposes_only_unprefixed_allowed_read_tools() {
        let mut r = registration();
        r.allowed_tools = vec!["search".into()];
        let tools = filtered_tools(&r, &status()).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "search");
        let output = serde_json::to_string(&tools).unwrap();
        assert!(!output.contains("other.private_data"));
        assert!(!output.contains("run_web_automation"));
        assert!(!output.contains("fetch_content"));
    }

    #[test]
    fn changed_tool_metadata_missing_schema_and_oversize_catalog_fail_closed() {
        let r = registration();
        let mut s = status();
        s["data"][0]["tools"]["tinyfish.search"]["annotations"]["readOnlyHint"] = json!(false);
        assert!(filtered_tools(&r, &s).is_err());
        let mut s = status();
        s["data"][0]["tools"]["tinyfish.search"]["annotations"]["destructiveHint"] = json!(true);
        assert!(filtered_tools(&r, &s).is_err());
        let mut s = status();
        s["data"][0]["tools"]["tinyfish.search"]
            .as_object_mut()
            .unwrap()
            .remove("inputSchema");
        assert!(filtered_tools(&r, &s).is_err());
        let mut s = status();
        s["data"][0]["tools"]["tinyfish.search"]["inputSchema"]["description"] =
            json!("x".repeat(50 * 1024));
        assert!(filtered_tools(&r, &s).is_err());
    }

    #[test]
    fn fresh_worker_contains_no_auth_history_or_workspace_inputs() {
        let directory = prepare_directory().unwrap();
        assert!(!directory.path().join("codex/auth.json").exists());
        assert_eq!(
            std::fs::read_dir(directory.path().join("workspace"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            std::fs::read_dir(directory.path().join("sqlite"))
                .unwrap()
                .count(),
            0
        );
    }
}
