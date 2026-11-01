//! A short-lived Streamable HTTP MCP client. One registered endpoint per call,
//! explicit initialization, session/protocol headers, bounded JSON or SSE, and
//! exact read-only tool allowlists. No server requests or auto OAuth are exposed.
use super::{
    McpRegistration,
    network::{PublicConnection, bounded_body},
};
use anyhow::{Result, ensure};
use reqwest::{
    StatusCode,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
};

const CONTROL_LIMIT: usize = 256 * 1024;
// Transport resource guard, separate from Codex's native tool-output token
// budget. Search results include image metadata and may exceed control frames.
const TOOL_RESULT_LIMIT: usize = 32 * 1024 * 1024;
const PROTOCOL: &str = "2025-06-18";
#[derive(Debug, thiserror::Error)]
#[error("documents_not_dispatched")]
pub(crate) struct DocumentNotDispatched;
/// A definitive provider refusal for this call, not an ambiguous transport failure.
/// Earlier mutation stages may still have succeeded and remain journaled by the caller.
#[derive(Debug, thiserror::Error)]
#[error("documents_rate_limited")]
pub(crate) struct DocumentRateLimited;
#[derive(Debug, thiserror::Error)]
#[error("mcp_http_failed status={status}")]
struct McpHttpError {
    status: u16,
}
#[derive(Debug, thiserror::Error)]
#[error("mcp_response_failed code={code:?}")]
struct McpRpcError {
    code: Option<i64>,
}

fn successful_http(status: StatusCode) -> Result<()> {
    if !status.is_success() {
        return Err(McpHttpError {
            status: status.as_u16(),
        }
        .into());
    }
    Ok(())
}

fn document_transport_error(error: anyhow::Error, initializing: bool) -> anyhow::Error {
    if error
        .downcast_ref::<McpHttpError>()
        .is_some_and(|error| error.status == 429)
        || error
            .downcast_ref::<McpRpcError>()
            .is_some_and(|error| error.code == Some(400007))
    {
        anyhow::anyhow!(DocumentRateLimited)
    } else if initializing {
        anyhow::anyhow!(DocumentNotDispatched)
    } else {
        error
    }
}

