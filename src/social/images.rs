//! Read only current-request QQ image attachments into bounded in-memory data URLs.
//!
//! Signed remote URLs are transport secrets: never put this value in a prompt or
//! raw error/log output. The caller selects actual request source attachments;
//! ordinary historical context must not be fetched or passed to the model.
//! This module writes no files, decodes no image pixels and runs no programs.

use std::{fmt, time::Duration};

use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{Client, Url, header};
use serde::{Deserialize, Serialize};

pub const MAX_IMAGES: usize = 3;
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_TOTAL_IMAGE_BYTES: usize = 6 * 1024 * 1024;
const MAX_SOURCE_URL_BYTES: usize = 8192;
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BASE64_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;
// Current official GROUP_MESSAGE_CREATE / GROUP_AT_MESSAGE_CREATE examples
// use this exact host. Unverified legacy hosts are intentionally not allowed.
const IMAGE_HOSTS: &[&str] = &["multimedia.nt.qq.com.cn"];

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteImage {
    pub url: String,
    /// Untrusted QQ metadata, retained for compatibility. It cannot determine the
    /// output MIME: the response Content-Type and bytes must agree instead.
    #[serde(default)]
    pub content_type: String,
}

impl fmt::Debug for RemoteImage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteImage")
            .field("url", &"[redacted]")
            .field("content_type_present", &!self.content_type.is_empty())
            .finish()
    }
}

fn source_url(value: &str) -> Result<Url> {
    ensure!(
        value.len() <= MAX_SOURCE_URL_BYTES
            && !value
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control() || ch == '\\'),
        "image_source_invalid"
    );
    // Reject even empty userinfo, which Url otherwise normalizes away. Only
    // explicit HTTPS URLs are accepted; QQ signed query strings stay intact.
    let authority = value
        .strip_prefix("https://")
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .ok_or_else(|| anyhow::anyhow!("image_source_invalid"))?;
    ensure!(!authority.contains('@'), "image_source_invalid");
    let url = Url::parse(value).map_err(|_| anyhow::anyhow!("image_source_invalid"))?;
    ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.port().is_none_or(|port| port == 443)
            && url
                .host_str()
                .is_some_and(|host| IMAGE_HOSTS.contains(&host)),
        "image_source_invalid"
    );
    Ok(url)
}

fn sources(images: &[RemoteImage]) -> Result<Vec<Url>> {
    ensure!(images.len() <= MAX_IMAGES, "image_count_exceeded");
    images.iter().map(|image| source_url(&image.url)).collect()
}

/// Fetch at most three HTTPS QQ images. Each request, including streaming its
/// body, has a ten-second deadline; there are no retries, redirects or fallback.
/// No QQ token, cookies or inherited authentication is supplied to the CDN.
pub async fn fetch_data_urls(images: &[RemoteImage]) -> Result<Vec<String>> {
    // Validate the entire set before making any network request.
    let urls = sources(images)?;
    if urls.is_empty() {
        return Ok(Vec::new());
    }
    let client = Client::builder()
        .https_only(true)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|_| anyhow::anyhow!("image_client_failed"))?;
    let data = fetch_urls(&client, urls).await?;
    validate_social_data_urls(&data)?;
    Ok(data)
}

async fn fetch_urls(client: &Client, urls: Vec<Url>) -> Result<Vec<String>> {
    let mut total_bytes = 0;
    let mut data_urls = Vec::with_capacity(urls.len());
    for url in urls {
        let (mime, bytes) = tokio::time::timeout(FETCH_TIMEOUT, fetch_one(client, url))
            .await
            .map_err(|_| anyhow::anyhow!("image_fetch_timeout"))??;
        total_bytes += bytes.len();
        ensure!(
            total_bytes <= MAX_TOTAL_IMAGE_BYTES,
            "image_total_too_large"
        );
        data_urls.push(format!("data:{mime};base64,{}", STANDARD.encode(bytes)));
    }
    Ok(data_urls)
}

