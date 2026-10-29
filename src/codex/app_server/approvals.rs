//! Routes server-initiated approval / elicitation requests.
//!
//! This module listens for server → client requests (`commandExecution/request
//! Approval`, `fileChange/requestApproval`, `permissions/requestApproval`,
//! `mcpServer/elicitation/request`, `account/chatgptAuthTokens/refresh`) and
//! either responds automatically (when no interactive handler is installed)
//! or delegates to a channel-based handler (QQ prompt flow).

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value as JsonValue;
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, info, warn};
static SOCIAL_TOOL_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

use super::{
    client::{JsonRpcClient, ServerRequest},
    protocol::{
        ApprovalDecision, ApprovalDecisionResponse, CommandApprovalParams, ElicitationResponse,
        FileChangeApprovalParams, JsonRpcError, McpElicitationParams, PermissionsApprovalParams,
        SimpleDecision, method,
    },
    supervisor::AppServerSupervisor,
};

/// The decision a handler can return for a command / file-change / permissions
/// approval request.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ApprovalOutcome {
    Accept,
    AcceptForSession,
    Decline,
    Cancel,
}

impl ApprovalOutcome {
    fn to_decision(self) -> ApprovalDecision {
        match self {
            ApprovalOutcome::Accept => ApprovalDecision::Simple(SimpleDecision::Accept),
            ApprovalOutcome::AcceptForSession => {
                ApprovalDecision::Simple(SimpleDecision::AcceptForSession)
            }
            ApprovalOutcome::Decline => ApprovalDecision::Simple(SimpleDecision::Decline),
            ApprovalOutcome::Cancel => ApprovalDecision::Simple(SimpleDecision::Cancel),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CommandApprovalEvent {
    pub(crate) command: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct FileChangeApprovalEvent {
    pub(crate) reason: Option<String>,
    pub(crate) grant_root: Option<String>,
    pub(crate) file_changes: JsonValue,
}

#[derive(Debug, Clone)]
pub(crate) struct PermissionsApprovalEvent {
    pub(crate) reason: Option<String>,
    pub(crate) permissions: JsonValue,
}

#[derive(Debug, Clone)]
pub(crate) struct ElicitationEvent {
    pub(crate) thread_id: String,
    pub(crate) server: Option<String>,
}

/// Every approval request yields one of these envelopes — the handler fills
/// the oneshot to report its decision. If the handler drops the envelope
/// without responding, the broker replies `Decline` so the turn can progress.
pub(crate) enum ApprovalRequest {
    Command {
        event: CommandApprovalEvent,
        reply: oneshot::Sender<ApprovalOutcome>,
    },
    FileChange {
        event: FileChangeApprovalEvent,
        reply: oneshot::Sender<ApprovalOutcome>,
    },
    Permissions {
        event: PermissionsApprovalEvent,
        reply: oneshot::Sender<ApprovalOutcome>,
    },
    Elicitation {
        event: ElicitationEvent,
        /// `Some(content)` accepts, `None` declines.
        reply: oneshot::Sender<Option<JsonValue>>,
    },
}

pub(crate) struct ApprovalBroker {
    supervisor: Arc<AppServerSupervisor>,
    handler_tx: Mutex<Option<mpsc::Sender<ApprovalRequest>>>,
}

impl ApprovalBroker {
    pub(crate) fn new(supervisor: Arc<AppServerSupervisor>) -> Arc<Self> {
        Arc::new(Self {
            supervisor,
            handler_tx: Mutex::new(None),
        })
    }

    /// Install an interactive handler. Any request delivered while no handler
    /// is installed (or while the handler's channel is full/closed) is
    /// auto-declined.
    pub(crate) async fn install_handler(&self, tx: mpsc::Sender<ApprovalRequest>) {
        *self.handler_tx.lock().await = Some(tx);
    }

    /// Launch the background dispatcher. Must be called exactly once after
    /// the supervisor has started.
    pub(crate) async fn start(self: &Arc<Self>) -> Result<()> {
        let mut rx = self
            .supervisor
            .take_server_request_receiver()
            .await
            .ok_or_else(|| anyhow::anyhow!("server request receiver already taken"))?;
        let this = self.clone();
        tokio::spawn(async move {
            let mut shutdown = this.supervisor.shutdown_receiver();
            loop {
                if *shutdown.borrow() {
                    break;
                }
                let request = tokio::select! {
                    biased;
                    _ = shutdown.changed() => break,
                    request = rx.recv() => request,
                };
                let Some(request) = request else {
                    break;
                };
                let this = this.clone();
                tokio::spawn(async move {
                    let mut shutdown = this.supervisor.shutdown_receiver();
                    if *shutdown.borrow() {
                        return;
                    }
                    tokio::select! {
                        biased;
                        _ = shutdown.changed() => {},
                        _ = this.dispatch(request) => {},
                    }
                });
            }
            debug!("approval broker dispatcher ended");
        });
        Ok(())
    }

    async fn dispatch(&self, req: ServerRequest) {
        let client = match self.supervisor.client().await {
            Ok(c) => c,
            Err(err) => {
                warn!(error = %err, method = %req.method, "no client for approval response");
                return;
            }
        };
        if req.connection_id != client.connection_id {
            return;
        }
        match req.method.as_str() {
            "item/tool/call" if self.supervisor.is_isolated() => {
                self.dispatch_social_tool(&client, req).await;
            }
            method::COMMAND_EXECUTION_REQUEST_APPROVAL | method::EXEC_COMMAND_APPROVAL => {
                self.dispatch_command(&client, req).await;
            }
            method::FILE_CHANGE_REQUEST_APPROVAL | method::APPLY_PATCH_APPROVAL => {
                self.dispatch_file_change(&client, req).await;
            }
            method::PERMISSIONS_REQUEST_APPROVAL => {
                self.dispatch_permissions(&client, req).await;
            }
            method::MCP_SERVER_ELICITATION_REQUEST => {
                self.dispatch_elicitation(&client, req).await;
            }
            method::CHATGPT_AUTH_TOKENS_REFRESH => {
                if self
                    .supervisor
                    .isolation_config()
                    .is_some_and(|config| config.image_worker)
                {
                    let auth = super::native_images::refresh_worker_auth(&self.supervisor).await;
                    match auth {
                        Ok(auth) => {
                            let _ = client.respond_ok(req.id, &auth.external_tokens()).await;
                        }
                        Err(_) => {
                            let _ = client
                                .respond_err(
                                    req.id,
                                    JsonRpcError {
                                        code: -32001,
                                        message: "image_auth_refresh_failed".into(),
                                        data: None,
                                    },
                                )
                                .await;
                        }
                    }
                    return;
                }
                let err = JsonRpcError {
                    code: -32001,
                    message: "Token refresh not supported by codex-claw".to_string(),
                    data: None,
                };
                let _ = client.respond_err(req.id, err).await;
                info!("declined chatgpt auth token refresh; user must re-login on host");
            }
            other => {
                warn!(method = %other, "unknown server-initiated request; declining");
                let err = JsonRpcError {
                    code: -32601,
                    message: format!("method not supported: {other}"),
                    data: None,
                };
                let _ = client.respond_err(req.id, err).await;
            }
        }
    }

    async fn dispatch_social_tool(&self, client: &Arc<JsonRpcClient>, req: ServerRequest) {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Call {
            thread_id: String,
            turn_id: String,
            call_id: String,
            tool: String,
            namespace: Option<String>,
            arguments: JsonValue,
        }
        let Ok(call) = serde_json::from_value::<Call>(req.params) else {
            let _ = client
                .respond_err(
                    req.id,
                    JsonRpcError {
                        code: -32602,
                        message: "Invalid tool call".into(),
                        data: None,
                    },
                )
                .await;
            return;
        };
        // turn/start can emit a request before its response reaches the session.
        // Wait for the session's exact binding; stale turns never bind themselves.
        for _ in 0..50 {
            self.supervisor.social_tools.bind_child_request(
                client.connection_id,
                &call.thread_id,
                &call.turn_id,
            );
            if self
                .supervisor
                .social_tools
                .turn_ready(&call.thread_id, &call.turn_id)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let mut sticker_preview = None;
        let mut timing_query = None;
        let mut output_workspace = None;
        let mut result = if let Some(lease) = self.supervisor.social_tools.authorize(
            &call.thread_id,
            &call.turn_id,
            call.namespace.as_deref(),
            &call.tool,
            &call.call_id,
        ) {
            output_workspace = lease
                .can_read_workspace
                .then(|| lease.workspace.as_ref().map(|w| w.directory.clone()))
                .flatten();
            if call.tool != "task_update" {
                timing_query = lease.interaction.clone();
            }
            if let Ok(_slot) = SOCIAL_TOOL_SLOTS.try_acquire() {
                let arguments = call.arguments.to_string();
                if matches!(
                    call.tool.as_str(),
                    "message_lookup" | "messages_list" | "tasks_manage" | "memory_manage"
                ) {
                    let reader = lease.messages.expect("authorized message reader");
                    let name = call.tool.clone();
                    let mut query = tokio::task::spawn_blocking(move || reader(&name, &arguments));
                    let mut cancelled = lease.cancelled;
                    tokio::select! {
                        biased;
                        _=cancelled.changed()=>serde_json::json!({"ok":false,"error":{"code":"cancelled","message":"消息读取已取消。"}}),
                        result=&mut query=>result.unwrap_or_else(|_|serde_json::json!({"ok":false,"error":{"code":"message_read_failed","message":"未能读取消息存档。"}})),
                    }
                } else if matches!(call.tool.as_str(), "sticker_lookup" | "sticker_manage") {
                    if !crate::social::sticker_tool_action_allowed(&call.tool, &arguments) {
                        serde_json::json!({"ok":false,"error":"sticker_action_not_allowed"})
                    } else {
                        let query = lease.stickers.expect("authorized sticker reader");
                        let mut cancelled = lease.cancelled;
                        let mut job = tokio::task::spawn_blocking(move || query(&arguments));
                        tokio::select! {biased;
                            _=cancelled.changed()=>serde_json::json!({"ok":false,"error":"cancelled"}),
                            result=&mut job=>match result {Ok(result)=>{sticker_preview=result.preview_png;result.data},Err(_)=>serde_json::json!({"ok":false,"error":"sticker_lookup_failed"})}
                        }
                    }
                } else if matches!(
                    call.tool.as_str(),
                    "qq_info"
                        | "task_update"
                        | "group_documents"
                        | "identity_manage"
                        | "group_profile_manage"
                ) {
                    let query = if call.tool == "group_documents" {
                        lease.documents.expect("authorized group documents")
                    } else if call.tool == "group_profile_manage" {
                        lease
                            .group_profile
                            .expect("authorized group profile editor")
                    } else if call.tool == "identity_manage" {
                        lease.identity.expect("authorized identity editor")
                    } else if call.tool == "task_update" {
                        lease.interaction.expect("authorized interaction")
                    } else {
                        lease.qq_info.expect("authorized QQ reader")
                    };
                    let mut cancelled = lease.cancelled;
                    if *cancelled.borrow() {
                        serde_json::json!({"ok":false,"error":{"code":"cancelled","message":"工具尚未执行，当前请求已取消。"}})
                    } else {
                        let cancelled_result = if call.tool == "group_documents" {
                            serde_json::json!({"ok":false,"status":"unknown","message":"文档操作已中断，可能有远端副作用。用 group_documents action=operation 和原 operation_id 查询记录，勿盲目重试。"})
                        } else {
                            serde_json::json!({"ok":false,"error":{"code":"cancelled","message":"当前操作已取消，请检查交付状态。"}})
                        };
                        tokio::select! {biased;_=cancelled.changed()=>cancelled_result,result=query(arguments)=>result}
                    }
                } else if super::scoped_integrations::TOOL_NAMES.contains(&call.tool.as_str()) {
                    let scope = lease.integrations.expect("authorized integration scope");
                    scope.call(&call.tool, &arguments, lease.cancelled).await
                } else if call.tool == "workspace_exec" {
                    if let Some(workspace) = lease.workspace {
                        super::workspace_exec::execute_with_skills(
                            &workspace.directory,
                            &workspace.bubblewrap,
                            &arguments,
                            Some(lease.cancelled),
                            &lease.skills,
                            lease.workspace_timeout_seconds,
                        )
                        .await
                    } else {
                        serde_json::json!({"ok":false,"error":{"code":"workspace_not_configured","message":"本对话尚未配置独立工作区。"}})
                    }
                } else {
                    super::social_tools::call_tool(&call.tool, &arguments).await
                }
            } else {
                serde_json::json!({"ok":false,"error":{"code":"tool_busy","message":"工具正在处理其他请求，请稍后重试。"}})
            }
        } else {
            serde_json::json!({"ok":false,"error":{"code":"tool_not_allowed","message":"This tool is disabled, the call is duplicated, the turn has ended, or its budget is exhausted."}})
        };
        if let Some(workspace) = output_workspace
            && super::tool_output::compact(&mut result, &workspace).is_err()
        {
            // The operation may have succeeded: never replace its result with a
            // failed call or encourage replay just because offloading failed.
            warn!("tool output archive unavailable; retaining original result");
        }
        let mut response = if matches!(call.tool.as_str(), "sticker_lookup" | "sticker_manage") {
            sticker_tool_response(result, sticker_preview)
        } else {
            social_tool_response(result, call.tool == "public_browser")
        };
        if let Some(query) = timing_query
            && let Ok(timing) = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                query(r#"{"kind":"observe","text":""}"#.into()),
            )
            .await
            && timing.get("ok").and_then(JsonValue::as_bool) == Some(true)
            && timing.get("long_task").and_then(JsonValue::as_bool) == Some(true)
            && let Some(items) = response
                .get_mut("contentItems")
                .and_then(JsonValue::as_array_mut)
        {
            items.push(serde_json::json!({"type":"inputText","text":format!("Host task timing (current request only): {timing}")}));
        }
        let _ = client.respond_ok(req.id, &response).await;
    }

    async fn dispatch_command(&self, client: &Arc<JsonRpcClient>, req: ServerRequest) {
        let Some(params) =
            parse_approval_params::<CommandApprovalParams>(client, &req, "command approval").await
        else {
            return;
        };
        let has_handler = self.handler_tx.lock().await.is_some();
        info!(
            thread_id = %params.thread_id,
            command = params.command.as_deref().unwrap_or("(missing)"),
            cwd = params.cwd.as_deref().unwrap_or(""),
            reason = params.reason.as_deref().unwrap_or(""),
            has_handler,
            "command approval requested"
        );
        let event = CommandApprovalEvent {
            command: params.command,
            cwd: params.cwd,
            reason: params.reason,
        };
        self.respond_with_outcome(client, req.id, |tx| ApprovalRequest::Command {
            event,
            reply: tx,
        })
        .await;
    }

    async fn dispatch_file_change(&self, client: &Arc<JsonRpcClient>, req: ServerRequest) {
        let Some(params) =
            parse_approval_params::<FileChangeApprovalParams>(client, &req, "file-change approval")
                .await
        else {
            return;
        };
        let event = FileChangeApprovalEvent {
            reason: params.reason,
            grant_root: params.grant_root,
            file_changes: params.file_changes,
        };
        self.respond_with_outcome(client, req.id, |tx| ApprovalRequest::FileChange {
            event,
            reply: tx,
        })
        .await;
    }

    async fn dispatch_permissions(&self, client: &Arc<JsonRpcClient>, req: ServerRequest) {
        let Some(params) = parse_approval_params::<PermissionsApprovalParams>(
            client,
            &req,
            "permissions approval",
        )
        .await
        else {
            return;
        };
        let event = PermissionsApprovalEvent {
            reason: params.reason,
            permissions: params.permissions,
        };
        self.respond_with_outcome(client, req.id, |tx| ApprovalRequest::Permissions {
            event,
            reply: tx,
        })
        .await;
    }

    async fn dispatch_elicitation(&self, client: &Arc<JsonRpcClient>, req: ServerRequest) {
        let Some(params) =
            parse_approval_params::<McpElicitationParams>(client, &req, "elicitation").await
        else {
            return;
        };
        let event = ElicitationEvent {
            thread_id: params.thread_id,
            server: params.server,
        };
        let (tx, rx) = oneshot::channel();
        let sent = self
            .send_to_handler(ApprovalRequest::Elicitation { event, reply: tx })
            .await;
        let resp = if sent {
            match rx.await {
                Ok(Some(content)) => ElicitationResponse::Accept { content },
                Ok(None) | Err(_) => ElicitationResponse::Decline,
            }
        } else {
            ElicitationResponse::Decline
        };
        let _ = client.respond_ok(req.id, &resp).await;
    }

    /// Ask the handler for a decision and reply with the shared
    /// `{decision}` response body.
    async fn respond_with_outcome<F>(&self, client: &Arc<JsonRpcClient>, id: JsonValue, build: F)
    where
        F: FnOnce(oneshot::Sender<ApprovalOutcome>) -> ApprovalRequest,
    {
        let outcome = self.ask_outcome(build).await;
        let resp = ApprovalDecisionResponse {
            decision: outcome.to_decision(),
        };
        let _ = client.respond_ok(id, &resp).await;
    }

    async fn ask_outcome<F>(&self, build: F) -> ApprovalOutcome
    where
        F: FnOnce(oneshot::Sender<ApprovalOutcome>) -> ApprovalRequest,
    {
        let (tx, rx) = oneshot::channel();
        if !self.send_to_handler(build(tx)).await {
            return ApprovalOutcome::Decline;
        }
        rx.await.unwrap_or(ApprovalOutcome::Decline)
    }

    /// Send a request to the installed handler; `false` when no handler is
    /// installed or its channel is closed.
    async fn send_to_handler(&self, request: ApprovalRequest) -> bool {
        let guard = self.handler_tx.lock().await;
        if let Some(sender) = guard.as_ref() {
            sender.send(request).await.is_ok()
        } else {
            false
        }
    }
}

fn sticker_tool_response(result: JsonValue, preview: Option<Vec<u8>>) -> JsonValue {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let success = result.get("ok").and_then(JsonValue::as_bool) == Some(true);
    let mut items = vec![serde_json::json!({"type":"inputText","text":result.to_string()})];
    if let Some(bytes) = preview {
        if !success || bytes.len() > 2 * 1024 * 1024 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return serde_json::json!({"success":false,"contentItems":[{"type":"inputText","text":"{\"ok\":false,\"error\":\"sticker_preview_invalid\"}"}]});
        }
        items.push(serde_json::json!({"type":"inputImage","imageUrl":format!("data:image/png;base64,{}",STANDARD.encode(bytes))}));
    }
    serde_json::json!({"success":success,"contentItems":items})
}

#[cfg(test)]
mod sticker_content_tests {
    use super::*;
    #[test]
    fn ordinary_json_cannot_promote_an_image_but_typed_catalogue_preview_can() {
        let result = sticker_tool_response(
            serde_json::json!({"ok":true,"preview_data_url":"file:///etc/passwd"}),
            None,
        );
        assert_eq!(result["contentItems"].as_array().unwrap().len(), 1);
        let result = sticker_tool_response(
            serde_json::json!({"ok":true}),
            Some(b"\x89PNG\r\n\x1a\npreview".to_vec()),
        );
        assert_eq!(result["contentItems"][1]["type"], "inputImage");
        assert_eq!(
            sticker_tool_response(serde_json::json!({"ok":true}), Some(b"GIF89a".to_vec()))["success"],
            false
        );
    }
}

/// Parse `req.params` as `P`; on failure log it, reply invalid-params, and
/// return `None`.
async fn parse_approval_params<P: serde::de::DeserializeOwned>(
    client: &Arc<JsonRpcClient>,
    req: &ServerRequest,
    label: &str,
) -> Option<P> {
    match serde_json::from_value(req.params.clone()) {
        Ok(params) => Some(params),
        Err(err) => {
            warn!(error = %err, "failed to parse {} params", label);
            respond_parse_err(client, req.id.clone()).await;
            None
        }
    }
}

async fn respond_parse_err(client: &Arc<JsonRpcClient>, id: JsonValue) {
    let err = JsonRpcError {
        code: -32602,
        message: "invalid params".to_string(),
        data: None,
    };
    let _ = client.respond_err(id, err).await;
}

/// Public screenshots are native model image input, never base64 text context.
fn social_tool_response(mut result: JsonValue, allow_image: bool) -> JsonValue {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let success = result.get("ok").and_then(JsonValue::as_bool) == Some(true);
    let image = result
        .as_object_mut()
        .and_then(|object| object.remove("image"));
    let mut content = vec![serde_json::json!({"type":"inputText","text":result.to_string()})];
    if allow_image
        && success
        && let Some(image) = image
        && image.get("mime_type").and_then(JsonValue::as_str) == Some("image/png")
        && let Some(data) = image.get("data").and_then(JsonValue::as_str)
        && data.len() <= 2_800_000
        && let Ok(bytes) = STANDARD.decode(data)
        && bytes.len() <= 2 * 1024 * 1024
        && bytes.starts_with(b"\x89PNG\r\n\x1a\n")
    {
        content.push(serde_json::json!({"type":"inputImage","imageUrl":format!("data:image/png;base64,{data}")}));
    }
    serde_json::json!({"success":success,"contentItems":content})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "explicit anonymous public browser smoke; needs trusted runtime JSON, no model"]
    async fn public_browser_host_bridge_returns_native_screenshot() {
        use super::super::scoped_integrations::{
            self, BrowserConfig, BrowserRuntime, IntegrationConfig, IntegrationGrant,
        };
        let file = std::env::var("CLAWMINGLE_BROWSER_SMOKE_RUNTIME_JSON")
            .expect("runtime JSON must be provided explicitly");
        let runtime: BrowserRuntime =
            serde_json::from_slice(&std::fs::read(file).expect("runtime JSON readable"))
                .expect("runtime JSON shape");
        let scope = scoped_integrations::bind(
            &IntegrationConfig {
                browser: BrowserConfig {
                    enabled: true,
                    runtime: Some(runtime),
                },
                ..Default::default()
            },
            &IntegrationGrant {
                browser: true,
                ..Default::default()
            },
        )
        .unwrap();
        let (_sender, cancel) = tokio::sync::watch::channel(false);
        let result = scope
            .call(
                "public_browser",
                r#"{"url":"https://example.com","wait_ms":250,"screenshot":true}"#,
                cancel,
            )
            .await;
        assert_eq!(
            result["ok"],
            true,
            "public browser failed: {}",
            result.get("error").unwrap_or(&serde_json::Value::Null)
        );
        // The public example page is multilingual; its visible heading is not fixed.
        assert_eq!(result["title"], "Example Domain");
        assert!(
            result["text"]
                .as_str()
                .unwrap()
                .contains("documentation examples")
        );
        assert!(result["resources_fetched"].as_u64().unwrap() > 0);
        let response = social_tool_response(result, true);
        assert_eq!(response["contentItems"].as_array().unwrap().len(), 2);
        assert_eq!(response["contentItems"][1]["type"], "inputImage");
        let text = response["contentItems"][0]["text"].as_str().unwrap();
        let metadata: JsonValue = serde_json::from_str(text).unwrap();
        assert!(metadata.get("image").is_none());
        assert!(!text.contains("data:image"));
    }

    #[test]
    fn browser_screenshots_are_native_images_and_never_json_text() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let data = STANDARD.encode(b"\x89PNG\r\n\x1a\n");
        let source = serde_json::json!({"ok":true,"text":"public page","image":{"mime_type":"image/png","data":data}});
        let result = social_tool_response(source.clone(), true);
        assert_eq!(result["contentItems"].as_array().unwrap().len(), 2);
        assert_eq!(result["contentItems"][1]["type"], "inputImage");
        assert!(
            result["contentItems"][1]["imageUrl"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert!(
            !result["contentItems"][0]["text"]
                .as_str()
                .unwrap()
                .contains(&data)
        );
        assert_eq!(
            social_tool_response(source, false)["contentItems"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let invalid =
            serde_json::json!({"ok":true,"image":{"mime_type":"image/png","data":"not_png"}});
        assert_eq!(
            social_tool_response(invalid, true)["contentItems"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn outcome_converts_to_simple_decision_string() {
        let cases = [
            ("acceptForSession", ApprovalOutcome::AcceptForSession),
            ("decline", ApprovalOutcome::Decline),
        ];
        for (expected, outcome) in cases {
            let value = serde_json::to_value(outcome.to_decision()).unwrap();
            assert_eq!(
                value,
                serde_json::Value::String(expected.into()),
                "case: {expected}"
            );
        }
    }
}
