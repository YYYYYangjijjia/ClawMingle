//! Bounded document transfers. Provider task keys stay in private host state;
//! callers can only follow an operation created in this folder. Unknown writes
//! are never resumed automatically, including an ambiguous import move.
use super::*;
use crate::codex::app_server::scoped_integrations::{document_download, document_upload};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
};

const MAX_TRANSFER_BYTES: usize = 2 * 1024 * 1024;
const MAX_EXPORT_FILES: usize = 64;
const EXPORT_DIRECTORY: &str = ".clawmingle-document-exports";
const NOFOLLOW: i32 = 0o400000;
const DIRECTORY: i32 = 0o200000;

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Import,
    Export,
}
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    UploadUnknown,
    SubmitUnknown,
    SubmitDeferred,
    Pending,
    MoveUnknown,
    MoveDeferred,
    DownloadUnknown,
    Complete,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Transfer {
    operation_id: String,
    folder: String,
    kind: Kind,
    phase: Phase,
    task_id: String,
    file_key: String,
    file_id: String,
    file_name: String,
    file_url: Option<String>,
    output_path: Option<String>,
    output_md5: Option<String>,
    output_bytes: Option<usize>,
    #[serde(default)]
    input_md5: Option<String>,
    #[serde(default)]
    input_bytes: Option<usize>,
}
impl Transfer {
    fn new(operation: &str, folder: &str, kind: Kind, task: String) -> Self {
        Self {
            operation_id: operation.into(),
            folder: folder.into(),
            kind,
            phase: Phase::Pending,
            task_id: task,
            file_key: String::new(),
            file_id: String::new(),
            file_name: String::new(),
            file_url: None,
            output_path: None,
            output_md5: None,
            output_bytes: None,
            input_md5: None,
            input_bytes: None,
        }
    }
    fn result(&self) -> Value {
        let mut value = json!({"operation_id":self.operation_id,
            "status":if self.phase == Phase::Complete {"complete"} else {"pending"},
            "poll_after_seconds":5});
        if !self.file_id.is_empty() {
            value["file_id"] = json!(self.file_id);
        }
        if self.phase == Phase::Complete {
            value["progress"] = json!(100);
            if let Some(url) = &self.file_url {
                value["url"] = json!(url);
            }
            if let Some(path) = &self.output_path {
                value["file_path"] = json!(path);
                value["bytes"] = json!(self.output_bytes);
            }
        }
        value
    }
}

