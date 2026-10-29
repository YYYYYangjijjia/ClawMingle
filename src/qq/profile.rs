//! Fixed GET-only QQ profile operations; raw responses stay in the host broker.
use super::QqApiClient;
use crate::qq::message_content::MessageScope;
use serde_json::Value;

pub enum QqReadRequest {
    SelfInfo,
    GroupInfo,
    Members { cursor: Option<String> },
    Member { member_openid: String },
    BotState,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum QqReadError {
    #[error("qq_info_invalid_request")]
    InvalidRequest,
    #[error("qq_info_authentication_failed")]
    Authentication,
    #[error("qq_info_permission_denied")]
    PermissionDenied,
    #[error("qq_info_platform_rejected")]
    Rejected { status: u16, code: Option<i64> },
    #[error("qq_info_network_failed")]
    Network,
    #[error("qq_info_response_invalid")]
    InvalidResponse,
    #[error("qq_info_response_too_large")]
    ResponseTooLarge,
}

fn identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

impl QqApiClient {
    pub async fn read_profile(
        &self,
        scope: MessageScope,
        target: &str,
        request: QqReadRequest,
    ) -> Result<Value, QqReadError> {
        use QqReadError as E;
        let mut url = if matches!(request, QqReadRequest::SelfInfo) {
            let mut url =
                reqwest::Url::parse(&self.config.api_base_url).map_err(|_| E::InvalidRequest)?;
            if !matches!(url.scheme(), "https" | "http")
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(E::InvalidRequest);
            }
            url.set_query(None);
            url.set_fragment(None);
            url.path_segments_mut()
                .map_err(|_| E::InvalidRequest)?
                .pop_if_empty()
                .extend(["users", "@me"]);
            url
        } else {
            if scope != MessageScope::Group || !identity(target) {
                return Err(E::InvalidRequest);
            }
            let resource = match &request {
                QqReadRequest::GroupInfo => "info",
                QqReadRequest::BotState => "bot_state",
                _ => "members",
            };
            self.passive_endpoint(scope, target, resource)
                .map_err(|_| E::InvalidRequest)?
        };
        match &request {
            QqReadRequest::Member { member_openid } => {
                if !identity(member_openid) {
                    return Err(E::InvalidRequest);
                }
                url.path_segments_mut()
                    .map_err(|_| E::InvalidRequest)?
                    .push(member_openid);
            }
            QqReadRequest::Members {
                cursor: Some(cursor),
            } => {
                if cursor.len() > 512 || cursor.chars().any(char::is_control) {
                    return Err(E::InvalidRequest);
                }
                url.query_pairs_mut().append_pair("cursor", cursor);
            }
            _ => {}
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let token = self
                .get_access_token()
                .await
                .map_err(|_| E::Authentication)?;
            let mut response = self
                .group_client
                .get(url)
                .header("Authorization", format!("QQBot {token}"))
                .header("X-Union-Appid", &self.config.app_id)
                .send()
                .await
                .map_err(|_| E::Network)?;
            let status = response.status();
            if status.as_u16() == 403 {
                return Err(E::PermissionDenied);
            }
            const LIMIT: usize = 128 * 1024;
            if response
                .content_length()
                .is_some_and(|size| size > LIMIT as u64)
            {
                return Err(E::ResponseTooLarge);
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| E::Network)? {
                if bytes.len() + chunk.len() > LIMIT {
                    return Err(E::ResponseTooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            let value = serde_json::from_slice::<Value>(&bytes).map_err(|_| E::InvalidResponse)?;
            let code = value
                .get("code")
                .or_else(|| value.get("err_code"))
                .and_then(Value::as_i64);
            if code == Some(11253) {
                return Err(E::PermissionDenied);
            }
            if !status.is_success() || code.is_some_and(|code| code != 0) {
                return Err(E::Rejected {
                    status: status.as_u16(),
                    code,
                });
            }
            if !value.is_object() {
                return Err(E::InvalidResponse);
            }
            Ok(value)
        })
        .await
        .map_err(|_| E::Network)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QqConfig;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path, query_param},
    };
    fn client(server: &MockServer) -> QqApiClient {
        QqApiClient::new(QqConfig {
            app_id: "fixture-app".into(),
            app_secret: "EXAMPLE_QQ_APP_SECRET".into(),
            api_base_url: server.uri(),
            token_url: format!("{}/token", server.uri()),
        })
        .unwrap()
    }
    async fn token(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"access_token":"EXAMPLE_QQ_ACCESS_TOKEN","expires_in":7200}),
                ),
            )
            .expect(1)
            .mount(server)
            .await;
    }
    #[tokio::test]
    async fn read_routes_are_fixed_scoped_authenticated_and_get_only() {
        let server = MockServer::start().await;
        token(&server).await;
        let qq = client(&server);
        for (route, request) in [
            ("/users/@me", QqReadRequest::SelfInfo),
            ("/v2/groups/current/info", QqReadRequest::GroupInfo),
            ("/v2/groups/current/bot_state", QqReadRequest::BotState),
            (
                "/v2/groups/current/members/known",
                QqReadRequest::Member {
                    member_openid: "known".into(),
                },
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(route))
                .and(header("Authorization", "QQBot EXAMPLE_QQ_ACCESS_TOKEN"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok":true})))
                .expect(1)
                .mount(&server)
                .await;
            qq.read_profile(MessageScope::Group, "current", request)
                .await
                .unwrap();
        }
        Mock::given(method("GET"))
            .and(path("/v2/groups/current/members"))
            .and(query_param("cursor", "next page"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"members":[]})))
            .expect(1)
            .mount(&server)
            .await;
        qq.read_profile(
            MessageScope::Group,
            "current",
            QqReadRequest::Members {
                cursor: Some("next page".into()),
            },
        )
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn invalid_scope_and_identifiers_never_request_credentials_or_network() {
        let server = MockServer::start().await;
        let qq = client(&server);
        assert_eq!(
            qq.read_profile(MessageScope::Private, "current", QqReadRequest::GroupInfo)
                .await
                .unwrap_err(),
            QqReadError::InvalidRequest
        );
        assert_eq!(
            qq.read_profile(MessageScope::Group, "../other", QqReadRequest::GroupInfo)
                .await
                .unwrap_err(),
            QqReadError::InvalidRequest
        );
        assert_eq!(
            qq.read_profile(
                MessageScope::Group,
                "current",
                QqReadRequest::Member {
                    member_openid: "../other".into()
                }
            )
            .await
            .unwrap_err(),
            QqReadError::InvalidRequest
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn permission_redirect_and_oversize_fail_without_retry_or_follow() {
        for (status, body, expected) in [
            (403, b"denied".to_vec(), QqReadError::PermissionDenied),
            (
                200,
                br#"{"code":11253}"#.to_vec(),
                QqReadError::PermissionDenied,
            ),
            (
                302,
                b"{}".to_vec(),
                QqReadError::Rejected {
                    status: 302,
                    code: None,
                },
            ),
            (
                200,
                vec![b'x'; 128 * 1024 + 1],
                QqReadError::ResponseTooLarge,
            ),
        ] {
            let server = MockServer::start().await;
            token(&server).await;
            Mock::given(method("GET"))
                .and(path("/v2/groups/current/info"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("Location", "/not-followed")
                        .set_body_bytes(body),
                )
                .expect(1)
                .mount(&server)
                .await;
            assert_eq!(
                client(&server)
                    .read_profile(MessageScope::Group, "current", QqReadRequest::GroupInfo)
                    .await
                    .unwrap_err(),
                expected
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
        }
    }
}
