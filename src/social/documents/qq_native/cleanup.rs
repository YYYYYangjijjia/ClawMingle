//! The native delete endpoint can delete an owner's document, not merely unlink
//! it. Only already-trashed, host-managed documents may use this operation.
use super::*;

pub(super) fn managed_registration(value: &Value) -> bool {
    value["managed_registration"] == true
        || (value.get("managed_registration").is_none()
            && matches!(value["status"].as_str(), Some("registered" | "unknown"))
            && value["verification"] == "last_registration_ack_not_live_list")
}

impl Registrar {
    /// Host-only operation. The enclosing group operation lock must be held.
    /// `documents` is the trusted provider transport, never model-supplied state.
    /// A new provider read is mandatory even when a local trash annotation exists.
    pub async fn cleanup_trashed(
        &self,
        root: &Path,
        file: &str,
        documents: &Remote,
    ) -> Result<Value> {
        ensure!(valid_id(file), "documents_id_invalid");
        let mut record = self.status(root, file)?;
        ensure!(
            self.known_registration(root, file)? && managed_registration(&record),
            "documents_native_cleanup_unmanaged"
        );

        let present = self.list_contains(file).await?;
        // Do this after the list walk, immediately before any possible write.
        check_trashed(documents, file).await?;
        record["managed_registration"] = json!(true);
        record["document_status"] = json!("trash");
        record["document_status_checked_at"] = json!(chrono::Utc::now().timestamp());
        if !present {
            return self.save_cleanup(root, file, record, "removed", Some(false));
        }
        if matches!(
            record["cleanup_status"].as_str(),
            Some("unknown" | "removed")
        ) {
            // A prior POST may still be in flight, or someone may have re-added
            // an entry. Presence is not proof that retrying a write is safe.
            return self.save_cleanup(root, file, record, "unknown", Some(true));
        }

        record = self.save_cleanup(root, file, record, "unknown", Some(true))?;
        // Cancellation after this durable point never authorizes another POST.
        let response = (self.remote)("cleanup_trashed".into(), json!({"file_id":file})).await;
        let rejected = response
            .as_ref()
            .ok()
            .and_then(|value| value["retcode"].as_i64())
            .is_some_and(|code| code != 0);
        match self.list_contains(file).await {
            Ok(false) => {
                // Do not infer provider recycling from QQ list disappearance.
                if check_trashed(documents, file).await.is_ok() {
                    record["document_status_checked_at"] = json!(chrono::Utc::now().timestamp());
                    self.save_cleanup(root, file, record, "removed", Some(false))
                } else {
                    record["document_status"] = Value::Null;
                    self.save_cleanup(root, file, record, "unknown", Some(false))
                }
            }
            Ok(true) => self.save_cleanup(
                root,
                file,
                record,
                if rejected { "rejected" } else { "unknown" },
                Some(true),
            ),
            Err(_) => self.save_cleanup(root, file, record, "unknown", None),
        }
    }

    async fn list_contains(&self, file: &str) -> Result<bool> {
        let permission = (self.remote)("permission".into(), Value::Null)
            .await
            .map_err(list_remote_error)?;
        ensure!(
            group_allowed(&permission),
            "documents_native_permission_denied"
        );
        let mut start = 0;
        // Exhaust the bounded pagination before concluding absence. A missing
        // page, malformed cursor or transport failure is never an empty list.
        for _ in 0..=MAX_LIST_START / LIST_PAGE_SIZE as u64 {
            let raw = (self.remote)("list".into(), json!({"start":start}))
                .await
                .map_err(list_remote_error)?;
            let page = list_result(&raw, start)?;
            if page["entries"]
                .as_array()
                .expect("validated list")
                .iter()
                .any(|entry| entry["file_id"] == file)
            {
                return Ok(true);
            }
            match page["next_start"].as_u64() {
                Some(next) => start = next,
                None => return Ok(false),
            }
        }
        anyhow::bail!("documents_native_list_limit")
    }

    fn save_cleanup(
        &self,
        root: &Path,
        file: &str,
        mut record: Value,
        status: &str,
        present: Option<bool>,
    ) -> Result<Value> {
        record["cleanup_status"] = json!(status);
        record["cleanup_pending"] = json!(status != "removed");
        record["cleanup_checked_at"] = json!(chrono::Utc::now().timestamp());
        record["group_list_present"] = json!(present);
        record["cleanup_verification"] = json!(if present.is_some() {
            "live_group_list"
        } else {
            "readback_unavailable"
        });
        if status == "removed" {
            // Preserve a tombstone so register cannot silently recreate the link.
            record["status"] = json!("removed");
        }
        record["message"] = json!(match status {
            "removed" => "已核验 QQ 群在线文档入口移除；原文档仍在回收站，没有创建副本。",
            "rejected" => "腾讯拒绝清理，QQ 群入口仍存在；请检查权限后再重试。",
            _ => "群入口清理结果尚未完全确认；请再次查询核验，勿重复提交删除。",
        });
        atomic_write(&self.path(root, file), &record.to_string())?;
        Ok(record)
    }
}

async fn check_trashed(documents: &Remote, file: &str) -> Result<()> {
    let info = documents("manage.query_file_info".into(), json!({"file_id":file}))
        .await
        .map_err(|_| anyhow::anyhow!("documents_native_cleanup_status_unavailable"))?;
    ensure!(
        info["status"] == "trash",
        "documents_native_cleanup_requires_trash"
    );
    Ok(())
}

pub(super) fn delete_form(
    group: &str,
    args: &Value,
    session: &Cookies,
) -> Result<Vec<(&'static str, String)>> {
    let file = args["file_id"]
        .as_str()
        .filter(|file| valid_id(file))
        .ok_or_else(|| anyhow::anyhow!("documents_id_invalid"))?;
    // Verified official group-c46016.js.es5.js uses these exact form fields.
    // list_result already checks that this actual pad_id belongs to DOMAIN.
    Ok(vec![
        ("group_code", group.to_owned()),
        ("domain_id", DOMAIN.to_string()),
        ("pad_id", file.to_owned()),
        ("save_back", "false".into()),
        ("t", chrono::Utc::now().timestamp_millis().to_string()),
        ("xsrf", session.xsrf.clone()),
        ("bkn", session.bkn.clone()),
    ])
}

#[cfg(test)]
mod tests;