struct Session {
    endpoint: PublicConnection,
    headers: HeaderMap,
    next_id: u64,
}
impl Session {
    async fn open(registration: &McpRegistration) -> Result<Self> {
        Self::open_with_prefix(registration, "Bearer ").await
    }
    async fn open_with_prefix(registration: &McpRegistration, prefix: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(path) = &registration.credential_file {
            // Exact private file only. Never log its path, body, or transport errors.
            let meta = fs::symlink_metadata(path)?;
            let uid = fs::metadata("/proc/self")?.uid();
            ensure!(
                meta.is_file()
                    && !meta.file_type().is_symlink()
                    && meta.uid() == uid
                    && meta.permissions().mode() & 0o077 == 0
                    && meta.nlink() == 1
                    && meta.len() <= 8192,
                "plugin_credential_invalid"
            );
            let parent = fs::metadata(
                path.parent()
                    .ok_or_else(|| anyhow::anyhow!("plugin_credential_invalid"))?,
            )?;
            ensure!(
                parent.is_dir() && parent.uid() == uid && parent.permissions().mode() & 0o077 == 0,
                "plugin_credential_invalid"
            );
            let mut file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(0o400000)
                .open(path)?;
            let actual = file.metadata()?;
            ensure!(
                actual.ino() == meta.ino() && actual.dev() == meta.dev(),
                "plugin_credential_changed"
            );
            let mut token = String::new();
            Read::by_ref(&mut file)
                .take(8193)
                .read_to_string(&mut token)?;
            let token = token.trim();
            ensure!(
                !token.is_empty() && token.len() <= 8192 && !token.chars().any(char::is_whitespace),
                "plugin_credential_invalid"
            );
            let mut value = HeaderValue::from_str(&format!("{prefix}{token}"))?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        let mut this = Self {
            endpoint: PublicConnection::new(&registration.endpoint).await?,
            headers,
            next_id: 1,
        };
        let init=this.request("initialize",json!({"protocolVersion":PROTOCOL,"capabilities":{},"clientInfo":{"name":"ClawMingle","version":"0.1"}}),CONTROL_LIMIT).await?;
        let version = init
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("mcp_protocol_missing"))?;
        ensure!(
            matches!(version, "2025-03-26" | "2025-06-18" | "2025-11-25"),
            "mcp_protocol_unsupported"
        );
        ensure!(
            init.get("capabilities")
                .and_then(|v| v.get("tools"))
                .is_some(),
            "mcp_tools_unavailable"
        );
        this.headers
            .insert("MCP-Protocol-Version", HeaderValue::from_str(version)?);
        let response = this
            .endpoint
            .client
            .post(this.endpoint.url.clone())
            .headers(this.headers.clone())
            .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .send()
            .await?;
        this.endpoint.verify(&response)?;
        successful_http(response.status())?;
        ensure!(
            response.status() == StatusCode::ACCEPTED,
            "mcp_notification_rejected"
        );
        Ok(this)
    }
    async fn request(&mut self, method: &str, params: Value, limit: usize) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let response = self
            .endpoint
            .client
            .post(self.endpoint.url.clone())
            .headers(self.headers.clone())
            .json(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .send()
            .await?;
        self.endpoint.verify(&response)?;
        successful_http(response.status())?;
        if method == "initialize"
            && let Some(session) = response.headers().get("Mcp-Session-Id")
        {
            ensure!(session.as_bytes().len() <= 256, "mcp_session_invalid");
            let mut value = session.clone();
            value.set_sensitive(true);
            self.headers.insert("Mcp-Session-Id", value);
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_owned();
        let body = bounded_body(response, limit).await?;
        parse_response(&content_type, &body, id)
    }
    async fn tools(&mut self, registration: &McpRegistration) -> Result<Vec<Value>> {
        let mut tools = Vec::new();
        let mut cursor = None;
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..4 {
            let page = self
                .request(
                    "tools/list",
                    cursor
                        .as_ref()
                        .map_or_else(|| json!({}), |c| json!({"cursor":c})),
                    CONTROL_LIMIT,
                )
                .await?;
            let entries = page
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("mcp_tools_invalid"))?;
            ensure!(entries.len() <= 256, "mcp_tools_limit");
            for entry in entries {
                if let Some(name) = entry.get("name").and_then(Value::as_str)
                    && registration.allowed_tools.iter().any(|t| t == name)
                {
                    ensure!(
                        entry.pointer("/annotations/destructiveHint") != Some(&json!(true))
                            && entry.pointer("/annotations/readOnlyHint") != Some(&json!(false)),
                        "mcp_write_tool_denied"
                    );
                    ensure!(
                        entry.get("inputSchema").is_some_and(Value::is_object),
                        "mcp_schema_missing"
                    );
                    if !tools.iter().any(|t: &Value| t["name"] == name) {
                        tools.push(json!({"name":name,"description":entry.get("description").and_then(Value::as_str).unwrap_or("").chars().take(1600).collect::<String>(),"inputSchema":entry["inputSchema"]}));
                    }
                }
            }
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(next) = &cursor {
                ensure!(
                    next.len() <= 2048 && seen.insert(next.clone()),
                    "mcp_cursor_invalid"
                );
            } else {
                return Ok(tools);
            }
        }
        anyhow::bail!("mcp_pagination_limit")
    }
    async fn close(self) {
        if self.headers.contains_key("Mcp-Session-Id") {
            let _ = self
                .endpoint
                .client
                .delete(self.endpoint.url)
                .headers(self.headers)
                .timeout(std::time::Duration::from_secs(2))
                .send()
                .await;
        }
    }
}

/// Dedicated provider adapter. This does not grant write access to generic MCP
/// registrations. The caller enforces group membership and journals mutations.
pub(crate) async fn tencent_docs_call(
    credential: &std::path::Path,
    endpoint: &str,
    provider_tool: &str,
    arguments: Value,
) -> Result<Value> {
    let registration = McpRegistration {
        enabled: true,
        name: "Tencent Docs".into(),
        endpoint: endpoint.into(),
        codex_app: false,
        app_id: None,
        tool_namespace: None,
        personal_account: false,
        allowed_tools: Vec::new(),
        credential_file: Some(credential.to_owned()),
    };
    let mut session = Session::open_with_prefix(&registration, "")
        .await
        .map_err(|error| document_transport_error(error, true))?;
    let result = session
        .request(
            "tools/call",
            json!({"name":provider_tool,"arguments":arguments}),
            TOOL_RESULT_LIMIT,
        )
        .await;
    session.close().await;
    document_result(result.map_err(|error| document_transport_error(error, false))?)
}

