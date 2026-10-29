//! Named checkpoints, not author overrides or remarks on automatic revisions.
//! The host enables this only for ordinary docs after a successful content write
//! and a fresh scope check. This endpoint snapshots the current whole document;
//! it has no compare-and-swap or idempotency key and may include concurrent edits.
use super::*;
use crate::codex::app_server::scoped_integrations::network::{PublicConnection, bounded_body};
use reqwest::header::COOKIE;

const SOURCE: &str = "来自 ClawMingle";
const LIMIT: usize = 20;

struct Version {
    id: String,
    rev: u64,
    alias: String,
}
struct Versions {
    records: Vec<Version>,
    count: u64,
    truncated: bool,
}
fn parse_versions(value: &Value) -> Result<Versions> {
    ensure!(value["retcode"] == 0, "documents_native_versions_rejected");
    let result = &value["result"];
    let rows = result["records"]
        .as_array()
        .filter(|r| r.len() <= LIMIT)
        .ok_or_else(|| anyhow::anyhow!("documents_native_versions_invalid"))?;
    let count = result["nums"]
        .as_u64()
        .filter(|n| *n >= rows.len() as u64)
        .ok_or_else(|| anyhow::anyhow!("documents_native_versions_invalid"))?;
    ensure!(
        result["page"] == 1 && result["pages"].is_u64() && (count == 0 || !rows.is_empty()),
        "documents_native_versions_invalid"
    );
    let mut ids = BTreeSet::new();
    let mut records = Vec::with_capacity(rows.len());
    for row in rows {
        let id = row["recordId"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| row["recordId"].as_u64().map(|id| id.to_string()))
            .filter(|id| valid_id(id) && ids.insert(id.clone()))
            .ok_or_else(|| anyhow::anyhow!("documents_native_versions_invalid"))?;
        let rev = row["rev"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("documents_native_versions_invalid"))?;
        let alias = row["alias"]
            .as_str()
            .filter(|s| s.len() <= 1024)
            .ok_or_else(|| anyhow::anyhow!("documents_native_versions_invalid"))?
            .to_owned();
        records.push(Version { id, rev, alias });
    }
    Ok(Versions {
        truncated: count > records.len() as u64 || result["pages"].as_u64().unwrap() > 1,
        records,
        count,
    })
}

fn finish(path: &Path, mut record: Value, status: &str) -> Result<Value> {
    record["status"] = json!(status);
    record["checked_at"] = json!(chrono::Utc::now().timestamp());
    record["message"] = json!(match status {
        "confirmed" => "已在腾讯文档的已保存版本中标记“来自 ClawMingle”；作者仍为当前登录账号。",
        "no_changes" => "腾讯文档报告没有新变更，本次没有创建新的命名版本。",
        "rejected" => "腾讯文档未允许保存来源版本；正文操作结果保持不变。",
        "unavailable" => "来源版本暂时无法保存；正文操作结果保持不变。",
        _ => "来源版本保存结果尚未确认；只查询核验，不重复保存或重放正文操作。",
    });
    atomic_write(path, &record.to_string())?;
    Ok(record)
}