impl Broker {
    pub(super) async fn transfer(
        &self,
        r: &Request,
        journal: &Path,
        fingerprint: &str,
    ) -> Result<Value> {
        match r.action.as_str() {
            "import" => self.start_import(r, journal, fingerprint).await,
            "export" => self.start_export(r, journal, fingerprint).await,
            "import_progress" => self.poll_import(r, journal, fingerprint).await,
            "export_progress" => self.poll_export(r, journal, fingerprint).await,
            _ => anyhow::bail!("documents_action_invalid"),
        }
    }
    fn transfer_path(&self, operation: &str) -> PathBuf {
        self.root.join(format!("transfer-{}.json", hash(operation)))
    }
    fn save_transfer(&self, transfer: &Transfer) -> Result<()> {
        atomic_write(
            &self.transfer_path(&transfer.operation_id),
            &serde_json::to_string(transfer)?,
        )
    }
    fn load_transfer(&self, operation: &str, kind: Kind) -> Result<Transfer> {
        ensure!(
            valid_id(operation) && operation.len() <= 80,
            "documents_source_operation_required"
        );
        let path = self.transfer_path(operation);
        ensure!(path.exists(), "documents_transfer_not_found");
        let transfer: Transfer = serde_json::from_str(&private_read_limit(&path, 32768)?)?;
        ensure!(
            transfer.operation_id == operation
                && transfer.folder == self.folder
                && transfer.kind == kind,
            "documents_transfer_scope_denied"
        );
        Ok(transfer)
    }
    fn transfer_stage(
        &self,
        r: &Request,
        journal: &Path,
        fingerprint: &str,
        stage: &str,
    ) -> Result<()> {
        save_operation(
            journal,
            fingerprint,
            &json!({"ok":false,"status":"unknown",
            "operation_id":r.operation_id,"source_operation_id":r.source_operation_id,"stage":stage,
            "message":"传输操作结果尚未确认。请用原操作编号查询；不要更换编号自动重复导入或导出。"}),
        )
    }
    async fn start_import(&self, r: &Request, journal: &Path, fingerprint: &str) -> Result<Value> {
        self.retain_input(r, fingerprint)?;
        let bytes = self.retained_import(&r.operation_id)?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_TRANSFER_BYTES,
            "documents_transfer_size_limit"
        );
        let name = import_name(&r.file_path)?;
        let digest = format!("{:x}", md5::compute(&bytes));
        ensure!(
            !self.transfer_path(&r.operation_id).exists(),
            "documents_transfer_already_started"
        );
        self.transfer_stage(r, journal, fingerprint, "pre_import")?;
        self.reserve_document(r)?;
        let prepared = self
            .call(
                "manage.pre_import",
                json!({"file_name":name,
            "file_size":bytes.len(),"file_md5":digest}),
            )
            .await?;
        let task = provider_string(&prepared, "task_id")?;
        let key = provider_string(&prepared, "file_key")?;
        let upload_url = provider_string(&prepared, "upload_url")?;
        let mut transfer = Transfer::new(&r.operation_id, &self.folder, Kind::Import, task);
        transfer.file_name = name;
        transfer.file_key = key;
        transfer.input_bytes = Some(bytes.len());
        transfer.input_md5 = Some(digest);
        transfer.phase = Phase::UploadUnknown;
        self.save_transfer(&transfer)?;
        self.transfer_stage(r, journal, fingerprint, "upload")?;
        document_upload(&upload_url, bytes).await?;
        self.submit_import(&mut transfer, r, journal, fingerprint)
            .await
    }

    /// Resume only after a definitive refusal. Every dispatch is made ambiguous
    /// durably first, so cancellation or an unknown response can never replay it.
    async fn submit_import(
        &self,
        transfer: &mut Transfer,
        r: &Request,
        journal: &Path,
        fingerprint: &str,
    ) -> Result<Value> {
        let bytes = self.retained_import(&transfer.operation_id)?;
        let digest = format!("{:x}", md5::compute(&bytes));
        ensure!(
            !bytes.is_empty()
                && bytes.len() <= MAX_TRANSFER_BYTES
                && transfer.input_bytes == Some(bytes.len())
                && transfer.input_md5.as_deref() == Some(digest.as_str())
                && !transfer.file_key.is_empty(),
            "documents_retained_import_changed"
        );
        transfer.phase = Phase::SubmitUnknown;
        self.save_transfer(transfer)?;
        self.transfer_stage(r, journal, fingerprint, "async_import")?;
        // The provider can replace the pre-import task ID. Only the confirmed
        // async_import ID is eligible for polling; an ambiguous submit is not.
        let submitted = self
            .call(
                "manage.async_import",
                json!({"task_id":transfer.task_id,
            "file_key":transfer.file_key,"file_name":transfer.file_name,
            "file_size":bytes.len(),"file_md5":digest}),
            )
            .await;
        let submitted = match submitted {
            Ok(value) => value,
            Err(error) => {
                self.defer_import_step(
                    transfer,
                    r,
                    journal,
                    fingerprint,
                    Phase::SubmitDeferred,
                    &error,
                )?;
                return Err(error);
            }
        };
        transfer.task_id = provider_string(&submitted, "task_id")?;
        transfer.phase = Phase::Pending;
        transfer.file_key.clear();
        self.save_transfer(transfer)?;
        Ok(transfer.result())
    }

    fn defer_import_step(
        &self,
        transfer: &mut Transfer,
        r: &Request,
        journal: &Path,
        fingerprint: &str,
        deferred: Phase,
        error: &anyhow::Error,
    ) -> Result<()> {
        use crate::codex::app_server::scoped_integrations::{
            DocumentNotDispatched, DocumentRateLimited,
        };
        if error.is::<DocumentRateLimited>() || error.is::<DocumentNotDispatched>() {
            transfer.phase = deferred;
            self.save_transfer(transfer)?;
            let stage = if deferred == Phase::SubmitDeferred {
                "async_import"
            } else {
                "import_move"
            };
            save_operation(
                journal,
                fingerprint,
                &json!({"ok":false,"status":"transfer_deferred",
                "operation_id":r.operation_id,"source_operation_id":transfer.operation_id,
                "stage":stage,"file_id":transfer.file_id,
                "message":"导入的当前步骤已明确未执行，已保留上传和文档身份。待额度或连接恢复后，用原 source_operation_id 调用 import_progress 继续；不要重新导入。"}),
            )?;
        }
        Ok(())
    }
    async fn start_export(&self, r: &Request, journal: &Path, fingerprint: &str) -> Result<Value> {
        ensure!(valid_id(&r.file_id), "documents_id_invalid");
        ensure!(
            !self.transfer_path(&r.operation_id).exists(),
            "documents_transfer_already_started"
        );
        let entry = self.member(&r.file_id).await?;
        if r.expected_modified_at.is_some() {
            let info = self.metadata(&r.file_id, &entry).await?;
            ensure!(
                modified_at(&info) == r.expected_modified_at,
                "documents_changed_read_again"
            );
        }
        self.transfer_stage(r, journal, fingerprint, "export")?;
        let exported = self
            .call("manage.export_file", json!({"file_id":r.file_id}))
            .await?;
        let mut transfer = Transfer::new(
            &r.operation_id,
            &self.folder,
            Kind::Export,
            provider_string(&exported, "task_id")?,
        );
        transfer.file_id = r.file_id.clone();
        self.save_transfer(&transfer)?;
        Ok(transfer.result())
    }
    async fn poll_import(&self, r: &Request, journal: &Path, fingerprint: &str) -> Result<Value> {
        let mut transfer = self.load_transfer(&r.source_operation_id, Kind::Import)?;
        if transfer.phase == Phase::SubmitDeferred {
            return self
                .submit_import(&mut transfer, r, journal, fingerprint)
                .await;
        }
        if transfer.phase == Phase::MoveDeferred {
            return self
                .move_import(&mut transfer, r, journal, fingerprint)
                .await;
        }
        if transfer.phase == Phase::Complete {
            self.member(&transfer.file_id).await?;
            return self.finish_import(&transfer, journal, fingerprint).await;
        }
        if transfer.phase == Phase::MoveUnknown {
            // A previous move may have succeeded. Membership can prove success;
            // failing to find it never authorizes a second move.
            self.member(&transfer.file_id).await?;
            transfer.phase = Phase::Complete;
            self.save_transfer(&transfer)?;
            return self.finish_import(&transfer, journal, fingerprint).await;
        }
        ensure!(
            transfer.phase == Phase::Pending,
            "documents_transfer_unknown_inspect_first"
        );
        let response = self
            .call(
                "manage.import_progress",
                json!({"task_id":transfer.task_id}),
            )
            .await?;
        let progress = import_export_progress(&response)?;
        if progress < 100 {
            let mut result = transfer.result();
            result["progress"] = json!(progress);
            return Ok(result);
        }
        let file = provider_string(&response, "file_id")?;
        ensure!(
            valid_id(&file) && file != self.folder,
            "documents_created_id_invalid"
        );
        transfer.file_id = file;
        transfer.file_url = document_url(&response["file_url"]);
        self.move_import(&mut transfer, r, journal, fingerprint)
            .await
    }

    async fn move_import(
        &self,
        transfer: &mut Transfer,
        r: &Request,
        journal: &Path,
        fingerprint: &str,
    ) -> Result<Value> {
        transfer.phase = Phase::MoveUnknown;
        self.save_transfer(transfer)?;
        save_operation(
            journal,
            fingerprint,
            &json!({"ok":false,"status":"unknown",
            "operation_id":r.operation_id,"source_operation_id":r.source_operation_id,
            "stage":"import_move","file_id":transfer.file_id,
            "message":"导入已创建文档，移入群目录尚未确认。请先检查目录，不要重新导入或重复移动。"}),
        )?;
        let moved = self
            .call(
                "manage.move_file",
                json!({"file_id":transfer.file_id,"target_folder_id":self.folder}),
            )
            .await;
        if let Err(error) = moved {
            self.defer_import_step(
                transfer,
                r,
                journal,
                fingerprint,
                Phase::MoveDeferred,
                &error,
            )?;
            return Err(error);
        }
        self.member(&transfer.file_id).await?;
        transfer.phase = Phase::Complete;
        self.save_transfer(transfer)?;
        self.finish_import(transfer, journal, fingerprint).await
    }
    async fn finish_import(
        &self,
        transfer: &Transfer,
        journal: &Path,
        fingerprint: &str,
    ) -> Result<Value> {
        let mut result = transfer.result();
        // The original import input keeps the stable deliverable key across
        // later polling calls, which may belong to a different Codex turn.
        if self
            .root
            .join(format!("draft-{}.json", hash(&transfer.operation_id)))
            .exists()
        {
            let draft = self.draft(&transfer.operation_id)?;
            let original: Request = serde_json::from_value(draft["data"]["request"].clone())?;
            self.remember_document(&original, &transfer.file_id)?;
        }
        result["task_id"] = json!(self.folder);
        result["qq_registration"] = json!({"status":"not_completed","message":"导入已完成；用 registration 查询群登记状态。勿重复导入。"});
        save_operation(
            journal,
            fingerprint,
            &json!({"ok":true,"data":result,"untrusted_external_content":true}),
        )?;
        result["qq_registration"] = self
            .register(
                &transfer.file_id,
                transfer.file_url.as_deref().unwrap_or(""),
            )
            .await?;
        Ok(result)
    }
    async fn poll_export(&self, r: &Request, journal: &Path, fingerprint: &str) -> Result<Value> {
        let mut transfer = self.load_transfer(&r.source_operation_id, Kind::Export)?;
        self.member(&transfer.file_id).await?;
        if transfer.phase == Phase::Complete {
            verify_export(&self.workspace, &transfer)?;
            return Ok(transfer.result());
        }
        // A cancelled local write is not overwritten. Once the file is complete,
        // its digest can prove success even if the final state save was interrupted.
        if transfer.phase == Phase::DownloadUnknown {
            verify_export(&self.workspace, &transfer)?;
            transfer.phase = Phase::Complete;
            self.save_transfer(&transfer)?;
            return Ok(transfer.result());
        }
        ensure!(
            transfer.phase == Phase::Pending,
            "documents_transfer_unknown_inspect_first"
        );
        let response = self
            .call(
                "manage.export_progress",
                json!({"task_id":transfer.task_id}),
            )
            .await?;
        let progress = import_export_progress(&response)?;
        if progress < 100 {
            let mut result = transfer.result();
            result["progress"] = json!(progress);
            return Ok(result);
        }
        let url = provider_string(&response, "file_url")?;
        let bytes = document_download(&url).await?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_TRANSFER_BYTES,
            "documents_transfer_size_limit"
        );
        // Source access is checked again after network I/O, before publishing bytes.
        self.member(&transfer.file_id).await?;
        let extension = export_extension(&response, &url);
        let name = format!("export-{}.{}", hash(&transfer.operation_id), extension);
        transfer.output_path = Some(format!("/workspace/{EXPORT_DIRECTORY}/{name}"));
        transfer.output_md5 = Some(format!("{:x}", md5::compute(&bytes)));
        transfer.output_bytes = Some(bytes.len());
        transfer.phase = Phase::DownloadUnknown;
        self.save_transfer(&transfer)?;
        self.transfer_stage(r, journal, fingerprint, "export_local_write")?;
        write_export(&self.workspace, &name, &bytes)?;
        transfer.phase = Phase::Complete;
        self.save_transfer(&transfer)?;
        Ok(transfer.result())
    }
}