async fn fetch_one(client: &Client, url: Url) -> Result<(&'static str, Vec<u8>)> {
    let mut response = client
        .get(url)
        .header(header::ACCEPT, "image/png,image/jpeg,image/webp,image/gif")
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                anyhow::anyhow!("image_fetch_timeout")
            } else {
                anyhow::anyhow!("image_fetch_failed")
            }
        })?;
    ensure!(
        !response.status().is_redirection(),
        "image_redirect_rejected"
    );
    ensure!(response.status() == 200, "image_http_rejected");
    ensure!(
        response
            .headers()
            .get_all(header::CONTENT_ENCODING)
            .iter()
            .all(|value| value
                .to_str()
                .is_ok_and(|value| value.eq_ignore_ascii_case("identity"))),
        "image_encoding_unsupported"
    );
    let content_types = response.headers().get_all(header::CONTENT_TYPE);
    let mut content_type_values = content_types.iter();
    let content_type = content_type_values
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(|value| value.trim().to_ascii_lowercase())
        .ok_or_else(|| anyhow::anyhow!("image_content_type_invalid"))?;
    ensure!(
        content_type_values.next().is_none() && supported_mime(&content_type),
        "image_content_type_invalid"
    );
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= MAX_IMAGE_BYTES as u64),
        "image_too_large"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        if error.is_timeout() {
            anyhow::anyhow!("image_fetch_timeout")
        } else {
            anyhow::anyhow!("image_fetch_failed")
        }
    })? {
        ensure!(
            chunk.len() <= MAX_IMAGE_BYTES - bytes.len(),
            "image_too_large"
        );
        bytes.extend_from_slice(&chunk);
    }
    let mime = sniff_mime(&bytes).ok_or_else(|| anyhow::anyhow!("image_format_unsupported"))?;
    ensure!(mime == content_type, "image_mime_mismatch");
    Ok((mime, bytes))
}

fn supported_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    )
}

