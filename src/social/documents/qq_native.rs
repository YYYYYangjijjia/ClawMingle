//! QQ's native group-document relation, separate from Tencent home folders.
//! The browser session belongs to the operator; neither cookies nor actual QQ
//! group numbers enter model arguments or results. No ambiguous POST is retried.
use super::*;
use crate::codex::app_server::scoped_integrations::network::{PublicConnection, bounded_body};
use reqwest::header::{COOKIE, HeaderValue};

const API: &str = "https://docs.qq.com/cgi-bin/group_online_docs/";
const DOMAIN: u64 = 300000000;
const LIST_PAGE_SIZE: usize = 20;
const MAX_LIST_START: u64 = 10000;
// Expand only after verifying identity preservation against the live provider.
pub(super) const VERIFIED_TYPES: &[&str] = &[
    "doc",
    "smartcanvas",
    "sheet",
    "slide",
    "smartsheet",
    "form",
    "mind",
    "flowchart",
    "board",
];

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NativeConfig {
    pub enabled: bool,
    pub browser_state_file: Option<PathBuf>,
    pub groups: BTreeMap<String, String>,
}
impl NativeConfig {
    pub fn validate(&self, bindings: &BTreeMap<String, String>) -> Result<()> {
        ensure!(
            self.groups.len() <= 64
                && self
                    .groups
                    .iter()
                    .all(|(group, code)| bindings.contains_key(group) && valid_group(code))
                && self.groups.values().collect::<BTreeSet<_>>().len() == self.groups.len(),
            "documents_native_groups_invalid"
        );
        ensure!(
            !self.enabled
                || self
                    .browser_state_file
                    .as_ref()
                    .is_some_and(|p| p.is_absolute()),
            "documents_native_credential_required"
        );
        Ok(())
    }
    pub(super) fn registrar(&self, group: &str) -> Option<Registrar> {
        if !self.enabled {
            return None;
        }
        let group = self.groups.get(group)?.clone();
        let path = self.browser_state_file.clone()?;
        let bound_group = group.clone();
        let remote: Remote = Arc::new(move |action, args| {
            let group = bound_group.clone();
            let path = path.clone();
            Box::pin(async move {
                if action.starts_with("version_") {
                    super::native_versions::http(&path, &action, &args).await
                } else {
                    http(&path, &group, &action, &args).await
                }
            })
        });
        Some(Registrar { group, remote })
    }
}
fn valid_group(code: &str) -> bool {
    (5..=12).contains(&code.len()) && code.bytes().all(|c| c.is_ascii_digit())
}