impl qq_native::Registrar {
    /// The caller must hold the group operation lock, confirm this is an ordinary
    /// doc in the bound workspace, and use the successful content operation ID.
    pub(super) async fn source_checkpoint(
        &self,
        root: &Path,
        file: &str,
        operation: &str,
    ) -> Result<Value> {
        ensure!(
            valid_id(file) && valid_id(operation) && operation.len() <= 80,
            "documents_native_versions_input_invalid"
        );
        let path = root.join(format!(
            "source-checkpoint-{}.json",
            hash(&format!("{file}:{operation}"))
        ));
        if path.exists() {
            let record: Value = serde_json::from_str(&private_read_limit(&path, 16384)?)?;
            ensure!(
                record["file_id"] == file && record["operation_id"] == operation,
                "documents_native_versions_journal_invalid"
            );
            return if record["status"] == "unknown" {
                self.reconcile_checkpoint(&path, file, record).await
            } else {
                Ok(record)
            };
        }
        let mut record = json!({"source":SOURCE,"file_id":file,"operation_id":operation,"kind":"named_checkpoint","author_changed":false,"list_limit":LIMIT});
        let before = match self.checkpoint_versions(file).await {
            Ok(before) => before,
            Err(_) => return finish(&path, record, "unavailable"),
        };
        record["before_record_ids"] =
            json!(before.records.iter().map(|v| &v.id).collect::<Vec<_>>());
        record["before_max_rev"] = json!(before.records.iter().map(|v| v.rev).max());
        record["before_record_count"] = json!(before.count);
        record["before_list_truncated"] = json!(before.truncated);
        let check = match (self.remote)("version_check".into(), json!({"file_id":file})).await {
            Ok(value) => value,
            Err(_) => return finish(&path, record, "unavailable"),
        };
        if check["retcode"] != 0 {
            return finish(
                &path,
                record,
                if check["retcode"] == 318301 {
                    "no_changes"
                } else {
                    "rejected"
                },
            );
        }
        record = finish(&path, record, "unknown")?;
        // Durable before dispatch: cancellation, a missing ACK or a later call
        // never permits a second save for this same content operation.
        match (self.remote)("version_save".into(), json!({"file_id":file})).await {
            Ok(value) if value["retcode"] == 318301 => return finish(&path, record, "no_changes"),
            Ok(value) if value["retcode"].as_i64().is_some_and(|code| code != 0) => {
                return finish(&path, record, "rejected");
            }
            _ => {}
        }
        self.reconcile_checkpoint(&path, file, record).await
    }

    async fn checkpoint_versions(&self, file: &str) -> Result<Versions> {
        parse_versions(&(self.remote)("version_list".into(), json!({"file_id":file})).await?)
    }

    async fn reconcile_checkpoint(
        &self,
        path: &Path,
        file: &str,
        mut record: Value,
    ) -> Result<Value> {
        let before = record["before_record_ids"]
            .as_array()
            .filter(|v| v.len() <= LIMIT)
            .ok_or_else(|| anyhow::anyhow!("documents_native_versions_journal_invalid"))?;
        let old_ids: BTreeSet<_> = before
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        ensure!(
            old_ids.len() == before.len() && record["before_record_count"].is_u64(),
            "documents_native_versions_journal_invalid"
        );
        let after = match self.checkpoint_versions(file).await {
            Ok(value) => value,
            Err(_) => {
                record["verification"] = json!("readback_unavailable");
                return finish(path, record, "unknown");
            }
        };
        record["record_count"] = json!(after.count);
        record["list_truncated"] = json!(after.truncated);
        let candidates: Vec<_> = after
            .records
            .iter()
            .filter(|v| {
                v.alias == SOURCE
                    && !old_ids.contains(&v.id)
                    && record["before_max_rev"]
                        .as_u64()
                        .is_none_or(|rev| v.rev > rev)
            })
            .collect();
        // A count/revision advance also prevents an old version from beyond the
        // bounded page being mistaken for our new checkpoint after list churn.
        if candidates.len() == 1 && after.count > record["before_record_count"].as_u64().unwrap() {
            record["record_id"] = json!(candidates[0].id);
            record["rev"] = json!(candidates[0].rev);
            record["verification"] = json!("unique_new_named_version");
            finish(path, record, "confirmed")
        } else {
            record["verification"] = json!("no_unique_new_named_version");
            finish(path, record, "unknown")
        }
    }
}

fn request(action: &str, args: &Value) -> Result<(&'static str, Value)> {
    let file = args["file_id"]
        .as_str()
        .filter(|id| valid_id(id))
        .ok_or_else(|| anyhow::anyhow!("documents_native_versions_input_invalid"))?;
    let id = format!("300000000${file}");
    Ok(match action {
        "version_list" => (
            "/api/file/record/version/get_list",
            json!({"globalPadId":id,"page":1,"limit":LIMIT,"filter_type":0}),
        ),
        "version_check" => (
            "/api/file/record/version/save",
            json!({"globalPadId":id,"alias":SOURCE,"only_check":true}),
        ),
        "version_save" => (
            "/api/file/record/version/save",
            json!({"globalPadId":id,"alias":SOURCE}),
        ),
        _ => anyhow::bail!("documents_native_versions_action_invalid"),
    })
}