pub(super) fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Pure boundary check for `turn/start` image URLs. External HTTP/file URLs,
/// alternate data encodings, unsupported MIME, excess bytes and mismatched magic
/// are rejected. This does not decode pixels or claim full image validity.
pub fn validate_social_data_urls(data_urls: &[String]) -> Result<()> {
    ensure!(data_urls.len() <= MAX_IMAGES, "image_count_exceeded");
    let mut total_bytes = 0;
    for value in data_urls {
        let (mime, encoded) = value
            .strip_prefix("data:")
            .and_then(|value| value.split_once(";base64,"))
            .ok_or_else(|| anyhow::anyhow!("image_data_url_invalid"))?;
        ensure!(
            supported_mime(mime) && !encoded.is_empty(),
            "image_data_url_invalid"
        );
        ensure!(encoded.len() <= MAX_BASE64_BYTES, "image_too_large");
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| anyhow::anyhow!("image_data_url_invalid"))?;
        ensure!(bytes.len() <= MAX_IMAGE_BYTES, "image_too_large");
        ensure!(sniff_mime(&bytes) == Some(mime), "image_mime_mismatch");
        total_bytes += bytes.len();
        ensure!(
            total_bytes <= MAX_TOTAL_IMAGE_BYTES,
            "image_total_too_large"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    // A real one-pixel PNG; no decoder or disk fixture is required.
    const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    fn image() -> RemoteImage {
        RemoteImage {
            url: "https://multimedia.nt.qq.com.cn/download?fileid=example&rkey=EXAMPLE_SIGNATURE"
                .into(),
            content_type: "image/png".into(),
        }
    }

    fn png() -> Vec<u8> {
        STANDARD.decode(PNG_BASE64).unwrap()
    }

    // The only transport override is compiled into local tests. The production
    // path cannot replace its validated HTTPS CDN URL with an arbitrary endpoint.
    async fn from_test_endpoints(
        images: &[RemoteImage],
        endpoints: &[String],
    ) -> Result<Vec<String>> {
        let _ = sources(images)?;
        assert_eq!(images.len(), endpoints.len());
        let urls = endpoints
            .iter()
            .map(|value| {
                let url = Url::parse(value).unwrap();
                assert_eq!(url.scheme(), "http");
                assert!(matches!(url.host_str(), Some("127.0.0.1" | "::1")));
                url
            })
            .collect();
        let client = Client::builder()
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .timeout(FETCH_TIMEOUT)
            .build()
            .unwrap();
        fetch_urls(&client, urls).await
    }

    #[test]
    fn only_exact_https_qq_image_hosts_and_signed_queries_are_allowed() {
        for url in [
            "https://multimedia.nt.qq.com.cn/download?fileid=example",
            "https://multimedia.nt.qq.com.cn:443/download?fileid=example&rkey=EXAMPLE%2BVALUE",
        ] {
            let parsed = source_url(url).unwrap();
            assert_eq!(parsed.scheme(), "https");
        }
        for url in [
            "https://gchat.qpic.cn/example",
            "http://multimedia.nt.qq.com.cn/example",
            "//multimedia.nt.qq.com.cn/example",
            "file:///tmp/example.png",
            "data:image/png;base64,example",
            "https://localhost/example",
            "https://127.0.0.1/example",
            "https://[::1]/example",
            "https://multimedia.nt.qq.com.cn.evil.invalid/example",
            "https://images.multimedia.nt.qq.com.cn/example",
            "https://multimedia.nt.qq.com.cn./example",
            "https://@multimedia.nt.qq.com.cn/example",
            "https://multimedia.nt.qq.com.cn:8443/example",
            "https://multimedia.nt.qq.com.cn/example#fragment",
            "https://multimedia.nt.qq.com.cn/evil\\path",
            "https://multimedia.nt.qq.com.cn/space here",
        ] {
            assert_eq!(
                source_url(url).unwrap_err().to_string(),
                "image_source_invalid"
            );
        }
        let private_address = std::net::Ipv4Addr::new(10, 0, 0, 1);
        assert!(source_url(&format!("https://{private_address}/example")).is_err());
        let mut credentials = Url::parse("https://multimedia.nt.qq.com.cn/example").unwrap();
        credentials.set_username("EXAMPLE_USER").unwrap();
        credentials.set_password(Some("EXAMPLE_PASSWORD")).unwrap();
        assert!(source_url(credentials.as_str()).is_err());
    }

    #[test]
    fn remote_image_serde_retains_metadata_but_debug_never_exposes_urls_or_metadata() {
        let mut source = image();
        source.content_type = "EXAMPLE_PRIVATE_METADATA".into();
        let encoded = serde_json::to_string(&source).unwrap();
        assert_eq!(
            serde_json::from_str::<RemoteImage>(&encoded).unwrap(),
            source
        );
        let debug = format!("{source:?}");
        assert!(!debug.contains("EXAMPLE_SIGNATURE"));
        assert!(!debug.contains("EXAMPLE_PRIVATE_METADATA"));
        assert!(!debug.contains("multimedia.nt.qq.com.cn"));
    }

    #[tokio::test]
    async fn empty_set_needs_no_network_and_excess_count_is_never_silently_truncated() {
        assert!(fetch_data_urls(&[]).await.unwrap().is_empty());
        assert_eq!(
            fetch_data_urls(&vec![image(); MAX_IMAGES + 1])
                .await
                .unwrap_err()
                .to_string(),
            "image_count_exceeded"
        );
        assert_eq!(
            validate_social_data_urls(&vec![String::new(); MAX_IMAGES + 1])
                .unwrap_err()
                .to_string(),
            "image_count_exceeded"
        );
    }

    #[test]
    fn data_url_boundary_accepts_only_supported_mime_matching_magic_and_strict_base64() {
        for (mime, bytes) in [
            ("image/png", png()),
            ("image/jpeg", b"\xff\xd8\xff\xe0example".to_vec()),
            ("image/gif", b"GIF89aexample".to_vec()),
            ("image/webp", b"RIFF\x10\0\0\0WEBPVP8 example".to_vec()),
        ] {
            let value = format!("data:{mime};base64,{}", STANDARD.encode(bytes));
            validate_social_data_urls(&[value]).unwrap();
        }
        for value in [
            "https://multimedia.nt.qq.com.cn/example".to_owned(),
            "file:///tmp/example.png".to_owned(),
            "data:image/svg+xml;base64,PHN2Zz4=".to_owned(),
            "data:image/png;charset=utf-8;base64,aGVsbG8=".to_owned(),
            "data:image/png;base64,".to_owned(),
            "data:image/png;base64,invalid!".to_owned(),
            format!("data:image/jpeg;base64,{PNG_BASE64}"),
            format!("data:image/png;base64,{PNG_BASE64}\n"),
        ] {
            assert!(validate_social_data_urls(&[value]).is_err());
        }
    }

    #[test]
    fn data_url_decoded_bytes_obey_per_image_and_total_budget() {
        let mut bytes = png();
        bytes.resize(MAX_IMAGE_BYTES, 0);
        let at_limit = format!("data:image/png;base64,{}", STANDARD.encode(&bytes));
        validate_social_data_urls(&vec![at_limit; MAX_IMAGES]).unwrap();
        bytes.push(0);
        let over_limit = format!("data:image/png;base64,{}", STANDARD.encode(bytes));
        assert_eq!(
            validate_social_data_urls(&[over_limit])
                .unwrap_err()
                .to_string(),
            "image_too_large"
        );
    }

    #[tokio::test]
    async fn mock_download_returns_original_bytes_with_sniffed_mime_not_attachment_metadata() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/image"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(png(), "image/png; charset=binary"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut source = image();
        source.content_type = "image/jpeg".into();
        let result = from_test_endpoints(&[source], &[format!("{}/image", server.uri())])
            .await
            .unwrap();
        assert_eq!(result, [format!("data:image/png;base64,{PNG_BASE64}")]);
        validate_social_data_urls(&result).unwrap();
    }

    #[tokio::test]
    async fn redirect_is_rejected_without_following_or_retrying() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/redirect"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/unexpected", server.uri())),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/unexpected"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(png(), "image/png"))
            .expect(0)
            .mount(&server)
            .await;
        let error = from_test_endpoints(&[image()], &[format!("{}/redirect", server.uri())])
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "image_redirect_rejected");
    }

    #[tokio::test]
    async fn declared_and_chunked_bodies_over_limit_are_rejected() {
        let server = MockServer::start().await;
        for (endpoint, chunked) in [("/declared", false), ("/chunked", true)] {
            let mut body = png();
            body.resize(MAX_IMAGE_BYTES + 1, 0);
            let mut response = ResponseTemplate::new(200).set_body_raw(body, "image/png");
            if chunked {
                response = response.insert_header("Transfer-Encoding", "chunked");
            }
            Mock::given(method("GET"))
                .and(path(endpoint))
                .respond_with(response)
                .expect(1)
                .mount(&server)
                .await;
            let error = from_test_endpoints(&[image()], &[format!("{}{endpoint}", server.uri())])
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), "image_too_large");
        }
    }

    #[tokio::test]
    async fn mime_mismatch_unknown_format_and_encoding_are_rejected_without_echoing_body() {
        let server = MockServer::start().await;
        for (endpoint, response, code) in [
            (
                "/mismatch",
                ResponseTemplate::new(200).set_body_raw(png(), "image/jpeg"),
                "image_mime_mismatch",
            ),
            (
                "/unsupported-mime",
                ResponseTemplate::new(200).set_body_raw(png(), "application/octet-stream"),
                "image_content_type_invalid",
            ),
            (
                "/unsupported-format",
                ResponseTemplate::new(200).set_body_raw("EXAMPLE_PRIVATE_RESPONSE", "image/png"),
                "image_format_unsupported",
            ),
            (
                "/encoded",
                ResponseTemplate::new(200)
                    .set_body_raw(png(), "image/png")
                    .insert_header("Content-Encoding", "gzip"),
                "image_encoding_unsupported",
            ),
            (
                "/error",
                ResponseTemplate::new(403).set_body_string("EXAMPLE_PRIVATE_RESPONSE"),
                "image_http_rejected",
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(endpoint))
                .respond_with(response)
                .expect(1)
                .mount(&server)
                .await;
            let error = from_test_endpoints(&[image()], &[format!("{}{endpoint}", server.uri())])
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), code);
            assert!(!error.to_string().contains("EXAMPLE_PRIVATE"));
        }
    }

    #[tokio::test]
    async fn whole_source_set_is_validated_before_the_first_download() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(png(), "image/png"))
            .expect(0)
            .mount(&server)
            .await;
        let invalid = RemoteImage {
            url: "http://127.0.0.1/not-a-qq-image".into(),
            content_type: "image/png".into(),
        };
        let error = from_test_endpoints(&[image(), invalid], &[server.uri(), server.uri()])
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "image_source_invalid");
    }
}