#[derive(Clone)]
pub(super) struct Registrar {
    group: String,
    pub(super) remote: Remote,
}
fn state(status: &str) -> Value {
    json!({"status":status,"recorded_at":chrono::Utc::now().timestamp(),"verification":"last_registration_ack_not_live_list","message":match status {
        "registered"=>"已登记到 QQ 群在线文档；后续编辑仍使用原文档。",
        "unknown"=>"群登记结果尚未确认。请刷新 QQ 群在线文档核对，勿重复创建或登记。",
        "login_required"=>"文档已保留；请管理员在后台重新登录 QQ 群文档后调用 register。",
        "rejected"=>"腾讯未确认群登记；请检查登录账号和群权限，文档仍保留。",
        "unsupported_type"=>"此文档类型的 QQ 群登记尚未验证，仍可通过原链接使用。",
        "not_configured"=>"本群未开启 QQ 原生群文档登记，仍可通过原链接使用。",
        _=>"群登记尚未完成，文档仍保留。"
    }})
}
pub(super) fn unavailable() -> Value {
    state("not_configured")
}
impl Registrar {
    fn path(&self, root: &Path, file: &str) -> PathBuf {
        root.join(format!(
            "qq-{}.json",
            hash(&format!("{}:{file}", self.group))
        ))
    }
    pub fn status(&self, root: &Path, file: &str) -> Result<Value> {
        let path = self.path(root, file);
        if !path.exists() {
            return Ok(state("not_registered"));
        }
        Ok(serde_json::from_str(&private_read_limit(&path, 8192)?)?)
    }
    /// Only a persisted send attempt in this group establishes a known relation.
    /// Directory absence, an arbitrary model-supplied ID or another group's
    /// journal must not authorize a query outside the current document folder.
    pub fn known_registration(&self, root: &Path, file: &str) -> Result<bool> {
        ensure!(valid_id(file), "documents_id_invalid");
        Ok(matches!(
            self.status(root, file)?["status"].as_str(),
            Some("registered" | "unknown" | "removed")
        ))
    }
    /// Called only for a validated entry from this registrar's live QQ list.
    /// This proves the relation, but never grants document-body write access.
    pub fn observed_in_list(&self, root: &Path, file: &str) -> Result<Value> {
        ensure!(valid_id(file), "documents_id_invalid");
        let mut observed = self.status(root, file)?;
        // Preserve provenance before a list read replaces verification. Merely
        // seeing an unrelated account document never makes it managed by us.
        observed["managed_registration"] = json!(cleanup::managed_registration(&observed));
        observed["status"] = json!("registered");
        observed["verification"] = json!("live_group_list");
        observed["list_checked_at"] = json!(chrono::Utc::now().timestamp());
        if observed["document_status"] != "trash" {
            observed["message"] =
                json!("已在当前 QQ 群在线文档列表中核验此入口；文档本体状态需另行检查。");
        }
        atomic_write(&self.path(root, file), &observed.to_string())?;
        Ok(observed)
    }
    /// The caller must first obtain an explicit provider `status=trash` result.
    /// This is a local annotation, never a remote removal or proof of unlinking.
    pub fn mark_trashed(&self, root: &Path, file: &str) -> Result<Value> {
        ensure!(
            self.known_registration(root, file)?,
            "documents_scope_denied"
        );
        let mut previous = self.status(root, file)?;
        previous["document_status"] = json!("trash");
        previous["document_status_checked_at"] = json!(chrono::Utc::now().timestamp());
        if previous["cleanup_status"] != "removed" {
            previous["cleanup_pending"] = json!(true);
            previous["message"] = json!("文档已移入腾讯文档回收站；QQ 群列表入口尚未确认移除。");
        }
        // Retain registered/unknown so an old registration command cannot
        // silently re-add the trashed document or retry an uncertain POST.
        atomic_write(&self.path(root, file), &previous.to_string())?;
        Ok(previous)
    }
    pub async fn list(&self, start: u64) -> Result<Value> {
        ensure!(start <= MAX_LIST_START, "documents_input_limit");
        let permission = (self.remote)("permission".into(), Value::Null)
            .await
            .map_err(list_remote_error)?;
        ensure!(
            group_allowed(&permission),
            "documents_native_permission_denied"
        );
        let result = (self.remote)("list".into(), json!({"start":start}))
            .await
            .map_err(list_remote_error)?;
        list_result(&result, start)
    }
    pub async fn register(&self, root: &Path, file: &str, url: &str) -> Result<Value> {
        // The group operation lock is held across this separate relation journal.
        let previous = self.status(root, file)?;
        if matches!(
            previous["status"].as_str(),
            Some("registered" | "unknown" | "removed")
        ) {
            return Ok(previous);
        }
        if !verified_url(url) {
            return Ok(state("unsupported_type"));
        }
        let permission = match (self.remote)("permission".into(), Value::Null).await {
            Ok(v) => v,
            Err(e) => {
                return Ok(state(
                    if e.to_string() == "documents_native_login_required" {
                        "login_required"
                    } else {
                        "unavailable"
                    },
                ));
            }
        };
        if !group_allowed(&permission) {
            return Ok(state("rejected"));
        }
        let path = self.path(root, file);
        let mut pending = state("unknown");
        pending["managed_registration"] = json!(true);
        atomic_write(&path, &pending.to_string())?;
        // After this durable point, cancellation/timeout/parse errors remain
        // unknown across restarts and even new caller operation IDs.
        let mut result = match (self.remote)("register".into(), json!({"file_id":file})).await {
            Ok(v) => registration_result(&v, file),
            Err(_) => state("unknown"),
        };
        result["managed_registration"] = json!(true);
        atomic_write(&path, &result.to_string())?;
        Ok(result)
    }
}
fn list_remote_error(error: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(if error.to_string() == "documents_native_login_required" {
        "documents_native_login_required"
    } else {
        "documents_native_unavailable"
    })
}
fn list_result(value: &Value, start: u64) -> Result<Value> {
    ensure!(value["retcode"] == 0, "documents_native_list_rejected");
    let list = value["data"]["doc_list"]
        .as_array()
        .filter(|list| list.len() <= LIST_PAGE_SIZE)
        .ok_or_else(|| anyhow::anyhow!("documents_native_list_invalid"))?;
    let next = value["data"]["next"]
        .as_u64()
        .filter(|next| *next == 0 || (*next > start && *next <= MAX_LIST_START && !list.is_empty()))
        .ok_or_else(|| anyhow::anyhow!("documents_native_list_invalid"))?;
    let mut entries = Vec::with_capacity(list.len());
    let mut seen = BTreeSet::new();
    for entry in list {
        let meta = &entry["doc_meta_data"];
        let file = meta["doc_id"]["pad_id"]
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or_else(|| anyhow::anyhow!("documents_native_list_invalid"))?;
        let domain = &meta["doc_id"]["domain_id"];
        ensure!(
            domain.as_u64().or_else(|| domain.as_str()?.parse().ok()) == Some(DOMAIN)
                && seen.insert(file),
            "documents_native_list_invalid"
        );
        let title = meta["title"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("documents_native_list_invalid"))?
            .chars()
            .filter(|c| !c.is_control())
            .take(240)
            .collect::<String>();
        let raw_url = entry["doc_url"]
            .as_str()
            .filter(|url| url.len() <= 4096)
            .ok_or_else(|| anyhow::anyhow!("documents_native_list_invalid"))?;
        let mut url = reqwest::Url::parse("https://docs.qq.com")
            .expect("fixed URL")
            .join(raw_url)
            .map_err(|_| anyhow::anyhow!("documents_native_list_invalid"))?;
        ensure!(
            url.scheme() == "https"
                && matches!(url.host_str(), Some("docs.qq.com" | "10000.docs.qq.com"))
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none()
                && url
                    .path_segments()
                    .and_then(|mut p| p.next_back())
                    .is_some_and(|segment| url_file_matches(segment, file)),
            "documents_native_list_invalid"
        );
        url.set_query(None);
        url.set_fragment(None);
        let provider_status = meta["doc_status"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("documents_native_list_invalid"))?;
        entries.push(json!({"file_id":file,"title":title,"url":url.as_str(),"provider_status":provider_status}));
    }
    Ok(
        json!({"entries":entries,"next_start":if next==0 {Value::Null}else{json!(next)},
        "verification":"live_group_list","provider_status_interpreted":false}),
    )
}
fn url_file_matches(segment: &str, file: &str) -> bool {
    use base64::Engine;
    // Native list URLs use D + base64(pad_id), whereas MCP exposes pad_id.
    // Keep both known URL forms, but never accept a link to another file.
    segment == file
        || segment.strip_prefix('D').is_some_and(|encoded| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .is_ok_and(|decoded| decoded == file.as_bytes())
        })
}
fn list_query(group: &str, args: &Value, session: &Cookies) -> Result<Vec<(&'static str, String)>> {
    let start = args["start"]
        .as_u64()
        .filter(|start| *start <= MAX_LIST_START)
        .ok_or_else(|| anyhow::anyhow!("documents_input_limit"))?;
    Ok(vec![
        ("group_code", group.to_owned()),
        ("folder_id", "/".to_owned()),
        ("start", start.to_string()),
        ("limit", LIST_PAGE_SIZE.to_string()),
        ("bkn", session.bkn.clone()),
        ("xsrf", session.xsrf.clone()),
    ])
}
fn verified_url(raw: &str) -> bool {
    reqwest::Url::parse("https://docs.qq.com")
        .unwrap()
        .join(raw)
        .ok()
        .is_some_and(|u| {
            u.scheme() == "https"
                && u.host_str() == Some("docs.qq.com")
                && u.username().is_empty()
                && u.password().is_none()
                && u.port().is_none()
                && u.path().split('/').nth(1).is_some_and(|t| {
                    VERIFIED_TYPES.contains(&if t == "aio" { "smartcanvas" } else { t })
                })
        })
}
fn group_allowed(value: &Value) -> bool {
    let p = &value["data"]["get_rsp"];
    value["retcode"] == 0
        && p["cmduin_privilege"].as_u64().is_some_and(|v| v <= 2)
        && p["shutup_flag_me"].as_bool() == Some(false)
        && p["shutup_flag"].as_bool().is_some()
        && !(p["cmduin_privilege"] == 0 && p["shutup_flag"] == true)
}
fn registration_result(value: &Value, file: &str) -> Value {
    let doc = &value["data"]["doc_info"]["doc_meta_data"]["doc_id"];
    if value["retcode"] == 0
        && doc["pad_id"] == file
        && doc["domain_id"]
            .as_u64()
            .or_else(|| doc["domain_id"].as_str()?.parse().ok())
            == Some(DOMAIN)
    {
        state("registered")
    } else if value["retcode"].as_i64().is_some_and(|v| v != 0) {
        state("rejected")
    } else {
        state("unknown")
    }
}