pub(super) async fn http(path: &Path, action: &str, args: &Value) -> Result<Value> {
    use std::os::unix::fs::MetadataExt;
    let (endpoint, body) = request(action, args)?;
    let session = (|| {
        ensure!(
            std::fs::symlink_metadata(path)?.nlink() == 1,
            "private_session_required"
        );
        qq_native::cookies_for_path(
            &private_read_limit(path, 512 * 1024)?,
            chrono::Utc::now().timestamp() as f64,
            endpoint,
        )
    })()
    .map_err(|_| anyhow::anyhow!("documents_native_login_required"))?;
    let operation = async {
        let connection = PublicConnection::new(&format!("https://docs.qq.com{endpoint}")).await?;
        let response = connection
            .client
            .post(connection.url.clone())
            .query(&[("xsrf", session.xsrf)])
            .header(COOKIE, session.header)
            .header("Origin", "https://docs.qq.com")
            .header("Referer", "https://docs.qq.com/")
            .json(&body)
            .send()
            .await?;
        connection.verify(&response)?;
        ensure!(response.status().is_success(), "http_rejected");
        Ok::<_, anyhow::Error>(serde_json::from_slice(
            &bounded_body(response, 256 * 1024).await?,
        )?)
    }
    .await;
    // reqwest errors can contain the full URL with its xsrf query parameter.
    operation.map_err(|_| anyhow::anyhow!("documents_native_versions_unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    type Calls = Arc<Mutex<Vec<(String, Value)>>>;
    fn version(id: &str, rev: u64, alias: &str) -> Value {
        json!({"recordId":id,"rev":rev,"alias":alias,"creator":"private_author","url":"private_link"})
    }
    fn list(records: Vec<Value>, count: u64) -> Value {
        json!({"retcode":0,"result":{"records":records,"nums":count,"page":1,"pages":count.div_ceil(LIMIT as u64),"creators":{"private_author":"private_profile"}}})
    }
    fn fake(
        results: Vec<(&str, Value)>,
    ) -> (tempfile::TempDir, PathBuf, qq_native::Registrar, Calls) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        private_directory(&root).unwrap();
        let mut registrar = qq_native::NativeConfig {
            enabled: true,
            browser_state_file: Some("/private/fixture.json".into()),
            groups: BTreeMap::from([("test_group".into(), "12345".into())]),
        }
        .registrar("test_group")
        .unwrap();
        let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(
            results
                .into_iter()
                .map(|(s, v)| (s.to_owned(), v))
                .collect::<Vec<_>>(),
        )));
        let calls: Calls = Arc::new(Mutex::new(vec![]));
        let observed = calls.clone();
        registrar.remote = Arc::new(move |action, args| {
            observed.lock().unwrap().push((action.clone(), args));
            let (expected, value) = queue.lock().unwrap().pop_front().expect("unexpected call");
            assert_eq!(action, expected);
            Box::pin(async move {
                if value == "timeout" {
                    anyhow::bail!("private_url_xsrf");
                }
                if value == "pending" {
                    return std::future::pending().await;
                }
                Ok(value)
            })
        });
        (dir, root, registrar, calls)
    }

    #[test]
    fn fixed_actions_and_source_ignore_caller_supplied_alias_global_id_and_url() {
        let args = json!({"file_id":"file_one","alias":"other","globalPadId":"other","url":"https://example.com","only_check":false});
        let (endpoint, body) = request("version_check", &args).unwrap();
        assert_eq!(endpoint, "/api/file/record/version/save");
        assert_eq!(
            body,
            json!({"globalPadId":"300000000$file_one","alias":SOURCE,"only_check":true})
        );
        assert_eq!(
            request("version_list", &args).unwrap().1,
            json!({"globalPadId":"300000000$file_one","page":1,"limit":20,"filter_type":0})
        );
        assert_eq!(
            request("version_save", &args).unwrap().1,
            json!({"globalPadId":"300000000$file_one","alias":SOURCE})
        );
        assert!(request("version_rename", &args).is_err());
        assert!(request("version_delete", &args).is_err());
        assert!(request("version_save", &json!({"file_id":"../other"})).is_err());
    }

    #[test]
    fn list_is_bounded_validated_and_marks_truncation_without_identity_leaks() {
        let input = list(vec![version("existing", 4, SOURCE)], 21);
        let parsed = parse_versions(&input).unwrap();
        assert_eq!(parsed.count, 21);
        assert!(parsed.truncated);
        assert_eq!(parsed.records[0].id, "existing");
        let mut numeric = list(vec![version("existing", 4, SOURCE)], 1);
        numeric["result"]["records"][0]["recordId"] = json!(42);
        assert_eq!(parse_versions(&numeric).unwrap().records[0].id, "42");
        for bad in [
            json!({}),
            json!({"retcode":1}),
            list(
                vec![version("same", 1, SOURCE), version("same", 2, SOURCE)],
                2,
            ),
            list(vec![version("existing", 1, SOURCE)], 0),
            list(vec![version("id", 1, SOURCE); 21], 21),
        ] {
            assert!(parse_versions(&bad).is_err());
        }
    }

    #[tokio::test]
    async fn saves_one_new_named_checkpoint_and_reuses_confirmed_record() {
        let (_dir, root, r, calls) = fake(vec![
            ("version_list", list(vec![version("old", 1, SOURCE)], 1)),
            ("version_check", json!({"retcode":0})),
            ("version_save", json!({"retcode":0})),
            (
                "version_list",
                list(
                    vec![version("new", 2, SOURCE), version("old", 1, SOURCE)],
                    2,
                ),
            ),
        ]);
        let result = r
            .source_checkpoint(&root, "file_one", "operation_one")
            .await
            .unwrap();
        assert_eq!(result["status"], "confirmed");
        assert_eq!(result["record_id"], "new");
        assert_eq!(result["rev"], 2);
        assert_eq!(result["author_changed"], false);
        assert_eq!(result["list_truncated"], false);
        assert!(!result.to_string().contains("private_"));
        assert_eq!(
            r.source_checkpoint(&root, "file_one", "operation_one")
                .await
                .unwrap(),
            result
        );
        assert_eq!(calls.lock().unwrap().len(), 4);
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|(_, args)| args == &json!({"file_id":"file_one"}))
        );
    }

    #[tokio::test]
    async fn no_changes_is_terminal_and_does_not_claim_a_new_version() {
        for stage in ["check", "save"] {
            let mut sequence = vec![
                ("version_list", list(vec![], 0)),
                (
                    "version_check",
                    json!({"retcode":if stage=="check" {318301} else {0}}),
                ),
            ];
            if stage == "save" {
                sequence.push(("version_save", json!({"retcode":318301})));
            }
            let (_dir, root, r, calls) = fake(sequence);
            let result = r
                .source_checkpoint(&root, "file_one", "operation_one")
                .await
                .unwrap();
            assert_eq!(result["status"], "no_changes");
            assert!(result.get("record_id").is_none());
            assert_eq!(
                r.source_checkpoint(&root, "file_one", "operation_one")
                    .await
                    .unwrap(),
                result
            );
            assert_eq!(
                calls.lock().unwrap().len(),
                if stage == "check" { 2 } else { 3 }
            );
        }
    }

    #[tokio::test]
    async fn unknown_only_reconciles_and_never_resaves() {
        let (_dir, root, r, calls) = fake(vec![
            ("version_list", list(vec![], 0)),
            ("version_check", json!({"retcode":0})),
            ("version_save", json!("timeout")),
            ("version_list", json!("timeout")),
            ("version_list", list(vec![], 0)),
            ("version_list", list(vec![version("new", 1, SOURCE)], 1)),
        ]);
        for _ in 0..2 {
            let result = r
                .source_checkpoint(&root, "file_one", "operation_one")
                .await
                .unwrap();
            assert_eq!(result["status"], "unknown");
            assert!(!result.to_string().contains("private_"));
        }
        assert_eq!(
            r.source_checkpoint(&root, "file_one", "operation_one")
                .await
                .unwrap()["status"],
            "confirmed"
        );
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(action, _)| action == "version_save")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn unchanged_or_ambiguous_aliases_are_never_claimed() {
        for after in [
            list(vec![version("old", 1, SOURCE)], 1),
            list(
                vec![version("first", 2, SOURCE), version("second", 3, SOURCE)],
                3,
            ),
            list(vec![version("different", 2, "human")], 2),
            list(vec![version("older_unseen", 0, SOURCE)], 21),
            list(vec![version("new", 2, SOURCE)], 1),
        ] {
            let (_dir, root, r, _) = fake(vec![
                ("version_list", list(vec![version("old", 1, SOURCE)], 1)),
                ("version_check", json!({"retcode":0})),
                ("version_save", json!({"retcode":0})),
                ("version_list", after),
            ]);
            let result = r
                .source_checkpoint(&root, "file_one", "operation_one")
                .await
                .unwrap();
            assert_eq!(result["status"], "unknown");
            assert!(result.get("record_id").is_none());
        }
    }

    #[tokio::test]
    async fn truncated_page_can_confirm_a_unique_newer_record_but_reports_its_limit() {
        let old: Vec<_> = (0..20)
            .map(|n| version(&format!("old_{n}"), n + 1, SOURCE))
            .collect();
        let mut new = old[..19].to_vec();
        new.insert(0, version("new", 21, SOURCE));
        let (_dir, root, r, _) = fake(vec![
            ("version_list", list(old, 30)),
            ("version_check", json!({"retcode":0})),
            ("version_save", json!({"retcode":0})),
            ("version_list", list(new, 31)),
        ]);
        let result = r
            .source_checkpoint(&root, "file_one", "operation_one")
            .await
            .unwrap();
        assert_eq!(result["status"], "confirmed");
        assert_eq!(result["before_list_truncated"], true);
        assert_eq!(result["list_truncated"], true);
        assert_eq!(result["record_count"], 31);
    }

    #[tokio::test]
    async fn cancellation_during_save_leaves_durable_unknown() {
        let (_dir, root, r, calls) = fake(vec![
            ("version_list", list(vec![], 0)),
            ("version_check", json!({"retcode":0})),
            ("version_save", json!("pending")),
            ("version_list", list(vec![version("new", 1, SOURCE)], 1)),
        ]);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(25),
                r.source_checkpoint(&root, "file_one", "operation_one")
            )
            .await
            .is_err()
        );
        assert_eq!(
            r.source_checkpoint(&root, "file_one", "operation_one")
                .await
                .unwrap()["status"],
            "confirmed"
        );
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(action, _)| action == "version_save")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn preflight_denial_or_read_failure_never_dispatches_save() {
        for sequence in [
            vec![("version_list", json!("timeout"))],
            vec![
                ("version_list", list(vec![], 0)),
                (
                    "version_check",
                    json!({"retcode":12607,"msg":"private_denial"}),
                ),
            ],
            vec![
                ("version_list", list(vec![], 0)),
                ("version_check", json!("timeout")),
            ],
        ] {
            let (_dir, root, r, calls) = fake(sequence);
            let result = r
                .source_checkpoint(&root, "file_one", "operation_one")
                .await
                .unwrap();
            assert!(matches!(
                result["status"].as_str(),
                Some("rejected" | "unavailable")
            ));
            assert!(!result.to_string().contains("private_"));
            assert!(
                calls
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(action, _)| action != "version_save")
            );
        }
    }

    #[tokio::test]
    async fn invalid_inputs_are_rejected_before_any_remote_read() {
        let (_dir, root, r, calls) = fake(vec![]);
        assert!(
            r.source_checkpoint(&root, "../file", "operation")
                .await
                .is_err()
        );
        assert!(
            r.source_checkpoint(&root, "file", "../operation")
                .await
                .is_err()
        );
        assert!(
            r.source_checkpoint(&root, "file", &"a".repeat(81))
                .await
                .is_err()
        );
        assert!(calls.lock().unwrap().is_empty());
    }
}
