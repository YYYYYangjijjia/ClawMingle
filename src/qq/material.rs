//! Original GIF/WebP delivery through QQ's documented Markdown asset API.
//! Uploading cannot send a message; the final message POST remains single-shot.
use super::*;
use serde_json::json;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialImage {
    pub image_id: String,
    pub reference: String,
    pub ttl: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MaterialError {
    #[error("material_request_invalid")]
    Invalid,
    #[error("material_authentication_failed")]
    Authentication,
    #[error("material_transport_failed")]
    Transport,
    #[error("material_api_rejected_{status}")]
    Rejected { status: u16 },
    #[error("material_response_invalid")]
    Response,
}
fn valid_reference(reference: &str) -> bool {
    reference
        .strip_prefix("qqbot-media://image/v1/")
        .is_some_and(|token| {
            !token.is_empty()
                && token.len() <= 2048
                && token
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.~+/=%".contains(&byte))
        })
}
fn put_url(value: &str) -> std::result::Result<Url, MaterialError> {
    if value.len() > 8192 {
        return Err(MaterialError::Response);
    }
    let url = Url::parse(value).map_err(|_| MaterialError::Response)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port().is_some_and(|port| port != 443)
        || !url
            .host_str()
            .is_some_and(|host| host.contains(".cos.") && host.ends_with(".myqcloud.com"))
    {
        return Err(MaterialError::Response);
    }
    Ok(url)
}
impl QqApiClient {
    async fn material_post(
        &self,
        path: &str,
        body: &Value,
    ) -> std::result::Result<Value, MaterialError> {
        let token = self
            .get_access_token()
            .await
            .map_err(|_| MaterialError::Authentication)?;
        let url = format!(
            "{}/v2/media-images/{path}",
            self.config.api_base_url.trim_end_matches('/')
        );
        let mut response = self
            .group_client
            .post(url)
            .header("Authorization", format!("QQBot {token}"))
            .header("X-Union-Appid", &self.config.app_id)
            .json(body)
            .send()
            .await
            .map_err(|_| MaterialError::Transport)?;
        if !response.status().is_success() {
            return Err(MaterialError::Rejected {
                status: response.status().as_u16(),
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > 65536)
        {
            return Err(MaterialError::Response);
        }
        let mut bytes = Vec::new();
        while let Some(part) = response
            .chunk()
            .await
            .map_err(|_| MaterialError::Transport)?
        {
            if bytes.len() + part.len() > 65536 {
                return Err(MaterialError::Response);
            }
            bytes.extend_from_slice(&part);
        }
        serde_json::from_slice(&bytes).map_err(|_| MaterialError::Response)
    }
    /// Validated original bytes, no external resource URL or model-selected path.
    pub async fn upload_material_image(
        &self,
        bytes: &[u8],
        mime: &str,
    ) -> std::result::Result<MaterialImage, MaterialError> {
        self.upload_material_inner(bytes, mime, None).await
    }
    async fn upload_material_inner(
        &self,
        bytes: &[u8],
        mime: &str,
        put_override: Option<&str>,
    ) -> std::result::Result<MaterialImage, MaterialError> {
        let extension = match mime {
            "image/png" if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => "png",
            "image/jpeg" if bytes.starts_with(b"\xff\xd8\xff") => "jpeg",
            "image/gif" if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") => "gif",
            "image/webp"
                if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" =>
            {
                "webp"
            }
            _ => return Err(MaterialError::Invalid),
        };
        if bytes.is_empty() || bytes.len() > MAX_PASSIVE_IMAGE_BYTES {
            return Err(MaterialError::Invalid);
        }
        let md5 = format!("{:x}", md5::compute(bytes));
        let sha1 = format!("{:x}", Sha1::digest(bytes));
        let prepared=self.material_post("prepare",&json!({"file_name":format!("sticker.{extension}"),"file_size":bytes.len(),"content_type":mime,"md5":md5,"sha1":sha1})).await?;
        let id = prepared["upload_id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 1024)
            .ok_or(MaterialError::Response)?;
        let presigned = prepared["presigned_url"]
            .as_str()
            .ok_or(MaterialError::Response)?;
        let mut target = put_url(presigned)?;
        // This transport seam is unavailable in production builds.
        if cfg!(test)
            && let Some(value) = put_override
        {
            target = Url::parse(value).map_err(|_| MaterialError::Invalid)?;
        }
        let response = self
            .group_client
            .put(target)
            .header("Content-Type", mime)
            .body(bytes.to_vec())
            .send()
            .await
            .map_err(|_| MaterialError::Transport)?;
        if !response.status().is_success() {
            return Err(MaterialError::Rejected {
                status: response.status().as_u16(),
            });
        }
        let etag = response
            .headers()
            .get("ETag")
            .and_then(|value| value.to_str().ok())
            .map(|value| value.trim_matches('"'))
            .ok_or(MaterialError::Response)?;
        if !etag.eq_ignore_ascii_case(&md5) {
            return Err(MaterialError::Response);
        }
        let result = self
            .material_post("complete", &json!({"upload_id":id,"md5":md5}))
            .await?;
        let image_id = result["image_id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 1024 && !id.chars().any(char::is_control))
            .ok_or(MaterialError::Response)?;
        let reference = result["url"]
            .as_str()
            .filter(|reference| valid_reference(reference))
            .ok_or(MaterialError::Response)?;
        let ttl = result["ttl"]
            .as_u64()
            .filter(|ttl| *ttl > 0 && *ttl <= 90 * 86400)
            .ok_or(MaterialError::Response)?;
        // preview_url and signed upload URLs are deliberately not retained.
        Ok(MaterialImage {
            image_id: image_id.into(),
            reference: reference.into(),
            ttl,
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn send_passive_material(
        &self,
        scope: super::super::message_content::MessageScope,
        target: &str,
        passive_id: &str,
        msg_seq: u32,
        material: &MaterialImage,
        alt: &str,
        reference_idx: Option<&str>,
    ) -> std::result::Result<GroupMessageAck, GroupSendError> {
        if passive_id.is_empty() || msg_seq == 0 || !valid_reference(&material.reference) {
            return Err(GroupSendError::NotSent {
                reason: GroupNotSentReason::InvalidRequest,
            });
        }
        let label = alt
            .chars()
            .filter(|ch| !ch.is_control() && !"[]()\\".contains(*ch))
            .take(100)
            .collect::<String>();
        let content = format!(
            "![{}]({})",
            if label.is_empty() { "表情" } else { &label },
            material.reference
        );
        let mut body = json!({"msg_type":2,"msg_id":passive_id,"msg_seq":msg_seq,"markdown":{"content":content}});
        if let Some(reference) = reference_idx {
            if reference.is_empty() {
                return Err(GroupSendError::NotSent {
                    reason: GroupNotSentReason::InvalidRequest,
                });
            }
            body["message_reference"] = json!({"message_id":reference});
        }
        let url = self
            .passive_endpoint(scope, target, "messages")
            .map_err(|reason| GroupSendError::NotSent { reason })?;
        self.post_passive_body(url, &body, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, header, method, path},
    };
    #[test]
    fn signed_upload_urls_and_material_refs_cannot_be_arbitrary_destinations() {
        for value in [
            "https://example.com/x",
            "http://bucket.cos.ap-guangzhou.myqcloud.com/x",
            "https://bucket.cos.ap-guangzhou.myqcloud.com.evil.example/x",
            "https://user@bucket.cos.ap-guangzhou.myqcloud.com/x",
        ] {
            assert!(put_url(value).is_err());
        }
        assert!(
            put_url("https://bucket.cos.ap-guangzhou.myqcloud.com/x?signature=EXAMPLE").is_ok()
        );
        assert!(!valid_reference(
            "qqbot-media://image/v1/token)[x](https://example.com)"
        ));
    }
    #[tokio::test]
    async fn original_gif_upload_and_single_acknowledged_markdown_send() {
        let server = MockServer::start().await;
        let config = QqConfig {
            app_id: "example".into(),
            app_secret: "EXAMPLE_QQ_SECRET".into(),
            api_base_url: server.uri(),
            token_url: format!("{}/token", server.uri()),
        };
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token":"EXAMPLE_QQ_TOKEN","expires_in":7200})),
            )
            .mount(&server)
            .await;
        let bytes = b"GIF89a-original-fixture";
        let hash = format!("{:x}", md5::compute(bytes));
        Mock::given(method("POST")).and(path("/v2/media-images/prepare")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"upload_id":"upload-example","presigned_url":"https://bucket.cos.ap-guangzhou.myqcloud.com/original","expires_in":3600}))).expect(1).mount(&server).await;
        Mock::given(method("PUT"))
            .and(path("/put"))
            .and(header("Content-Type", "image/gif"))
            .respond_with(ResponseTemplate::new(200).insert_header("ETag", format!("\"{hash}\"")))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/v2/media-images/complete")).and(body_json(json!({"upload_id":"upload-example","md5":hash}))).respond_with(ResponseTemplate::new(200).set_body_json(json!({"image_id":"image-example","url":"qqbot-media://image/v1/example_token","ttl":7776000,"preview_url":"https://example.com/do-not-save"}))).expect(1).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/v2/groups/group/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"ack-example"})))
            .expect(1)
            .mount(&server)
            .await;
        let client = QqApiClient::new(config).unwrap();
        let material = client
            .upload_material_inner(bytes, "image/gif", Some(&format!("{}/put", server.uri())))
            .await
            .unwrap();
        assert_eq!(
            client
                .send_passive_material(
                    super::super::super::message_content::MessageScope::Group,
                    "group",
                    "source",
                    1,
                    &material,
                    "hi",
                    None
                )
                .await
                .unwrap()
                .id,
            "ack-example"
        );
        let requests = server.received_requests().await.unwrap();
        let put = requests
            .iter()
            .find(|request| request.method == "PUT")
            .unwrap();
        assert_eq!(put.body, bytes);
        assert!(put.headers.get("Authorization").is_none());
    }
    #[tokio::test]
    async fn uncertain_material_message_is_one_post_without_plain_text_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token":"EXAMPLE_QQ_TOKEN","expires_in":7200})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/users/member/messages"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;
        let client = QqApiClient::new(QqConfig {
            app_id: "example".into(),
            app_secret: "EXAMPLE_QQ_SECRET".into(),
            api_base_url: server.uri(),
            token_url: format!("{}/token", server.uri()),
        })
        .unwrap();
        let material = MaterialImage {
            image_id: "example".into(),
            reference: "qqbot-media://image/v1/example".into(),
            ttl: 3600,
        };
        let result = client
            .send_passive_material(
                super::super::super::message_content::MessageScope::Private,
                "member",
                "source",
                1,
                &material,
                "wave",
                None,
            )
            .await;
        assert!(matches!(result, Err(GroupSendError::Unknown { .. })));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path().ends_with("/messages"))
                .count(),
            1
        );
    }
}
