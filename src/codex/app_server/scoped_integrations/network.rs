//! Pin checked public DNS addresses and verify the actual peer. Automatic
//! redirects and proxies are disabled; each caller owns its header policy and
//! must validate any explicitly followed redirect as a new public connection.
use super::super::social_tools::{public_ip, public_url};
use anyhow::{Result, ensure};
use reqwest::{Client, Response, Url};
use std::{net::SocketAddr, time::Duration};

#[derive(Debug, thiserror::Error)]
#[error("integration_response_too_large")]
pub(super) struct ResponseTooLarge;

pub(crate) struct PublicConnection {
    pub client: Client,
    pub url: Url,
    addresses: Vec<SocketAddr>,
}
impl PublicConnection {
    pub async fn new(raw: &str) -> Result<Self> {
        let url = public_url(raw).map_err(|_| anyhow::anyhow!("public_url_invalid"))?;
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("host_missing"))?;
        let addresses: Vec<_> =
            tokio::net::lookup_host((host.trim_start_matches('[').trim_end_matches(']'), 443))
                .await?
                .collect();
        ensure!(
            !addresses.is_empty()
                && addresses.len() <= 32
                && addresses.iter().all(|a| public_ip(a.ip())),
            "public_address_required"
        );
        let client = Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(12))
            .connect_timeout(Duration::from_secs(5))
            .resolve_to_addrs(host, &addresses)
            .build()?;
        Ok(Self {
            client,
            url,
            addresses,
        })
    }
    pub fn verify_peer(&self, response: &Response) -> Result<()> {
        ensure!(
            response
                .remote_addr()
                .is_some_and(|a| self.addresses.iter().any(|checked| checked.ip() == a.ip())),
            "public_peer_mismatch"
        );
        Ok(())
    }
    pub fn verify(&self, response: &Response) -> Result<()> {
        self.verify_peer(response)?;
        ensure!(
            !response.status().is_redirection(),
            "public_redirect_denied"
        );
        Ok(())
    }
}
pub(crate) async fn bounded_body(mut response: Response, limit: usize) -> Result<Vec<u8>> {
    ensure!(
        response.content_length().is_none_or(|n| n <= limit as u64),
        ResponseTooLarge
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(bytes.len() + chunk.len() <= limit, ResponseTooLarge);
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

// Only provider-returned COS endpoints are eligible for document bytes. Never
// accept model-selected upload endpoints, headers, cookies or bearer tokens.
fn document_transfer_url(raw: &str) -> Result<Url> {
    let url = public_url(raw).map_err(|_| anyhow::anyhow!("documents_transfer_url_invalid"))?;
    let host = url.host_str().unwrap_or_default();
    let labels = host.split('.').collect::<Vec<_>>();
    ensure!(
        matches!(labels.len(), 4 | 5)
            && labels[labels.len() - 2..] == ["myqcloud", "com"]
            && labels[labels.len() - 4] == "cos"
            && labels[labels.len() - 3].len() >= 3
            && labels[labels.len() - 3].contains('-')
            && labels.iter().all(|label| !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')),
        "documents_transfer_host_denied"
    );
    Ok(url)
}

pub(crate) async fn document_upload(raw: &str, bytes: Vec<u8>) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= 2 * 1024 * 1024,
        "documents_transfer_size_limit"
    );
    document_transfer_url(raw)?;
    let operation = async {
        let connection = PublicConnection::new(raw).await?;
        let response = connection
            .client
            .put(connection.url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes)
            .send()
            .await?;
        connection.verify(&response)?;
        ensure!(response.status().is_success(), "document_upload_rejected");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    // reqwest errors can contain the complete signed URL. Never retain them as
    // context, return them to the model, or let a generic logger display them.
    operation.map_err(|_| anyhow::anyhow!("documents_upload_unconfirmed"))
}

pub(crate) async fn document_download(raw: &str) -> Result<Vec<u8>> {
    document_transfer_url(raw)?;
    let operation = async {
        let connection = PublicConnection::new(raw).await?;
        let response = connection.client.get(connection.url.clone()).send().await?;
        connection.verify(&response)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(anyhow::anyhow!(DocumentExportMissing));
        }
        ensure!(response.status().is_success(), "document_download_rejected");
        let bytes = bounded_body(response, 2 * 1024 * 1024).await?;
        ensure!(!bytes.is_empty(), "document_download_empty");
        Ok::<_, anyhow::Error>(bytes)
    }
    .await;
    operation.map_err(|error| {
        if error.is::<DocumentExportMissing>() {
            anyhow::anyhow!("documents_export_file_not_found")
        } else {
            anyhow::anyhow!("documents_download_failed")
        }
    })
}

#[derive(Debug, thiserror::Error)]
#[error("documents_export_file_not_found")]
struct DocumentExportMissing;

#[cfg(test)]
mod document_transfer_tests {
    use super::*;
    #[test]
    fn only_explicit_public_tencent_cos_shapes_are_eligible() {
        for url in [
            "https://cos.ap-guangzhou.myqcloud.com/import/object?signature=EXAMPLE",
            "https://docs-import-export-example.cos.ap-guangzhou.myqcloud.com/export/document",
        ] {
            assert!(document_transfer_url(url).is_ok());
        }
        for url in [
            "http://cos.ap-guangzhou.myqcloud.com/import/object",
            "https://cos.ap-guangzhou.myqcloud.com.evil.invalid/object",
            "https://docs.qq.com/object",
            "https://cos.myqcloud.com/object",
            "https://cos.ap-guangzhou.myqcloud.com@evil.invalid/object",
            "https://cos.ap-guangzhou.myqcloud.com:8443/object",
            "https://cos.ap-guangzhou.myqcloud.com/object#fragment",
            "https://127.0.0.1/object",
            "https://bucket.other.ap-guangzhou.myqcloud.com/object",
        ] {
            assert!(document_transfer_url(url).is_err());
        }
        let mut with_credentials =
            Url::parse("https://cos.ap-guangzhou.myqcloud.com/object").unwrap();
        with_credentials.set_username("YOUR_USERNAME").unwrap();
        with_credentials
            .set_password(Some("YOUR_PASSWORD"))
            .unwrap();
        assert!(document_transfer_url(with_credentials.as_str()).is_err());
    }
    #[tokio::test]
    async fn invalid_transfer_requests_do_not_reach_network_or_expose_urls() {
        let raw = "https://example.invalid/object?signature=EXAMPLE";
        let error = document_upload(raw, vec![1]).await.unwrap_err().to_string();
        assert_eq!(error, "documents_transfer_host_denied");
        assert!(!error.contains("EXAMPLE"));
        assert!(
            document_upload(raw, vec![0; 2 * 1024 * 1024 + 1])
                .await
                .is_err()
        );
        assert!(document_upload(raw, vec![]).await.is_err());
        assert!(document_download(raw).await.is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn body_budget_checks_declared_and_streamed_lengths() {
        for chunked in [false, true] {
            for length in [1024, 1025] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = [0; 4096];
                    let received = socket.read(&mut request).await.unwrap();
                    assert!(received > 0);
                    let framing = if chunked {
                        "Transfer-Encoding: chunked\r\n".to_owned()
                    } else {
                        format!("Content-Length: {length}\r\n")
                    };
                    let mut response =
                        format!("HTTP/1.1 200 OK\r\n{framing}Connection: close\r\n\r\n");
                    if chunked {
                        response.push_str(&format!("{length:x}\r\n"));
                    }
                    response.push_str(&"x".repeat(length));
                    if chunked {
                        response.push_str("\r\n0\r\n\r\n");
                    }
                    socket.write_all(response.as_bytes()).await.unwrap();
                });
                let response = Client::new()
                    .get(format!("http://{address}"))
                    .send()
                    .await
                    .unwrap();
                let result = bounded_body(response, 1024).await;
                if length == 1024 {
                    assert_eq!(result.unwrap().len(), length);
                } else {
                    assert!(result.unwrap_err().is::<ResponseTooLarge>());
                }
                server.await.unwrap();
            }
        }
    }
}
