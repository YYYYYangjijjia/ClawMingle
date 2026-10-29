//! Task folders are provider folders, not QQ group-list folder IDs. The local
//! index is only a routing hint; remote membership is checked before every use.
use super::*;

#[derive(Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Index {
    documents: Vec<Document>,
}
#[derive(Deserialize, Serialize)]
struct Document {
    task_id: String,
    document_key: String,
    file_id: String,
    title: String,
    #[serde(default)]
    pending_operation: String,
}

fn canonical(title: &str) -> String {
    title.split_whitespace().collect::<String>().to_lowercase()
}

impl Broker {
    fn index(&self) -> Result<Index> {
        let path = self.root.join("workspace-index.json");
        if !path.exists() {
            return Ok(Index::default());
        }
        Ok(serde_json::from_str(&private_read_limit(
            &path,
            2 * 1024 * 1024,
        )?)?)
    }
    pub(super) fn remember_document(&self, r: &Request, file: &str) -> Result<()> {
        if r.document_key.is_empty() {
            return Ok(());
        }
        let mut index = self.index()?;
        index.documents.retain(|d| {
            !(d.file_id == file && d.task_id != self.folder)
                && !(d.task_id == self.folder && d.document_key == r.document_key)
        });
        ensure!(index.documents.len() < 10000, "documents_index_limit");
        index.documents.push(Document {
            task_id: self.folder.clone(),
            document_key: r.document_key.clone(),
            file_id: file.into(),
            title: r.title.clone(),
            pending_operation: String::new(),
        });
        atomic_write(
            &self.root.join("workspace-index.json"),
            &serde_json::to_string(&index)?,
        )
    }
    pub(super) fn reserve_document(&self, r: &Request) -> Result<()> {
        if r.document_key.is_empty() {
            return Ok(());
        }
        let mut index = self.index()?;
        if index
            .documents
            .iter()
            .any(|d| d.task_id == self.folder && d.document_key == r.document_key)
        {
            return Ok(());
        }
        ensure!(index.documents.len() < 10000, "documents_index_limit");
        index.documents.push(Document {
            task_id: self.folder.clone(),
            document_key: r.document_key.clone(),
            file_id: String::new(),
            title: r.title.clone(),
            pending_operation: r.operation_id.clone(),
        });
        atomic_write(
            &self.root.join("workspace-index.json"),
            &serde_json::to_string(&index)?,
        )
    }
    pub(super) async fn entries(&self) -> Result<Vec<Value>> {
        let mut start = 0;
        let mut entries = Vec::new();
        for _ in 0..20 {
            let page = self.list(start).await?;
            entries.extend(page["files"].as_array().unwrap().iter().cloned());
            if page["finished"] == true {
                return Ok(entries);
            }
            start = page["next_start"].as_u64().unwrap() as usize;
        }
        anyhow::bail!("documents_folder_too_large")
    }
    fn in_folder(&self, folder: &str) -> Self {
        Self {
            folder: folder.into(),
            root: self.root.clone(),
            remote: self.remote.clone(),
            workspace: self.workspace.clone(),
            registrar: self.registrar.clone(),
        }
    }
    pub(super) async fn task(&self, id: &str) -> Result<Self> {
        ensure!(valid_id(id) && id != self.folder, "documents_task_invalid");
        ensure!(
            self.entries()
                .await?
                .iter()
                .any(|e| e["id"] == id && e["is_folder"] == true),
            "documents_task_scope_denied"
        );
        Ok(self.in_folder(id))
    }
    async fn locate(&self, file: &str) -> Result<Self> {
        let entries = self.entries().await?;
        if entries
            .iter()
            .any(|e| e["id"] == file && e["is_folder"] == false)
        {
            return Ok(self.in_folder(&self.folder));
        }
        let mut folders: Vec<_> = entries
            .iter()
            .filter(|e| e["is_folder"] == true)
            .filter_map(|e| e["id"].as_str().filter(|s| valid_id(s)))
            .collect();
        ensure!(folders.len() <= 64, "documents_task_limit");
        let index = self.index()?;
        let hint = index
            .documents
            .iter()
            .find(|d| d.file_id == file)
            .map(|d| d.task_id.as_str());
        folders.sort_by_key(|id| Some(*id) != hint);
        for folder in folders {
            let scoped = self.in_folder(folder);
            if scoped
                .entries()
                .await?
                .iter()
                .any(|e| e["id"] == file && e["is_folder"] == false)
            {
                return Ok(scoped);
            }
        }
        anyhow::bail!("documents_scope_denied")
    }
    pub(super) async fn scope(&self, r: &Request) -> Result<Self> {
        if matches!(r.action.as_str(), "tasks" | "task_create" | "organize") {
            return Ok(self.in_folder(&self.folder));
        }
        if !r.task_id.is_empty() {
            return self.task(&r.task_id).await;
        }
        if matches!(r.action.as_str(), "import_progress" | "export_progress") {
            let path = self
                .root
                .join(format!("transfer-{}.json", hash(&r.source_operation_id)));
            let transfer: Value = serde_json::from_str(&private_read_limit(&path, 32768)?)?;
            let folder = transfer["folder"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("documents_transfer_scope_denied"))?;
            return if folder == self.folder {
                Ok(self.in_folder(folder))
            } else {
                self.task(folder).await
            };
        }
        if !r.file_id.is_empty()
            && let Some(doc) = self
                .index()?
                .documents
                .iter()
                .find(|d| d.file_id == r.file_id)
        {
            return self.task(&doc.task_id).await;
        }
        Ok(self.in_folder(&self.folder))
    }
    pub(super) async fn tasks(&self) -> Result<Value> {
        let entries = self.entries().await?;
        Ok(
            json!({"tasks":entries.iter().filter(|e|e["is_folder"]==true).collect::<Vec<_>>(),
            "unorganized_documents":entries.iter().filter(|e|e["is_folder"]==false).collect::<Vec<_>>(),
            "hierarchy":"Tencent Docs bound folder / task folder / documents",
            "qq_group_list_hierarchy":"not_verified",
            "next_step":"Reuse a task folder for continued requirements. Inspect existing documents before creating; organize can move a legacy document without changing its URL. QQ group-list registration is separate from these account folders."}),
        )
    }
    pub(super) async fn create_task(
        &self,
        r: &Request,
        journal: &Path,
        fingerprint: &str,
    ) -> Result<Value> {
        let entries = self.entries().await?;
        if let Some(existing) = entries.iter().find(|e| {
            e["is_folder"] == true
                && e["title"]
                    .as_str()
                    .is_some_and(|s| canonical(s) == canonical(&r.title))
        }) {
            return Ok(
                json!({"task_id":existing["id"],"title":existing["title"],"url":existing["url"],"reused":true}),
            );
        }
        ensure!(
            entries.iter().filter(|e| e["is_folder"] == true).count() < 64,
            "documents_task_limit"
        );
        let reservation = self
            .root
            .join(format!("task-create-{}.json", hash(&canonical(&r.title))));
        if reservation.exists() {
            let reserved: Value = serde_json::from_str(&private_read_limit(&reservation, 8192)?)?;
            let original = reserved["operation_id"]
                .as_str()
                .filter(|s| valid_id(s))
                .ok_or_else(|| anyhow::anyhow!("documents_operation_invalid"))?;
            if original != r.operation_id {
                return Ok(
                    json!({"status":"previous_operation_pending","operation_id":original,
                    "message":"同名任务目录已有未确认的创建操作。先核对原操作和目录，不要更换编号重复创建。"}),
                );
            }
        }
        atomic_write(
            &reservation,
            &json!({"operation_id":r.operation_id,"title":r.title}).to_string(),
        )?;
        save_operation(
            journal,
            fingerprint,
            &json!({"ok":false,"status":"unknown","operation_id":r.operation_id,
            "message":"任务目录创建结果尚未确认，请先核对目录，勿重复创建。"}),
        )?;
        let created = self
            .call(
                "manage.create_file",
                json!({"title":r.title,"file_type":"folder","parent_id":self.folder}),
            )
            .await?;
        let id = created["file_id"]
            .as_str()
            .filter(|s| valid_id(s))
            .ok_or_else(|| anyhow::anyhow!("documents_created_id_missing"))?;
        save_operation(
            journal,
            fingerprint,
            &json!({"ok":false,"status":"created_task_verify_unknown", "task_id":id,
            "message":"任务目录已创建，父目录核验尚未完成；保留此 ID，不要重复新建。"}),
        )?;
        self.task(id).await?;
        Ok(json!({"task_id":id,"title":r.title,"url":created["url"],"reused":false}))
    }
    pub(super) async fn reuse_document(&self, r: &Request) -> Result<Option<Value>> {
        let entries = self.entries().await?;
        let index = self.index()?;
        let key = index
            .documents
            .iter()
            .find(|d| d.task_id == self.folder && d.document_key == r.document_key);
        if let Some(saved) = key.filter(|d| d.file_id.is_empty()) {
            let path = self
                .root
                .join(format!("{}.json", hash(&saved.pending_operation)));
            let previous = if path.exists() {
                serde_json::from_str::<Value>(&private_read_limit(&path, 32 * 1024 * 1024 + 4096)?)?
                    ["result"]
                    .clone()
            } else {
                Value::Null
            };
            let partial = partial_result(&previous);
            if matches!(
                partial["status"].as_str(),
                Some("created_move_unknown" | "created_move_deferred")
            ) && let Some(entry) = entries
                .iter()
                .find(|e| e["id"] == partial["file_id"] && e["is_folder"] == false)
            {
                let file = entry["id"]
                    .as_str()
                    .filter(|s| valid_id(s))
                    .ok_or_else(|| anyhow::anyhow!("documents_id_invalid"))?;
                self.remember_document(r, file)?;
                let old: Value =
                    serde_json::from_str(&private_read_limit(&path, 32 * 1024 * 1024 + 4096)?)?;
                let fp = old["fingerprint"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("documents_operation_invalid"))?;
                save_operation(
                    &path,
                    fp,
                    &json!({"ok":true,"data":{"file_id":file,"url":entry["url"],"task_id":self.folder,
                        "recovered_by_membership":true,"qq_registration":"not_rechecked"},"untrusted_external_content":true}),
                )?;
                return Ok(Some(
                    json!({"status":"reuse_existing","file_id":file,"url":entry["url"],"task_id":self.folder,
                        "next_step":"Previous creation confirmed by fresh task-folder membership. No new content applied; read and update this document. Check registration separately."}),
                ));
            }
            if saved.pending_operation == r.operation_id && previous["status"] == "not_dispatched" {
                return Ok(None);
            }
            return Ok(Some(
                json!({"status":"previous_operation_pending","operation_id":saved.pending_operation,
                "previous_result":previous,"message":"此任务的同一文档已有创建或导入操作。先查询该 operation_id；不要更换操作号重复创建。"}),
            ));
        }
        let title = if r.title.is_empty() {
            r.arguments["title"].as_str().unwrap_or("")
        } else {
            &r.title
        };
        let existing = if let Some(saved) = key {
            Some(
                entries
                    .iter()
                    .find(|e| e["id"] == saved.file_id && e["is_folder"] == false)
                    .ok_or_else(|| {
                        anyhow::anyhow!("documents_previous_document_missing_inspect_first")
                    })?,
            )
        } else {
            entries.iter().find(|e| {
                e["is_folder"] == false
                    && !title.is_empty()
                    && e["title"]
                        .as_str()
                        .is_some_and(|s| canonical(s) == canonical(title))
            })
        };
        if let Some(entry) = existing {
            let file = entry["id"]
                .as_str()
                .filter(|s| valid_id(s))
                .ok_or_else(|| anyhow::anyhow!("documents_id_invalid"))?;
            self.remember_document(r, file)?;
            return Ok(Some(
                json!({"status":"reuse_existing","task_id":self.folder,"file_id":file,
                "title":entry["title"],"url":entry["url"],
                "next_step":"No new document was created and requested content was NOT applied. Read this existing document, then append or make a targeted edit. A genuinely separate deliverable needs a distinct document_key and title."}),
            ));
        }
        Ok(None)
    }
    pub(super) async fn organize(
        &self,
        r: &Request,
        journal: &Path,
        fingerprint: &str,
    ) -> Result<Value> {
        let source = self.locate(&r.file_id).await?;
        let dest = self.task(&r.task_id).await?;
        let entry = source.member(&r.file_id).await?;
        let info = source.metadata(&r.file_id, &entry).await?;
        if info["content_version_available"] == false {
            ensure!(
                !r.expected_title.is_empty() && info["title"] == r.expected_title,
                "documents_changed_read_again"
            );
        } else {
            ensure!(
                r.expected_modified_at.is_some() && modified_at(&info) == r.expected_modified_at,
                "documents_changed_read_again"
            );
        }
        if let Some(existing) = dest.reuse_document(r).await? {
            ensure!(
                existing["file_id"] == r.file_id,
                "documents_task_document_conflict"
            );
        }
        if source.folder != dest.folder {
            save_operation(
                journal,
                fingerprint,
                &json!({"ok":false,"status":"organize_unknown","file_id":r.file_id,
                "task_id":r.task_id,"message":"移动结果尚未确认；检查原目录和任务目录，不要重复移动。"}),
            )?;
            self.call(
                "manage.move_file",
                json!({"file_id":r.file_id,"target_folder_id":dest.folder}),
            )
            .await?;
        }
        let entry = dest.member(&r.file_id).await?;
        dest.remember_document(r, &r.file_id)?;
        Ok(
            json!({"file_id":r.file_id,"task_id":r.task_id,"url":entry["url"],"title":entry["title"],"identity_preserved":true}),
        )
    }

