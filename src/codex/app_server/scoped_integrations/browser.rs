//! Browser child has no network namespace access. The only network is this
//! bounded parent GET bridge; Chromium cannot bypass it via DNS, sockets, workers
//! or service workers. No host filesystem or browser profiles are mounted.
use super::super::{
    social_tools::public_url,
    workspace_runtime::{append_runtime_mounts, validated_native_binary},
};
use super::{
    BrowserRuntime, error,
    network::{PublicConnection, bounded_body},
};
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{Semaphore, watch},
};
const RESOURCE_LIMIT: usize = 1024 * 1024;
const TOTAL_LIMIT: usize = 6 * 1024 * 1024;
static SLOT: Semaphore = Semaphore::const_new(1);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    url: String,
    #[serde(default = "default_wait")]
    wait_ms: u64,
    #[serde(default)]
    screenshot: bool,
}
fn default_wait() -> u64 {
    1200
}
fn parse(raw: &str) -> Result<Arguments> {
    let args: Arguments = serde_json::from_str(raw)?;
    ensure!(args.wait_ms <= 5000, "invalid_wait");
    public_url(&args.url).map_err(|_| anyhow::anyhow!("invalid_url"))?;
    Ok(args)
}

pub(super) async fn run(
    runtime: &BrowserRuntime,
    raw: &str,
    mut cancel: watch::Receiver<bool>,
    mut revoked: watch::Receiver<bool>,
) -> Value {
    let args = match parse(raw) {
        Ok(v) => v,
        Err(_) => return error("invalid_arguments"),
    };
    let _slot = match SLOT.try_acquire() {
        Ok(v) => v,
        Err(_) => return error("browser_busy"),
    };
    let mut command = match build_command(runtime) {
        Ok(v) => v,
        Err(_) => return error("browser_runtime_unavailable"),
    };
    if *revoked.borrow() || *cancel.borrow() || cancel.has_changed().is_err() {
        return error("cancelled");
    }
    let mut child = match command.spawn() {
        Ok(v) => v,
        Err(_) => return error("browser_spawn_failed"),
    };
    let outcome = async {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("browser_pipe_missing"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("browser_pipe_missing"))?;
        stdin
            .write_all(
                format!(
                    "{}\n",
                    json!({"url":args.url,"wait_ms":args.wait_ms,"screenshot":args.screenshot})
                )
                .as_bytes(),
            )
            .await?;
        let mut reader = BufReader::new(stdout);
        let mut resources = 0usize;
        let mut bytes = 0usize;
        let mut denied = 0usize;
        let mut seen = std::collections::BTreeSet::new();
        loop {
            let line = bounded_line(&mut reader).await?;
            let value: Value = serde_json::from_slice(&line)?;
            match value.get("type").and_then(Value::as_str) {
                Some("fetch") => {
                    resources += 1;
                    ensure!(resources <= 48, "browser_resource_limit");
                    let id = value
                        .get("id")
                        .and_then(Value::as_u64)
                        .filter(|id| *id > 0)
                        .ok_or_else(|| anyhow::anyhow!("browser_protocol_invalid"))?;
                    ensure!(seen.insert(id), "browser_protocol_invalid");
                    let url = value
                        .get("url")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("browser_protocol_invalid"))?;
                    let result = if value.get("method").and_then(Value::as_str) == Some("GET")
                        && bytes < TOTAL_LIMIT
                    {
                        fetch_resource(url, (TOTAL_LIMIT - bytes).min(RESOURCE_LIMIT)).await
                    } else {
                        Err(anyhow::anyhow!("browser_request_denied"))
                    };
                    let response = match result {
                        Ok((status, content_type, body)) => {
                            bytes += body.len();
                            json!({"type":"fetch_result","id":id,"ok":true,"status":status,"content_type":content_type,"body":STANDARD.encode(body)})
                        }
                        Err(_) => {
                            denied += 1;
                            json!({"type":"fetch_result","id":id,"ok":false})
                        }
                    };
                    stdin.write_all(format!("{response}\n").as_bytes()).await?;
                }
                Some("result") => {
                    ensure!(
                        value.get("ok") == Some(&json!(true)),
                        "browser_render_failed"
                    );
                    let mut result = validate_result(value, args.screenshot)?;
                    result["resources_fetched"] = json!(resources - denied);
                    result["resources_denied"] = json!(denied);
                    result["bytes_received"] = json!(bytes);
                    result["untrusted_external_content"] = json!(true);
                    return Ok::<Value, anyhow::Error>(result);
                }
                _ => anyhow::bail!("browser_protocol_invalid"),
            }
        }
    };
    let result = tokio::select! {biased;_=cancel.changed()=>error("cancelled"),_=revoked.changed()=>error("cancelled"), outcome=tokio::time::timeout(Duration::from_secs(30),outcome)=>match outcome{Ok(Ok(value))=>value,Ok(Err(_))=>error("browser_read_failed"),Err(_)=>error("browser_timeout")}};
    // Killing bwrap's namespace init terminates all browser descendants. Reap the
    // exact child before releasing the only browser slot, including on success.
    let _ = child.start_kill();
    let _ = child.wait().await;
    result
}
async fn bounded_line<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let bytes = reader.fill_buf().await?;
        ensure!(!bytes.is_empty(), "browser_pipe_closed");
        let end = bytes.iter().position(|b| *b == b'\n').map(|n| n + 1);
        let count = end.unwrap_or(bytes.len());
        ensure!(
            line.len() + count <= 3 * 1024 * 1024,
            "browser_output_limit"
        );
        line.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if end.is_some() {
            return Ok(line);
        }
    }
}
async fn fetch_resource(raw: &str, limit: usize) -> Result<(u16, String, Vec<u8>)> {
    let mut url = reqwest::Url::parse(raw)?;
    url.set_fragment(None);
    let connection = PublicConnection::new(url.as_str()).await?;
    let response = connection
        .client
        .get(connection.url.clone())
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .header(reqwest::header::USER_AGENT, "ClawMingle-PublicBrowser/1.0")
        .send()
        .await?;
    connection.verify(&response)?;
    let status = response.status();
    ensure!(status.is_success(), "browser_resource_http_failed");
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    ensure!(allowed_type(&content_type), "browser_resource_type_denied");
    let body = bounded_body(response, limit).await?;
    Ok((status.as_u16(), content_type, body))
}
fn allowed_type(value: &str) -> bool {
    matches!(
        value,
        "text/html"
            | "text/plain"
            | "text/css"
            | "text/javascript"
            | "application/javascript"
            | "application/x-javascript"
            | "application/json"
            | "application/ld+json"
            | "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "image/svg+xml"
            | "image/avif"
            | "font/woff"
            | "font/woff2"
            | "application/font-woff"
    )
}
fn validate_result(mut value: Value, wants_image: bool) -> Result<Value> {
    ensure!(
        value
            .get("url")
            .and_then(Value::as_str)
            .is_some_and(|u| public_url(u).is_ok()),
        "browser_final_url_invalid"
    );
    ensure!(
        value
            .get("title")
            .and_then(Value::as_str)
            .is_some_and(|s| s.chars().count() <= 300)
            && value
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|s| s.chars().count() <= 24000),
        "browser_output_limit"
    );
    let links = value
        .get_mut("links")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow::anyhow!("browser_output_invalid"))?;
    ensure!(links.len() <= 40, "browser_output_limit");
    links.retain(|link| {
        link.get("url")
            .and_then(Value::as_str)
            .is_some_and(|u| public_url(u).is_ok())
            && link
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|s| s.chars().count() <= 120)
    });
    if let Some(image) = value.get("image") {
        ensure!(
            wants_image && image["mime_type"] == "image/png",
            "browser_image_unrequested"
        );
        let data = STANDARD.decode(
            image["data"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("browser_image_invalid"))?,
        )?;
        ensure!(
            data.len() <= 2 * 1024 * 1024 && data.starts_with(b"\x89PNG\r\n\x1a\n"),
            "browser_image_invalid"
        );
    }
    value.as_object_mut().unwrap().remove("type");
    Ok(value)
}
fn trusted_path(path: &Path, directory: bool) -> Result<std::path::PathBuf> {
    ensure!(path.is_absolute(), "browser_runtime_path_invalid");
    let resolved = path.canonicalize()?;
    let uid = fs::metadata("/proc/self")?.uid();
    for ancestor in resolved.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)?;
        ensure!(
            !metadata.file_type().is_symlink()
                && (metadata.uid() == 0 || metadata.uid() == uid)
                && metadata.permissions().mode() & 0o022 == 0,
            "browser_runtime_untrusted"
        );
    }
    let metadata = fs::metadata(&resolved)?;
    ensure!(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "browser_runtime_type_invalid"
    );
    Ok(resolved)
}
fn build_command(runtime: &BrowserRuntime) -> Result<Command> {
    let bubblewrap = validated_native_binary(&runtime.bubblewrap)?;
    let node = validated_native_binary(&runtime.node)?;
    let chromium = validated_native_binary(&runtime.chromium)?;
    ensure!(
        chromium.file_name().and_then(|s| s.to_str()) == Some("chrome"),
        "browser_chromium_invalid"
    );
    let worker = trusted_path(&runtime.worker, false)?;
    ensure!(
        fs::metadata(&worker)?.len() <= 64 * 1024,
        "browser_worker_invalid"
    );
    let modules = trusted_path(&runtime.node_modules, true)?;
    let playwright = trusted_path(&modules.join("playwright"), true)?;
    let core = trusted_path(&modules.join("playwright-core"), true)?;
    ensure!(
        playwright.join("index.mjs").is_file() && core.join("package.json").is_file(),
        "browser_runtime_missing"
    );
    let mut cmd = Command::new(bubblewrap);
    cmd.env_clear()
        .current_dir("/")
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    cmd.args([
        "--unshare-all",
        "--unshare-user",
        "--cap-drop",
        "ALL",
        "--new-session",
        "--die-with-parent",
    ]);
    append_runtime_mounts(&mut cmd)?;
    for path in [
        "/etc",
        "/runtime",
        "/runtime/node_modules",
        "/runtime/node_modules/playwright",
        "/runtime/node_modules/playwright-core",
        "/runtime/chromium",
        "/home",
        "/home/browser",
    ] {
        cmd.args(["--dir", path]);
    }
    cmd.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
    if Path::new("/etc/fonts").is_dir() {
        cmd.args(["--ro-bind", "/etc/fonts", "/etc/fonts"]);
    }
    for (source, target) in [
        (node.as_path(), "/runtime/node"),
        (worker.as_path(), "/runtime/worker.mjs"),
        (playwright.as_path(), "/runtime/node_modules/playwright"),
        (core.as_path(), "/runtime/node_modules/playwright-core"),
        (chromium.parent().unwrap(), "/runtime/chromium"),
    ] {
        cmd.arg("--ro-bind").arg(source).arg(target);
    }
    cmd.env("HOME", "/home/browser")
        .env("PATH", "/usr/bin")
        .env("TMPDIR", "/tmp")
        .env("LANG", "C.UTF-8");
    cmd.args([
        "--chdir",
        "/tmp",
        "--",
        "/runtime/node",
        "/runtime/worker.mjs",
    ]);
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_arguments_deny_private_urls_and_unbounded_waits() {
        for raw in [
            r#"{"url":"https://127.0.0.1/"}"#,
            r#"{"url":"file:///etc/passwd"}"#,
            r#"{"url":"https://example.com/","wait_ms":5001}"#,
            r#"{"url":"https://example.com/","script":"evil"}"#,
        ] {
            assert!(parse(raw).is_err());
        }
        assert!(parse(r#"{"url":"https://example.com/","screenshot":true}"#).is_ok());
    }
    #[test]
    fn result_drops_nonpublic_links_and_disallows_unrequested_images() {
        let value = json!({"type":"result","ok":true,"url":"https://example.com/","title":"Example","text":"Text","links":[{"text":"private","url":"http://localhost/"},{"text":"public","url":"https://example.org/"}]});
        let result = validate_result(value, false).unwrap();
        assert_eq!(result["links"].as_array().unwrap().len(), 1);
        let mut value = result;
        value["image"] = json!({"mime_type":"image/png","data":""});
        assert!(validate_result(value, false).is_err());
    }
    #[test]
    fn browser_resource_types_exclude_downloads() {
        assert!(allowed_type("application/javascript"));
        assert!(!allowed_type("application/octet-stream"));
        assert!(!allowed_type("application/pdf"));
    }
    #[tokio::test]
    async fn worker_lines_have_hard_limit() {
        let bytes = vec![b'x'; 3 * 1024 * 1024 + 1];
        assert!(bounded_line(&mut bytes.as_slice()).await.is_err());
    }
}