pub(super) struct Cookies {
    pub(super) header: HeaderValue,
    pub(super) xsrf: String,
    pub(super) bkn: String,
}
#[cfg(test)]
fn cookies(raw: &str, now: f64) -> Result<Cookies> {
    cookies_for_path(raw, now, "/cgi-bin/group_online_docs/doc_list")
}
pub(super) fn cookies_for_path(raw: &str, now: f64, endpoint_path: &str) -> Result<Cookies> {
    ensure!(
        endpoint_path.starts_with('/'),
        "invalid_cookie_endpoint_path"
    );
    let value: Value = serde_json::from_str(raw)?;
    let list = value["cookies"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("invalid_cookies"))?;
    ensure!(list.len() <= 256, "cookie_limit");
    let mut accepted = Vec::new();
    for c in list {
        let domain = c["domain"].as_str().unwrap_or("");
        let path = c["path"].as_str().unwrap_or("");
        let expires = c["expires"].as_f64().unwrap_or(0.0);
        if !matches!(domain, "docs.qq.com" | ".docs.qq.com" | ".qq.com")
            || !(path.starts_with('/')
                && (path == endpoint_path
                    || endpoint_path
                        .strip_prefix(path)
                        .is_some_and(|tail| path.ends_with('/') || tail.starts_with('/'))))
            || !(expires == -1.0 || expires > now)
        {
            continue;
        }
        let name = c["name"].as_str().unwrap_or("");
        let val = c["value"].as_str().unwrap_or("");
        ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
                && val
                    .bytes()
                    .all(|b| (0x21..=0x7e).contains(&b) && !b";,\"\\".contains(&b)),
            "invalid_cookie"
        );
        accepted.push((path.len(), name, val));
    }
    accepted.sort_by_key(|c| std::cmp::Reverse(c.0));
    let get = |key| {
        accepted
            .iter()
            .find(|c| c.1 == key)
            .map(|c| c.2)
            .unwrap_or("")
    };
    let xsrf = get("TOK").to_owned();
    ensure!(!xsrf.is_empty(), "session_missing");
    let bkn = get("skey")
        .encode_utf16()
        .fold(5381u32, |n, c| n.wrapping_mul(33).wrapping_add(c.into()))
        & 0x7fff_ffff;
    let raw = accepted
        .iter()
        .map(|c| format!("{}={}", c.1, c.2))
        .collect::<Vec<_>>()
        .join("; ");
    ensure!(raw.len() <= 32768, "cookie_limit");
    let mut header = HeaderValue::from_str(&raw)?;
    header.set_sensitive(true);
    Ok(Cookies {
        header,
        xsrf,
        bkn: bkn.to_string(),
    })
}
async fn http(path: &Path, group: &str, action: &str, args: &Value) -> Result<Value> {
    use std::os::unix::fs::MetadataExt;
    let name = match action {
        "register" => "doc_add",
        "permission" => "group_info_get",
        "list" => "doc_list",
        "cleanup_trashed" => "doc_delete",
        _ => anyhow::bail!("documents_native_unavailable"),
    };
    let session = (|| {
        ensure!(
            std::fs::symlink_metadata(path)?.nlink() == 1,
            "private_session_required"
        );
        cookies_for_path(
            &private_read_limit(path, 512 * 1024)?,
            chrono::Utc::now().timestamp() as f64,
            &format!("/cgi-bin/group_online_docs/{name}"),
        )
    })()
    .map_err(|_| anyhow::anyhow!("documents_native_login_required"))?;
    let operation = async {
        ensure!(valid_group(group), "invalid_group");
        let endpoint = format!("{API}{name}");
        let connection = PublicConnection::new(&endpoint).await?;
        let request = if action == "register" {
            let file = args["file_id"]
                .as_str()
                .filter(|s| valid_id(s))
                .ok_or_else(|| anyhow::anyhow!("invalid_file"))?;
            connection.client.post(connection.url.clone()).form(&[
                ("group_code", group),
                ("pad_id", file),
                ("copy", "1"),
                ("domain_id", "300000000"),
                ("dont_add_recent", "1"),
                ("folder_id", "/"),
                ("xsrf", session.xsrf.as_str()),
            ])
        } else if action == "cleanup_trashed" {
            connection
                .client
                .post(connection.url.clone())
                .form(&cleanup::delete_form(group, args, &session)?)
        } else if action == "list" {
            connection
                .client
                .get(connection.url.clone())
                .query(&list_query(group, args, &session)?)
        } else {
            connection.client.get(connection.url.clone()).query(&[
                ("group_code", group),
                ("bkn", session.bkn.as_str()),
                ("xsrf", session.xsrf.as_str()),
            ])
        };
        let response = request
            .header(COOKIE, session.header)
            .header("Origin", "https://docs.qq.com")
            .header("Referer", "https://docs.qq.com/mall/index/doc")
            .send()
            .await?;
        connection.verify(&response)?;
        ensure!(response.status().is_success(), "http_rejected");
        Ok::<_, anyhow::Error>(serde_json::from_slice(
            &bounded_body(response, 256 * 1024).await?,
        )?)
    }
    .await;
    // Transport errors can carry xsrf in query strings; never preserve them.
    operation.map_err(|_| anyhow::anyhow!("documents_native_unavailable"))
}

mod cleanup;
#[cfg(test)]
mod list_tests;
#[cfg(test)]
mod tests;