/// Provider status fields belong to this envelope only. Never inspect document
/// bodies, descriptions, nested data, or free-form text for error-code substrings.
fn document_rate_limit_envelope(value: &Value) -> bool {
    [
        value.get("ret"),
        value.get("code"),
        value.pointer("/error/code"),
    ]
    .into_iter()
    .flatten()
    .any(|code| code.as_i64() == Some(400007) || code.as_str() == Some("400007"))
}

fn document_result(result: Value) -> Result<Value> {
    if document_rate_limit_envelope(&result) {
        return Err(DocumentRateLimited.into());
    }
    let value: Result<Value> = if let Some(value) = result.get("structuredContent") {
        Ok(value.clone())
    } else {
        result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|items| items.iter().find(|v| v["type"] == "text"))
            .and_then(|v| v["text"].as_str())
            .ok_or_else(|| anyhow::anyhow!("documents_response_invalid"))
            .and_then(|text| serde_json::from_str(text).map_err(Into::into))
    };
    if value.as_ref().is_ok_and(document_rate_limit_envelope) {
        return Err(DocumentRateLimited.into());
    }
    ensure!(
        result.get("isError") != Some(&json!(true)),
        "documents_provider_rejected"
    );
    let value = value?;
    ensure!(
        value.is_object()
            && value
                .get("error")
                .is_none_or(|v| v.is_null() || v.as_str() == Some(""))
            && value.get("ret").is_none_or(|v| v.as_i64() == Some(0)),
        "documents_provider_rejected"
    );
    ensure!(
        value
            .get("err_msg")
            .is_none_or(|v| v.is_null() || v.as_str() == Some("")),
        "documents_provider_rejected"
    );
    Ok(value)
}