    pub(super) fn retain_input(&self, r: &Request, fingerprint: &str) -> Result<()> {
        let path = self
            .root
            .join(format!("draft-{}.json", hash(&r.operation_id)));
        if path.exists() {
            let text = private_read_limit(&path, 256 * 1024)?;
            let old: Value = serde_json::from_str(&text)?;
            if old["fingerprint"] == fingerprint {
                return Ok(());
            }
            // A failed preflight did not reserve a remote operation. Preserve
            // its draft too, while allowing corrected inputs before dispatch.
            ensure!(
                !self
                    .root
                    .join(format!("{}.json", hash(&r.operation_id)))
                    .exists(),
                "documents_operation_id_conflict"
            );
            atomic_write(
                &self.root.join(format!(
                    "draft-{}-{}.json",
                    hash(&r.operation_id),
                    hash(&text)
                )),
                &text,
            )?;
            let binary = self
                .root
                .join(format!("draft-{}.b64", hash(&r.operation_id)));
            if binary.exists() {
                atomic_write(
                    &self.root.join(format!(
                        "draft-{}-{}.b64",
                        hash(&r.operation_id),
                        hash(&text)
                    )),
                    &private_read_limit(&binary, 3 * 1024 * 1024)?,
                )?;
            }
        }
        let mut draft = json!({"fingerprint":fingerprint,"saved_at":chrono::Utc::now().timestamp(),"source":"ClawMingle","source_label":"来自 ClawMingle","request":r});
        if r.action == "import" {
            let bytes = super::super::media::read_workspace_bytes(&self.workspace, &r.file_path)?;
            ensure!(
                !bytes.is_empty() && bytes.len() <= 2 * 1024 * 1024,
                "documents_transfer_size_limit"
            );
            let target = self
                .root
                .join(format!("draft-{}.b64", hash(&r.operation_id)));
            use base64::Engine;
            atomic_write(
                &target,
                &base64::engine::general_purpose::STANDARD.encode(&bytes),
            )?;
            draft["import_bytes_retained"] = json!(true);
        }
        atomic_write(&path, &draft.to_string())
    }
    pub(super) fn retained_import(&self, operation: &str) -> Result<Vec<u8>> {
        use base64::Engine;
        let encoded = private_read_limit(
            &self.root.join(format!("draft-{}.b64", hash(operation))),
            3 * 1024 * 1024,
        )?;
        Ok(base64::engine::general_purpose::STANDARD.decode(encoded)?)
    }
    pub(super) fn draft(&self, operation: &str) -> Result<Value> {
        let path = self.root.join(format!("draft-{}.json", hash(operation)));
        let draft: Value = serde_json::from_str(&private_read_limit(&path, 256 * 1024)?)?;
        Ok(
            json!({"ok":true,"data":draft,"untrusted_external_content":true,
            "delivery_policy":"Retained operation input, not a complete backup of the cloud document. Do not send an attachment unless the user explicitly asks or accepts the fallback."}),
        )
    }
    pub(super) fn export_draft(&self, operation: &str) -> Result<Value> {
        let draft = self.draft(operation)?;
        let r: Request = serde_json::from_value(draft["data"]["request"].clone())?;
        let (bytes, extension) = if r.action == "import" {
            let extension = Path::new(&r.file_path)
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("bin");
            ensure!(
                !extension.is_empty()
                    && extension.len() <= 8
                    && extension.bytes().all(|b| b.is_ascii_alphanumeric()),
                "documents_export_name_invalid"
            );
            (self.retained_import(operation)?, extension.to_owned())
        } else if !r.content.is_empty() {
            (
                r.content.into_bytes(),
                if r.document_type == "flowchart" {
                    "mmd"
                } else {
                    "md"
                }
                .into(),
            )
        } else {
            (serde_json::to_vec_pretty(&r)?, "json".into())
        };
        let name = format!("draft-{}.{}", hash(operation), extension);
        let relative = format!(".clawmingle-document-exports/{name}");
        match super::super::media::read_workspace_bytes(&self.workspace, &relative) {
            Ok(previous) => ensure!(previous == bytes, "documents_export_changed_read_again"),
            Err(_) => transfers::write_export(&self.workspace, &name, &bytes)?,
        }
        Ok(
            json!({"ok":true,"data":{"file_path":relative,"bytes":bytes.len(),"retained_input_only":true},
            "message":"已恢复本次提交内容到工作区，未发送附件。只有用户明确要求或同意降级时才发送；JSON 是操作草稿，不要冒充完整原文档。"}),
        )
    }
    /// A saved version is a separate, best-effort checkpoint after content success.
    /// It does not change the author of automatic revisions or prove exclusive authorship.
    pub(super) async fn add_source_checkpoint(
        &self,
        r: &Request,
        entry: &Value,
        result: &mut Value,
    ) {
        let content_write = matches!(r.action.as_str(), "create" | "append" | "edit")
            || (r.action == "invoke" && native::tool(&r.tool).is_ok_and(|t| t["mutation"] == true));
        if !content_write
            || result["ok"] != true
            || matches!(
                result["data"]["status"].as_str(),
                Some("reuse_existing" | "pending" | "previous_operation_pending")
            )
        {
            return;
        }
        let creates = r.action == "create"
            || (r.action == "invoke" && native::tool(&r.tool).is_ok_and(|t| t["creates"] == true));
        let file = if creates {
            result["data"]["file_id"].as_str().unwrap_or("")
        } else {
            &r.file_id
        }
        .to_owned();
        let url = entry["url"]
            .as_str()
            .or_else(|| result["data"]["url"].as_str())
            .or_else(|| result["data"]["file_url"].as_str())
            .unwrap_or("");
        let ordinary_doc = reqwest::Url::parse("https://docs.qq.com")
            .unwrap()
            .join(url)
            .ok()
            .is_some_and(|u| {
                u.scheme() == "https"
                    && u.host_str() == Some("docs.qq.com")
                    && u.path().starts_with("/doc/")
            });
        let marker = if !ordinary_doc || !valid_id(&file) {
            json!({"status":"unsupported_type","message":"此文档类型的原生历史界面尚不支持已验证的来源标记；自动修订作者仍是登录账号。"})
        } else if let Some(registrar) = &self.registrar {
            match registrar
                .source_checkpoint(&self.root, &file, &r.operation_id)
                .await
            {
                Ok(value) => value,
                Err(_) => {
                    json!({"status":"unavailable","message":"正文操作已成功，原生来源版本未确认。不要重做正文操作。"})
                }
            }
        } else {
            json!({"status":"not_configured","message":"正文操作已成功；未配置独立腾讯网页登录，无法保存原生来源版本。"})
        };
        result["native_source_checkpoint"] = marker;
    }
    pub(super) fn revisions(&self, file: &str) -> Result<Value> {
        let mut rows = Vec::new();
        for item in std::fs::read_dir(&self.root)?.take(20000) {
            let item = item?;
            let name = item.file_name();
            let name = name.to_string_lossy();
            if name.len() != 51 || !name.starts_with("draft-") || !name.ends_with(".json") {
                continue;
            }
            let draft: Value =
                serde_json::from_str(&private_read_limit(&item.path(), 256 * 1024)?)?;
            let r: Request = serde_json::from_value(draft["request"].clone())?;
            let path = self.root.join(format!("{}.json", hash(&r.operation_id)));
            let result = if path.exists() {
                serde_json::from_str::<Value>(&private_read_limit(&path, 32 * 1024 * 1024 + 4096)?)?
                    ["result"]
                    .clone()
            } else {
                Value::Null
            };
            if r.file_id != file
                && result["data"]["file_id"] != file
                && partial_result(&result)["file_id"] != file
            {
                continue;
            }
            rows.push(json!({"operation_id":r.operation_id,"source":"来自 ClawMingle","submitted_at":draft["saved_at"],
                "action":r.action,"tool":r.tool,"confirmed":result["ok"]==true,
                "source_operation_id":r.source_operation_id,"native_source_checkpoint":result["native_source_checkpoint"],
                "content_applied":result["ok"]==true && !matches!(result["data"]["status"].as_str(),Some("reuse_existing"|"previous_operation_pending"|"pending")) && !matches!(r.action.as_str(),"export"|"export_progress"|"register"|"qq_cleanup"|"retention"|"organize"|"rename"|"task_create"),
                "status":if result["ok"]==true {json!("completed")} else {result["status"].clone()},
                "provider_version":result["data"].get("new_version").or_else(||result["data"].get("version")).cloned().unwrap_or(Value::Null)}));
        }
        rows.sort_by_key(|v| std::cmp::Reverse(v["submitted_at"].as_i64().unwrap_or(0)));
        let more = rows.len() > 100;
        rows.truncate(100);
        Ok(
            json!({"records":rows,"has_more":more,"source":"local_operation_journal",
            "native_revision_note_supported":false,
            "message":"本地工具操作记录，不能改写腾讯原生修订记录的作者；其中未确认或失败的操作不代表文档修改成功。"}),
        )
    }
    pub(super) async fn resume_created(&self, operation: &str) -> Result<Value> {
        let draft = self.draft(operation)?;
        let r: Request = serde_json::from_value(draft["data"]["request"].clone())?;
        let journal = self.root.join(format!("{}.json", hash(operation)));
        let saved: Value =
            serde_json::from_str(&private_read_limit(&journal, 32 * 1024 * 1024 + 4096)?)?;
        if saved["result"]["ok"] == true {
            return Ok(saved["result"].clone());
        }
        let partial = partial_result(&saved["result"]);
        ensure!(
            matches!(
                partial["status"].as_str(),
                Some("created_move_unknown" | "created_move_deferred")
            ),
            "documents_resume_inspect_first"
        );
        let file = partial["file_id"]
            .as_str()
            .filter(|s| valid_id(s))
            .ok_or_else(|| anyhow::anyhow!("documents_id_invalid"))?;
        let fingerprint = saved["fingerprint"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("documents_operation_invalid"))?;
        let scoped = self.scope(&r).await?;
        let created = json!({"file_id":file,"url":partial["url"]});
        let result = match scoped.member(file).await {
            Ok(_) => {
                scoped
                    .confirm_created(created, &r, &journal, fingerprint)
                    .await
            }
            Err(e)
                if e.to_string() == "documents_scope_denied"
                    && partial["status"] == "created_move_deferred" =>
            {
                scoped
                    .finish_created(created, &r, &journal, fingerprint)
                    .await
            }
            Err(e) if e.to_string() == "documents_scope_denied" => {
                return Ok(saved["result"].clone());
            }
            Err(e) => return Err(e),
        };
        match result {
            Ok(data) => {
                let result = json!({"ok":true,"data":data,"untrusted_external_content":true});
                save_operation(&journal, fingerprint, &result)?;
                Ok(result)
            }
            Err(_) => Ok(serde_json::from_str::<Value>(&private_read_limit(
                &journal,
                32 * 1024 * 1024 + 4096,
            )?)?["result"]
                .clone()),
        }
    }
}

fn partial_result(value: &Value) -> &Value {
    if value["error"] == "documents_rate_limited" && !value["previous_result"].is_null() {
        &value["previous_result"]
    } else {
        value
    }
}

#[cfg(test)]
mod tests;