fn provider_string(value: &Value, field: &str) -> Result<String> {
    let text = value[field]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 8192 && !s.chars().any(char::is_control))
        .ok_or_else(|| anyhow::anyhow!("documents_transfer_response_invalid"))?;
    Ok(text.to_owned())
}
fn import_name(path: &str) -> Result<String> {
    let name = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| {
            s.chars().count() <= 128 && !s.chars().any(char::is_control) && !s.contains('\\')
        })
        .ok_or_else(|| anyhow::anyhow!("documents_import_name_invalid"))?;
    let extension = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    ensure!(
        !extension.is_empty()
            && extension.len() <= 12
            && extension.bytes().all(|b| b.is_ascii_alphanumeric()),
        "documents_import_extension_required"
    );
    Ok(name.to_owned())
}
fn import_export_progress(response: &Value) -> Result<u64> {
    ensure!(
        response["error"].is_null() || response["error"] == "",
        "documents_transfer_provider_failed"
    );
    ensure!(
        !matches!(
            response["status"].as_str(),
            Some("failed" | "error" | "FAILED" | "ERROR")
        ),
        "documents_transfer_provider_failed"
    );
    response["progress"]
        .as_u64()
        .filter(|p| *p <= 100)
        .ok_or_else(|| anyhow::anyhow!("documents_transfer_progress_invalid"))
}
fn document_url(value: &Value) -> Option<String> {
    let url = reqwest::Url::parse(value.as_str()?).ok()?;
    (url.scheme() == "https"
        && url.host_str() == Some("docs.qq.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none())
    .then(|| url.to_string())
}
fn export_extension(response: &Value, url: &str) -> String {
    let parsed = reqwest::Url::parse(url).ok();
    let name = response["file_name"]
        .as_str()
        .or_else(|| parsed.as_ref().map(|u| u.path()))
        .unwrap_or_default();
    let extension = Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        extension.as_str(),
        "doc"
            | "docx"
            | "xls"
            | "xlsx"
            | "ppt"
            | "pptx"
            | "pdf"
            | "txt"
            | "md"
            | "csv"
            | "zip"
            | "aipage"
    ) {
        extension
    } else {
        "bin".into()
    }
}
fn open_directory(path: &Path) -> Result<File> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(NOFOLLOW | DIRECTORY)
        .open(path)?;
    let meta = directory.metadata()?;
    ensure!(
        meta.is_dir()
            && meta.uid() == std::fs::metadata("/proc/self")?.uid()
            && meta.permissions().mode() & 0o777 == 0o700,
        "documents_export_directory_invalid"
    );
    Ok(directory)
}
fn child(directory: &File, name: &str) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name)
}
pub(super) fn write_export(workspace: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b)),
        "documents_export_name_invalid"
    );
    let workspace = open_directory(workspace)?;
    let location = child(&workspace, EXPORT_DIRECTORY);
    match std::fs::DirBuilder::new().mode(0o700).create(&location) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    let directory = open_directory(&location)?;
    ensure!(
        std::fs::read_dir(child(&directory, "."))?
            .take(MAX_EXPORT_FILES)
            .count()
            < MAX_EXPORT_FILES,
        "documents_export_capacity_reached"
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(NOFOLLOW)
        .open(child(&directory, name))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    directory.sync_all()?;
    Ok(())
}
fn verify_export(workspace: &Path, transfer: &Transfer) -> Result<()> {
    let path = transfer
        .output_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("documents_export_unconfirmed"))?;
    let bytes = super::super::media::read_workspace_bytes(workspace, path)?;
    ensure!(
        transfer.output_bytes == Some(bytes.len())
            && transfer.output_md5.as_deref()
                == Some(format!("{:x}", md5::compute(&bytes)).as_str()),
        "documents_export_changed_read_again"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    type Calls = Arc<Mutex<Vec<(String, Value)>>>;
    fn broker(
        root: &Path,
        workspace: &Path,
        handler: impl Fn(&str, &Value) -> Result<Value> + Send + Sync + 'static,
    ) -> (Broker, Calls) {
        private_directory(root).unwrap();
        private_directory(workspace).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let remote: Remote = Arc::new(move |name, args| {
            recorded.lock().unwrap().push((name.clone(), args.clone()));
            let result = handler(&name, &args);
            Box::pin(async move { result })
        });
        (
            Broker {
                registrar: None,
                folder: "bound_folder".into(),
                root: root.into(),
                workspace: workspace.into(),
                remote,
            },
            calls,
        )
    }
    fn request(action: &str) -> Request {
        Request {
            action: action.into(),
            operation_id: "poll_one".into(),
            source_operation_id: "source_one".into(),
            ..Default::default()
        }
    }
    fn pending(broker: &Broker, kind: Kind) -> Transfer {
        let mut transfer = Transfer::new(
            "source_one",
            &broker.folder,
            kind,
            "private_task_key".into(),
        );
        if kind == Kind::Export {
            transfer.file_id = "member_file".into();
        }
        transfer
    }
    fn deferred_submission(broker: &Broker) -> Transfer {
        use base64::Engine;
        let bytes = b"retained original document";
        atomic_write(
            &broker
                .root
                .join(format!("draft-{}.b64", hash("source_one"))),
            &base64::engine::general_purpose::STANDARD.encode(bytes),
        )
        .unwrap();
        let mut transfer = pending(broker, Kind::Import);
        transfer.phase = Phase::SubmitDeferred;
        transfer.file_key = "private_upload_key".into();
        transfer.file_name = "report.docx".into();
        transfer.input_bytes = Some(bytes.len());
        transfer.input_md5 = Some(format!("{:x}", md5::compute(bytes)));
        broker.save_transfer(&transfer).unwrap();
        transfer
    }
    fn definitive_refusal(quota: bool) -> anyhow::Error {
        use crate::codex::app_server::scoped_integrations::{
            DocumentNotDispatched, DocumentRateLimited,
        };
        if quota {
            DocumentRateLimited.into()
        } else {
            DocumentNotDispatched.into()
        }
    }

    #[tokio::test]
    async fn deferred_import_submit_preserves_upload_and_resumes_only_the_refused_step() {
        for quota in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("state");
            let watched = root.join(format!("transfer-{}.json", hash("source_one")));
            let attempt = Arc::new(Mutex::new(0));
            let attempts = attempt.clone();
            let (broker, calls) =
                broker(&root, &temp.path().join("workspace"), move |name, args| {
                    assert_eq!(name, "manage.async_import");
                    let saved: Value =
                        serde_json::from_str(&std::fs::read_to_string(&watched).unwrap()).unwrap();
                    assert_eq!(saved["phase"], "submit_unknown");
                    assert_eq!(args["task_id"], "private_task_key");
                    assert_eq!(args["file_key"], "private_upload_key");
                    assert_eq!(args["file_size"], 26);
                    let mut count = attempts.lock().unwrap();
                    *count += 1;
                    if *count == 1 {
                        Err(definitive_refusal(quota))
                    } else {
                        Ok(json!({"task_id":"confirmed_private_task"}))
                    }
                });
            deferred_submission(&broker);
            let journal = temp.path().join("journal");
            assert!(
                broker
                    .transfer(&request("import_progress"), &journal, "first")
                    .await
                    .is_err()
            );
            assert!(
                broker
                    .load_transfer("source_one", Kind::Import)
                    .unwrap()
                    .phase
                    == Phase::SubmitDeferred
            );
            let saved: Value =
                serde_json::from_str(&std::fs::read_to_string(&journal).unwrap()).unwrap();
            assert_eq!(saved["result"]["status"], "transfer_deferred");
            assert_eq!(saved["result"]["source_operation_id"], "source_one");
            let mut next = request("import_progress");
            next.operation_id = "poll_two".into();
            let result = broker.transfer(&next, &journal, "second").await.unwrap();
            assert_eq!(result["status"], "pending");
            assert!(!result.to_string().contains("private"));
            let transfer = broker.load_transfer("source_one", Kind::Import).unwrap();
            assert!(transfer.phase == Phase::Pending);
            assert_eq!(transfer.task_id, "confirmed_private_task");
            assert!(transfer.file_key.is_empty());
            assert_eq!(calls.lock().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn resumed_import_submission_becoming_unknown_is_never_repeated() {
        let temp = tempfile::tempdir().unwrap();
        let (broker, calls) = broker(
            &temp.path().join("state"),
            &temp.path().join("workspace"),
            |name, _| {
                assert_eq!(name, "manage.async_import");
                anyhow::bail!("transport_unknown")
            },
        );
        deferred_submission(&broker);
        let journal = temp.path().join("journal");
        assert!(
            broker
                .transfer(&request("import_progress"), &journal, "first")
                .await
                .is_err()
        );
        assert!(
            broker
                .load_transfer("source_one", Kind::Import)
                .unwrap()
                .phase
                == Phase::SubmitUnknown
        );
        let mut next = request("import_progress");
        next.operation_id = "poll_two".into();
        assert!(broker.transfer(&next, &journal, "second").await.is_err());
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deferred_import_move_resumes_known_file_without_importing_again() {
        for quota in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("state");
            let watched = root.join(format!("transfer-{}.json", hash("source_one")));
            let count = Arc::new(Mutex::new(0));
            let attempts = count.clone();
            let (broker, calls) = broker(
                &root,
                &temp.path().join("workspace"),
                move |name, args| match name {
                    "manage.import_progress" => Ok(
                        json!({"progress":100,"file_id":"new_file","file_url":"https://docs.qq.com/doc/new_file"}),
                    ),
                    "manage.move_file" => {
                        let saved: Value =
                            serde_json::from_str(&std::fs::read_to_string(&watched).unwrap())
                                .unwrap();
                        assert_eq!(saved["phase"], "move_unknown");
                        assert_eq!(args["file_id"], "new_file");
                        assert_eq!(args["target_folder_id"], "bound_folder");
                        let mut n = attempts.lock().unwrap();
                        *n += 1;
                        if *n == 1 {
                            Err(definitive_refusal(quota))
                        } else {
                            Ok(json!({}))
                        }
                    }
                    "manage.folder_list" => {
                        Ok(json!({"list":[{"id":"new_file","is_folder":false}],"finish":true}))
                    }
                    _ => panic!("unexpected provider call"),
                },
            );
            broker
                .save_transfer(&pending(&broker, Kind::Import))
                .unwrap();
            let journal = temp.path().join("journal");
            assert!(
                broker
                    .transfer(&request("import_progress"), &journal, "first")
                    .await
                    .is_err()
            );
            let deferred = broker.load_transfer("source_one", Kind::Import).unwrap();
            assert!(deferred.phase == Phase::MoveDeferred);
            assert_eq!(deferred.file_id, "new_file");
            let mut next = request("import_progress");
            next.operation_id = "poll_two".into();
            assert_eq!(
                broker.transfer(&next, &journal, "second").await.unwrap()["status"],
                "complete"
            );
            let calls = calls.lock().unwrap();
            assert_eq!(
                calls
                    .iter()
                    .filter(|c| c.0 == "manage.import_progress")
                    .count(),
                1
            );
            assert_eq!(
                calls.iter().filter(|c| c.0 == "manage.move_file").count(),
                2
            );
        }
    }

    #[tokio::test]
    async fn resumed_import_move_becoming_unknown_is_never_repeated() {
        let temp = tempfile::tempdir().unwrap();
        let (broker, calls) = broker(
            &temp.path().join("state"),
            &temp.path().join("workspace"),
            |name, _| match name {
                "manage.move_file" => anyhow::bail!("transport_unknown"),
                "manage.folder_list" => Ok(json!({"list":[],"finish":true})),
                _ => panic!("unexpected provider call"),
            },
        );
        let mut transfer = pending(&broker, Kind::Import);
        transfer.phase = Phase::MoveDeferred;
        transfer.file_id = "new_file".into();
        broker.save_transfer(&transfer).unwrap();
        let journal = temp.path().join("journal");
        assert!(
            broker
                .transfer(&request("import_progress"), &journal, "first")
                .await
                .is_err()
        );
        assert!(
            broker
                .load_transfer("source_one", Kind::Import)
                .unwrap()
                .phase
                == Phase::MoveUnknown
        );
        let mut next = request("import_progress");
        next.operation_id = "poll_two".into();
        assert!(broker.transfer(&next, &journal, "second").await.is_err());
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.iter().filter(|c| c.0 == "manage.move_file").count(),
            1
        );
    }

    #[tokio::test]
    async fn deferred_import_submit_checks_retained_bytes_before_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let (broker, calls) = broker(
            &temp.path().join("state"),
            &temp.path().join("workspace"),
            |_, _| panic!("must not dispatch"),
        );
        deferred_submission(&broker);
        use base64::Engine;
        atomic_write(
            &broker
                .root
                .join(format!("draft-{}.b64", hash("source_one"))),
            &base64::engine::general_purpose::STANDARD.encode(b"changed bytes"),
        )
        .unwrap();
        assert_eq!(
            broker
                .transfer(
                    &request("import_progress"),
                    &temp.path().join("journal"),
                    "fp"
                )
                .await
                .unwrap_err()
                .to_string(),
            "documents_retained_import_changed"
        );
        assert!(calls.lock().unwrap().is_empty());
        assert!(
            broker
                .load_transfer("source_one", Kind::Import)
                .unwrap()
                .phase
                == Phase::SubmitDeferred
        );
    }

    #[test]
    fn legacy_transfer_records_default_input_verification_fields() {
        let transfer = Transfer::new("source_one", "bound_folder", Kind::Import, "task".into());
        let mut value = serde_json::to_value(transfer).unwrap();
        value.as_object_mut().unwrap().remove("input_bytes");
        value.as_object_mut().unwrap().remove("input_md5");
        let restored: Transfer = serde_json::from_value(value).unwrap();
        assert!(restored.phase == Phase::Pending);
        assert!(restored.input_bytes.is_none());
        assert!(restored.input_md5.is_none());
    }
    #[test]
    fn transfer_requests_require_import_path_and_accept_export_without_version() {
        let mut import = request("import");
        assert_eq!(
            import.validate().unwrap_err().to_string(),
            "documents_import_path_required"
        );
        import.file_path = "report.docx".into();
        import.validate().unwrap();
        let mut export = request("export");
        export.file_id = "member_file".into();
        export.validate().unwrap();
        export.expected_modified_at = Some(42);
        export.validate().unwrap();
    }
    #[tokio::test]
    async fn execute_does_not_journal_unknown_for_undispatched_input_or_scope_errors() {
        let temp = tempfile::tempdir().unwrap();
        let (broker, calls) = broker(
            &temp.path().join("state"),
            &temp.path().join("workspace"),
            |name, _| {
                assert_eq!(name, "manage.folder_list");
                Ok(json!({"list":[],"finish":true}))
            },
        );
        let journal = broker.root.join(format!("{}.json", hash("poll_one")));
        let mut import = request("import");
        import.file_path = "missing.docx".into();
        assert!(broker.execute(import, 1).await.is_err());
        assert!(!journal.exists());
        assert!(broker.execute(request("import_progress"), 1).await.is_err());
        assert!(!journal.exists());
        let mut transfer = pending(&broker, Kind::Import);
        transfer.folder = "other_folder".into();
        broker.save_transfer(&transfer).unwrap();
        assert!(broker.execute(request("import_progress"), 1).await.is_err());
        assert!(!journal.exists());
        transfer.folder = broker.folder.clone();
        transfer.phase = Phase::SubmitUnknown;
        broker.save_transfer(&transfer).unwrap();
        let error = broker
            .execute(request("import_progress"), 1)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "documents_transfer_unknown_inspect_first"
        );
        assert!(!journal.exists());
        let mut import = request("import");
        import.file_path = "missing.docx".into();
        let fingerprint = hash(&serde_json::to_string(&import).unwrap());
        save_operation(
            &journal,
            &fingerprint,
            &json!({"ok":false,"status":"not_dispatched"}),
        )
        .unwrap();
        assert!(broker.execute(import, 1).await.is_err());
        let saved: Value =
            serde_json::from_str(&private_read_limit(&journal, 32768).unwrap()).unwrap();
        assert_eq!(saved["result"]["status"], "not_dispatched");
        assert_eq!(calls.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn execute_export_version_error_is_distinct_from_ambiguous_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let (broker, calls) = broker(
            &temp.path().join("state"),
            &temp.path().join("workspace"),
            |name, _| match name {
                "manage.folder_list" => {
                    Ok(json!({"list":[{"id":"member_file","is_folder":false}],"finish":true}))
                }
                "manage.query_file_info" => Ok(json!({"last_modify_time":42})),
                "manage.export_file" => anyhow::bail!("simulated_timeout"),
                _ => panic!("unexpected call"),
            },
        );
        let journal = broker.root.join(format!("{}.json", hash("poll_one")));
        let mut export = request("export");
        export.file_id = "member_file".into();
        export.expected_modified_at = Some(41);
        assert_eq!(
            broker.execute(export, 1).await.unwrap_err().to_string(),
            "documents_changed_read_again"
        );
        assert!(!journal.exists());
        assert!(
            !calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.0 == "manage.export_file")
        );
        let mut export = request("export");
        export.file_id = "member_file".into();
        export.expected_modified_at = Some(42);
        let response = broker.execute(export, 1).await.unwrap();
        assert_eq!(response["status"], "unknown");
        assert_eq!(response["stage"], "export");
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.0 == "manage.export_file")
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn execute_completed_export_validation_failure_has_no_new_unknown_write() {
        let temp = tempfile::tempdir().unwrap();
        let (broker, calls) = broker(
            &temp.path().join("state"),
            &temp.path().join("workspace"),
            |name, _| {
                assert_eq!(name, "manage.folder_list");
                Ok(json!({"list":[{"id":"member_file","is_folder":false}],"finish":true}))
            },
        );
        let mut transfer = pending(&broker, Kind::Export);
        transfer.phase = Phase::Complete;
        transfer.output_path = Some(format!("/workspace/{EXPORT_DIRECTORY}/export-test.docx"));
        transfer.output_md5 = Some(format!("{:x}", md5::compute(b"complete")));
        transfer.output_bytes = Some(8);
        write_export(&broker.workspace, "export-test.docx", b"tampered").unwrap();
        broker.save_transfer(&transfer).unwrap();
        assert_eq!(
            broker
                .execute(request("export_progress"), 1)
                .await
                .unwrap_err()
                .to_string(),
            "documents_export_changed_read_again"
        );
        assert!(
            !broker
                .root
                .join(format!("{}.json", hash("poll_one")))
                .exists()
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn transfer_scope_and_kind_are_checked_before_provider_calls() {
        let temp = tempfile::tempdir().unwrap();
        let (broker, calls) = broker(
            &temp.path().join("state"),
            &temp.path().join("workspace"),
            |_, _| panic!("must not call provider"),
        );
        let mut transfer = pending(&broker, Kind::Import);
        transfer.folder = "other_folder".into();
        broker.save_transfer(&transfer).unwrap();
        assert!(
            broker
                .transfer(
                    &request("import_progress"),
                    &temp.path().join("journal"),
                    "fp"
                )
                .await
                .is_err()
        );
        transfer.folder = broker.folder.clone();
        transfer.kind = Kind::Export;
        broker.save_transfer(&transfer).unwrap();
        assert!(
            broker
                .transfer(
                    &request("import_progress"),
                    &temp.path().join("journal"),
                    "fp"
                )
                .await
                .is_err()
        );
        assert!(calls.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn import_only_moves_the_confirmed_new_file_then_checks_membership() {
        let temp = tempfile::tempdir().unwrap();
        let journal = temp.path().join("journal");
        let watched = journal.clone();
        let (broker, calls) = broker(
            &temp.path().join("state"),
            &temp.path().join("workspace"),
            move |name, args| match name {
                "manage.import_progress" => Ok(
                    json!({"progress":100,"file_id":"new_file","file_url":"https://docs.qq.com/doc/new_file"}),
                ),
                "manage.move_file" => {
                    let saved: Value =
                        serde_json::from_str(&std::fs::read_to_string(&watched).unwrap()).unwrap();
                    assert_eq!(saved["result"]["stage"], "import_move");
                    assert_eq!(args["file_id"], "new_file");
                    assert_eq!(args["target_folder_id"], "bound_folder");
                    Ok(json!({}))
                }
                "manage.folder_list" => {
                    Ok(json!({"list":[{"id":"new_file","is_folder":false}],"finish":true}))
                }
                _ => panic!("unexpected call"),
            },
        );
        broker
            .save_transfer(&pending(&broker, Kind::Import))
            .unwrap();
        let result = broker
            .transfer(&request("import_progress"), &journal, "fp")
            .await
            .unwrap();
        assert_eq!(result["status"], "complete");
        assert!(!result.to_string().contains("private_task_key"));
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .map(|c| c.0.as_str())
                .collect::<Vec<_>>(),
            [
                "manage.import_progress",
                "manage.move_file",
                "manage.folder_list"
            ]
        );
        broker
            .transfer(&request("import_progress"), &journal, "fp")
            .await
            .unwrap();
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.0 == "manage.move_file")
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn ambiguous_move_is_not_dispatched_twice_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let (broker, calls) = broker(
            &temp.path().join("state"),
            &temp.path().join("workspace"),
            |name, _| match name {
                "manage.import_progress" => Ok(json!({"progress":100,"file_id":"new_file"})),
                "manage.move_file" => anyhow::bail!("simulated_timeout"),
                "manage.folder_list" => Ok(json!({"list":[],"finish":true})),
                _ => panic!("unexpected call"),
            },
        );
        broker
            .save_transfer(&pending(&broker, Kind::Import))
            .unwrap();
        let journal = temp.path().join("journal");
        assert!(
            broker
                .transfer(&request("import_progress"), &journal, "fp")
                .await
                .is_err()
        );
        let mut second = request("import_progress");
        second.operation_id = "poll_two".into();
        assert!(broker.transfer(&second, &journal, "fp2").await.is_err());
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.0 == "manage.move_file")
                .count(),
            1
        );
    }
    #[tokio::test]
    async fn unknown_upload_or_submission_never_polls_or_reuploads() {
        let temp = tempfile::tempdir().unwrap();
        let (broker, calls) = broker(
            &temp.path().join("state"),
            &temp.path().join("workspace"),
            |_, _| panic!("must not call provider"),
        );
        for phase in [Phase::UploadUnknown, Phase::SubmitUnknown] {
            let mut transfer = pending(&broker, Kind::Import);
            transfer.phase = phase;
            broker.save_transfer(&transfer).unwrap();
            assert!(
                broker
                    .transfer(
                        &request("import_progress"),
                        &temp.path().join("journal"),
                        "fp"
                    )
                    .await
                    .is_err()
            );
        }
        assert!(calls.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn export_poll_rechecks_membership_before_using_private_task() {
        let temp = tempfile::tempdir().unwrap();
        let (broker, calls) = broker(
            &temp.path().join("state"),
            &temp.path().join("workspace"),
            |name, _| {
                assert_eq!(name, "manage.folder_list");
                Ok(json!({"list":[],"finish":true}))
            },
        );
        broker
            .save_transfer(&pending(&broker, Kind::Export))
            .unwrap();
        assert!(
            broker
                .transfer(
                    &request("export_progress"),
                    &temp.path().join("journal"),
                    "fp"
                )
                .await
                .is_err()
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn export_checks_version_and_persists_private_task_without_exposing_it() {
        let temp = tempfile::tempdir().unwrap();
        let journal = temp.path().join("journal");
        let watched = journal.clone();
        let (broker, calls) = broker(
            &temp.path().join("state"),
            &temp.path().join("workspace"),
            move |name, _| match name {
                "manage.folder_list" => {
                    Ok(json!({"list":[{"id":"member_file","is_folder":false}],"finish":true}))
                }
                "manage.query_file_info" => Ok(json!({"last_modify_time":42})),
                "manage.export_file" => {
                    let saved: Value =
                        serde_json::from_str(&std::fs::read_to_string(&watched).unwrap()).unwrap();
                    assert_eq!(saved["result"]["stage"], "export");
                    Ok(json!({"task_id":"private_task_key"}))
                }
                _ => panic!("unexpected call"),
            },
        );
        let mut r = request("export");
        r.file_id = "member_file".into();
        r.expected_modified_at = Some(41);
        assert!(broker.transfer(&r, &journal, "fp").await.is_err());
        assert!(
            !calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.0 == "manage.export_file")
        );
        r.expected_modified_at = Some(42);
        let result = broker.transfer(&r, &journal, "fp").await.unwrap();
        assert_eq!(result["status"], "pending");
        assert!(!result.to_string().contains("private_task_key"));
        assert_eq!(
            broker
                .load_transfer("poll_one", Kind::Export)
                .unwrap()
                .task_id,
            "private_task_key"
        );
    }
    #[test]
    fn export_write_rejects_symlinks_and_preserves_existing_files() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        private_directory(&workspace).unwrap();
        let outside = temp.path().join("outside");
        private_directory(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join(EXPORT_DIRECTORY)).unwrap();
        assert!(write_export(&workspace, "export-test.docx", b"document").is_err());
        std::fs::remove_file(workspace.join(EXPORT_DIRECTORY)).unwrap();
        write_export(&workspace, "export-test.docx", b"document").unwrap();
        assert!(write_export(&workspace, "export-test.docx", b"replacement").is_err());
        assert_eq!(
            std::fs::read(workspace.join(EXPORT_DIRECTORY).join("export-test.docx")).unwrap(),
            b"document"
        );
        assert!(write_export(&workspace, "../escape.docx", b"document").is_err());
    }
    #[tokio::test]
    async fn import_rejects_outside_workspace_and_oversize_before_preparing_upload() {
        let temp = tempfile::tempdir().unwrap();
        let (broker, calls) = broker(
            &temp.path().join("state"),
            &temp.path().join("workspace"),
            |_, _| panic!("must not call provider"),
        );
        let outside = temp.path().join("outside.docx");
        std::fs::write(&outside, b"outside").unwrap();
        std::os::unix::fs::symlink(&outside, broker.workspace.join("linked.docx")).unwrap();
        std::fs::write(
            broker.workspace.join("large.docx"),
            vec![0; MAX_TRANSFER_BYTES + 1],
        )
        .unwrap();
        for path in ["../outside.docx", "linked.docx", "large.docx"] {
            let mut r = request("import");
            r.file_path = path.into();
            assert!(
                broker
                    .transfer(&r, &temp.path().join("journal"), "fp")
                    .await
                    .is_err()
            );
        }
        assert!(calls.lock().unwrap().is_empty());
    }
    #[test]
    fn interrupted_export_only_recovers_matching_complete_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        private_directory(&workspace).unwrap();
        write_export(&workspace, "export-test.docx", b"complete").unwrap();
        let mut transfer = Transfer::new(
            "source_one",
            "bound_folder",
            Kind::Export,
            "private_task_key".into(),
        );
        transfer.phase = Phase::DownloadUnknown;
        transfer.output_path = Some(format!("/workspace/{EXPORT_DIRECTORY}/export-test.docx"));
        transfer.output_md5 = Some(format!("{:x}", md5::compute(b"complete")));
        transfer.output_bytes = Some(8);
        verify_export(&workspace, &transfer).unwrap();
        std::fs::write(
            workspace.join(EXPORT_DIRECTORY).join("export-test.docx"),
            b"partial",
        )
        .unwrap();
        assert!(verify_export(&workspace, &transfer).is_err());
    }
    #[test]
    fn transfer_response_validation_does_not_forward_secrets_or_failures() {
        assert!(document_url(&json!("https://docs.qq.com/doc/file?secret=value")).is_none());
        assert!(document_url(&json!("https://docs.qq.com.evil.invalid/doc/file")).is_none());
        assert_eq!(
            document_url(&json!("https://docs.qq.com/doc/file")).unwrap(),
            "https://docs.qq.com/doc/file"
        );
        for v in [
            json!({"progress":101}),
            json!({"progress":100,"error":"secret error"}),
            json!({"progress":100,"status":"failed"}),
        ] {
            assert!(import_export_progress(&v).is_err());
        }
        assert!(import_name("missing-extension").is_err());
        assert!(import_name("bad\\name.docx").is_err());
        assert_eq!(import_name("报告.docx").unwrap(), "报告.docx");
        assert_eq!(
            export_extension(
                &json!({"file_name":"evil.sh"}),
                "https://example.invalid/file"
            ),
            "bin"
        );
    }
}