pub(super) async fn list(registration: &McpRegistration) -> Result<Value> {
    let mut session = Session::open(registration).await?;
    let result = session.tools(registration).await;
    session.close().await;
    let tools = result?;
    let output = json!({"ok":true,"tools":tools,"untrusted_external_content":true});
    ensure!(output.to_string().len() <= 48 * 1024, "mcp_schema_budget");
    Ok(output)
}
pub(super) async fn call(
    registration: &McpRegistration,
    tool: &str,
    arguments: Value,
) -> Result<Value> {
    let mut session = Session::open(registration).await?;
    let operation = async {
        let tools = session.tools(registration).await?;
        ensure!(
            tools.iter().any(|t| t["name"] == tool),
            "mcp_tool_not_available"
        );
        let result = session
            .request(
                "tools/call",
                json!({"name":tool,"arguments":arguments}),
                TOOL_RESULT_LIMIT,
            )
            .await?;
        text_result(result)
    }
    .await;
    session.close().await;
    operation
}
/// Keep text and structured results intact. Codex owns the model-facing output
/// budget; do not cut serialized JSON mid-value before it reaches that budget.
pub(super) fn text_result(result: Value) -> Result<Value> {
    ensure!(result.is_object(), "mcp_result_invalid");
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("mcp_content_invalid"))?;
    let mut text = Vec::new();
    let mut truncated = false;
    for item in content {
        if item.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(value) = item.get("text").and_then(Value::as_str) {
                text.push(value.to_owned());
            }
        } else {
            truncated = true;
        }
    }
    let structured = result.get("structuredContent").cloned();
    Ok(
        json!({"ok":result.get("isError")!=Some(&json!(true)),"content":text,"structured_content":structured,"truncated":truncated,"untrusted_external_content":true}),
    )
}
fn parse_response(content_type: &str, bytes: &[u8], id: u64) -> Result<Value> {
    let candidates: Vec<Value> = match content_type {
        "application/json" => vec![serde_json::from_slice(bytes)?],
        "text/event-stream" => {
            let text = std::str::from_utf8(bytes)?.replace("\r\n", "\n");
            let mut candidates = Vec::new();
            for event in text.split("\n\n") {
                let data = event
                    .lines()
                    .filter_map(|line| {
                        line.strip_prefix("data:")
                            .map(|s| s.strip_prefix(' ').unwrap_or(s))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !data.is_empty() {
                    candidates.push(serde_json::from_str(&data)?);
                }
            }
            candidates
        }
        _ => anyhow::bail!("mcp_content_type_invalid"),
    };
    let mut answer = None;
    for candidate in candidates {
        ensure!(
            candidate.get("jsonrpc") == Some(&json!("2.0")),
            "mcp_jsonrpc_invalid"
        );
        // No capabilities were declared: server requests (sampling/elicitation) are unsupported.
        ensure!(
            !(candidate.get("method").is_some() && candidate.get("id").is_some()),
            "mcp_server_request_unsupported"
        );
        if candidate.get("id") == Some(&json!(id)) {
            ensure!(answer.is_none(), "mcp_response_failed");
            if let Some(error) = candidate.get("error") {
                return Err(McpRpcError {
                    code: error.get("code").and_then(Value::as_i64),
                }
                .into());
            }
            answer = candidate.get("result").cloned();
        }
    }
    answer.ok_or_else(|| anyhow::anyhow!("mcp_response_missing"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn http_rate_limit_is_typed_at_initialization_and_call_stages() {
        for initializing in [true, false] {
            let error = successful_http(StatusCode::TOO_MANY_REQUESTS).unwrap_err();
            assert!(document_transport_error(error, initializing).is::<DocumentRateLimited>());
            for status in [StatusCode::UNAUTHORIZED, StatusCode::SERVICE_UNAVAILABLE] {
                let error =
                    document_transport_error(successful_http(status).unwrap_err(), initializing);
                assert!(!error.is::<DocumentRateLimited>());
                assert_eq!(error.is::<DocumentNotDispatched>(), initializing);
            }
        }
        assert!(successful_http(StatusCode::OK).is_ok());
        assert!(successful_http(StatusCode::ACCEPTED).is_ok());
    }

    #[test]
    fn matching_json_rpc_rate_limit_is_preserved_without_provider_messages() {
        let rpc = json!({"jsonrpc":"2.0","id":2,"error":{"code":400007,"message":"untrusted-private-provider-detail"}});
        for (content_type, body) in [
            ("application/json", rpc.to_string()),
            ("text/event-stream", format!("data: {rpc}\n\n")),
        ] {
            for initializing in [true, false] {
                let error = parse_response(content_type, body.as_bytes(), 2).unwrap_err();
                assert!(
                    !error
                        .to_string()
                        .contains("untrusted-private-provider-detail")
                );
                assert!(document_transport_error(error, initializing).is::<DocumentRateLimited>());
            }
            let unrelated = parse_response(content_type, body.as_bytes(), 3).unwrap_err();
            assert!(!document_transport_error(unrelated, false).is::<DocumentRateLimited>());
        }
    }

    #[test]
    fn initialization_auth_and_ambiguous_call_failures_are_not_rate_limits() {
        for code in [400006, 400012, 400010] {
            let rpc = json!({"jsonrpc":"2.0","id":2,"error":{"code":code}}).to_string();
            for initializing in [true, false] {
                let error = document_transport_error(
                    parse_response("application/json", rpc.as_bytes(), 2).unwrap_err(),
                    initializing,
                );
                assert!(!error.is::<DocumentRateLimited>());
                assert_eq!(error.is::<DocumentNotDispatched>(), initializing);
            }
        }
        let error = document_transport_error(anyhow::anyhow!("transport_result_unknown"), false);
        assert!(!error.is::<DocumentRateLimited>());
        assert!(!error.is::<DocumentNotDispatched>());
    }

    #[test]
    fn tencent_status_envelopes_are_classified_before_generic_mcp_rejection() {
        for value in [
            json!({"ret":400007}),
            json!({"code":400007}),
            json!({"error":{"code":400007}}),
            json!({"ret":"400007"}),
        ] {
            for is_error in [true, false] {
                for result in [
                    json!({"isError":is_error,"structuredContent":value}),
                    json!({"isError":is_error,"content":[{"type":"text","text":value.to_string()}]}),
                ] {
                    assert!(
                        document_result(result)
                            .unwrap_err()
                            .is::<DocumentRateLimited>()
                    );
                }
            }
            assert!(
                document_result(value)
                    .unwrap_err()
                    .is::<DocumentRateLimited>()
            );
        }
    }

    #[test]
    fn document_text_nested_data_and_other_errors_never_become_rate_limits() {
        for value in [
            json!({"ret":0,"data":{"code":400007,"ret":400007,"error":{"code":400007}}}),
            json!({"ret":0,"text":"400007 REQUESTS_OVER_LIMIT"}),
            json!({"ret":0,"text":"{\"code\":400007}"}),
        ] {
            assert!(document_result(json!({"structuredContent":value})).is_ok());
            assert!(
                document_result(json!({"content":[{"type":"text","text":value.to_string()}]}))
                    .is_ok()
            );
        }
        for result in [
            json!({"isError":true,"content":[{"type":"text","text":"untrusted text says 400007"}]}),
            json!({"isError":true,"structuredContent":{"code":400006}}),
            json!({"isError":true,"structuredContent":{"error":{"code":400012}}}),
            json!({"structuredContent":{"ret":-1,"error":"400007"}}),
        ] {
            assert!(
                !document_result(result)
                    .unwrap_err()
                    .is::<DocumentRateLimited>()
            );
        }
    }

    #[test]
    fn authoritative_payload_takes_precedence_over_other_document_text() {
        let value = json!({"ret":0,"data":{"title":"example"}});
        let code_text = json!({"type":"text","text":"{\"code\":400007}"});
        assert_eq!(
            document_result(json!({"structuredContent":value,"content":[code_text]})).unwrap(),
            value
        );
        assert_eq!(
            document_result(
                json!({"content":[{"type":"text","text":value.to_string()},code_text]})
            )
            .unwrap(),
            value
        );
    }

    #[test]
    fn structured_data_reaches_codex_without_an_extra_character_budget() {
        let data = serde_json::json!({"large":"x".repeat(16001)});
        let value =
            super::text_result(serde_json::json!({"content":[],"structuredContent":data})).unwrap();
        assert_eq!(value["truncated"], false);
        assert_eq!(value["structured_content"], data);
    }

    use super::*;
    #[tokio::test]
    async fn large_json_and_sse_search_results_are_parsed_before_native_budgeting() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
        let server = MockServer::start().await;
        let payload = json!({"code":200,"data":{"notes":[{"title":"hotel","metadata":"图".repeat(130_000)}]}});
        let result = json!({"content":[{"type":"text","text":payload.to_string()}],"structuredContent":payload});
        let rpc = json!({"jsonrpc":"2.0","id":2,"result":result}).to_string();
        for (route, content_type, body) in [
            ("/json", "application/json", rpc.clone()),
            (
                "/sse",
                "text/event-stream",
                format!("event: message\r\ndata: {rpc}\r\n\r\n"),
            ),
        ] {
            assert!(body.len() > CONTROL_LIMIT && body.len() < TOOL_RESULT_LIMIT);
            Mock::given(path(route))
                .respond_with(ResponseTemplate::new(200).set_body_raw(body, content_type))
                .mount(&server)
                .await;
            let url = format!("{}{route}", server.uri());
            let response = reqwest::get(&url).await.unwrap();
            let bytes = bounded_body(response, TOOL_RESULT_LIMIT).await.unwrap();
            let parsed = text_result(parse_response(content_type, &bytes, 2).unwrap()).unwrap();
            assert_eq!(parsed["ok"], true);
            assert_eq!(parsed["truncated"], false);
            assert_eq!(
                serde_json::from_str::<Value>(parsed["content"][0].as_str().unwrap()).unwrap(),
                payload
            );
            assert_eq!(parsed["structured_content"], payload);
            // Initialisation, catalogues and document mutations retain their
            // separate control-frame resource guard.
            let response = reqwest::get(&url).await.unwrap();
            assert!(
                bounded_body(response, CONTROL_LIMIT)
                    .await
                    .unwrap_err()
                    .is::<super::super::network::ResponseTooLarge>()
            );
        }
    }
    #[test]
    fn json_and_sse_responses_match_request() {
        assert_eq!(
            parse_response(
                "application/json",
                br#"{"jsonrpc":"2.0","id":2,"result":{"a":1}}"#,
                2
            )
            .unwrap()["a"],
            1
        );
        assert_eq!(parse_response("text/event-stream",b"event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"a\":1}}\r\n\r\n",2).unwrap()["a"],1);
    }
    #[test]
    fn errors_unsolicited_requests_and_wrong_ids_are_rejected() {
        for value in [
            r#"{"jsonrpc":"2.0","id":3,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":2,"error":{"message":"private"}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"sampling/createMessage"}"#,
        ] {
            assert!(parse_response("application/json", value.as_bytes(), 2).is_err());
        }
    }
    #[test]
    fn text_reaches_codex_intact_without_fetching_embedded_resources() {
        let value=text_result(json!({"content":[{"type":"text","text":"x".repeat(30000)},{"type":"resource","resource":{"uri":"file:///private"}}]})).unwrap();
        assert_eq!(value["content"][0].as_str().unwrap().len(), 30000);
        assert_eq!(value["truncated"], true);
        assert!(!value.to_string().contains("file:///private"));
    }
}
