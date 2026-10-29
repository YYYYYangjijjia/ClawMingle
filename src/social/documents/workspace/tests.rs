use super::*;
use std::sync::Mutex;

#[derive(Default)]
struct Cloud {
    folders: BTreeMap<String, Vec<Value>>,
    calls: Vec<(String, Value)>,
    fail: Option<(String, bool)>,
    next: usize,
}
fn fixture(dir: &Path) -> (Broker, Arc<Mutex<Cloud>>) {
    let state = Arc::new(Mutex::new(Cloud {
        folders: BTreeMap::from([
            (
                "bound".into(),
                vec![json!({"id":"task_one","title":"Travel","is_folder":true})],
            ),
            ("task_one".into(), vec![]),
        ]),
        ..Default::default()
    }));
    let cloned = state.clone();
    let remote: Remote = Arc::new(move |name, args| {
        let mut state = cloned.lock().unwrap();
        state.calls.push((name.clone(), args.clone()));
        let result = (|| {
            if state.fail.as_ref().is_some_and(|(tool, _)| tool == &name) {
                let (_, quota) = state.fail.take().unwrap();
                return Err(if quota {
                    crate::codex::app_server::scoped_integrations::DocumentRateLimited.into()
                } else {
                    anyhow::anyhow!("transport_unknown")
                });
            }
            match name.as_str() {
                "manage.folder_list" => Ok(
                    json!({"list":state.folders.get(args["folder_id"].as_str().unwrap()).cloned().unwrap_or_default(),"finish":true}),
                ),
                "manage.create_file" => {
                    assert_eq!(args["file_type"], "folder");
                    assert_eq!(args["parent_id"], "bound");
                    let entry = json!({"id":"task_new","title":args["title"],"is_folder":true});
                    state.folders.get_mut("bound").unwrap().push(entry);
                    state.folders.insert("task_new".into(), vec![]);
                    Ok(
                        json!({"file_id":"task_new","url":"https://docs.qq.com/desktop/?folder=task_new"}),
                    )
                }
                "create_smartcanvas_by_mdx" => {
                    state.next += 1;
                    let file = format!("file_{}", state.next);
                    let url = format!("https://docs.qq.com/aio/{file}");
                    state
                        .folders
                        .entry("home".into())
                        .or_default()
                        .push(json!({"id":file,"title":args["title"],"is_folder":false,"url":url}));
                    Ok(json!({"file_id":file,"url":url}))
                }
                "manage.move_file" => {
                    let mut entry = None;
                    for files in state.folders.values_mut() {
                        if let Some(pos) = files.iter().position(|v| v["id"] == args["file_id"]) {
                            entry = Some(files.remove(pos));
                            break;
                        }
                    }
                    let target = args["target_folder_id"].as_str().unwrap();
                    state
                        .folders
                        .get_mut(target)
                        .unwrap()
                        .push(entry.expect("move actual file"));
                    Ok(json!({}))
                }
                "manage.query_file_info" => Ok(json!({"last_modify_time":10,"status":"normal"})),
                "smartcanvas.edit" => Ok(json!({"version":3})),
                _ => panic!("unexpected fixture tool {name}"),
            }
        })();
        Box::pin(async move { result })
    });
    private_directory(&dir.join("workspace")).unwrap();
    (
        Broker {
            folder: "bound".into(),
            root: dir.join("journal"),
            workspace: dir.join("workspace"),
            remote,
            registrar: None,
        },
        state,
    )
}
fn request(v: Value) -> Request {
    serde_json::from_value(v).unwrap()
}
fn create(op: &str) -> Request {
    request(
        json!({"action":"create","task_id":"task_one","document_key":"main-plan","title":"Travel plan","content":"# Plan\nOriginal generated content","operation_id":op}),
    )
}
fn count(state: &Arc<Mutex<Cloud>>, tool: &str) -> usize {
    state
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| c.0 == tool)
        .count()
}

#[tokio::test]
async fn retention_is_scoped_persistent_and_never_starts_archival() {
    let dir = tempfile::tempdir().unwrap();
    let (b, cloud) = fixture(dir.path());
    let created = b.execute(create("retention-create"), 1).await.unwrap();
    let file = created["data"]["file_id"].as_str().unwrap();
    let read = json!({"action":"retention","file_id":file});
    let default = b.execute(request(read.clone()), 2).await.unwrap();
    assert_eq!(default["data"]["class"], "standard");
    assert_eq!(default["data"]["automatic_archive_enabled"], false);
    for class in ["keep_online", "memory", "standard"] {
        let update = json!({"action":"retention","file_id":file,"retention_class":class,
            "operation_id":format!("policy-{class}")});
        let result = b.execute(request(update.clone()), 3).await.unwrap();
        assert_eq!(result["data"]["class"], class);
        assert_eq!(
            result["data"]["excluded_from_automatic_archive"],
            class != "standard"
        );
        assert_eq!(result["data"]["automatic_archive_enabled"], false);
        assert_eq!(b.execute(request(update), 4).await.unwrap(), result);
        // A reconstructed broker reads the same policy without a memory cache.
        let restored = b.in_folder("bound");
        assert_eq!(
            restored.execute(request(read.clone()), 5).await.unwrap(),
            result
        );
    }
    let revisions = b
        .execute(request(json!({"action":"revisions","file_id":file})), 6)
        .await
        .unwrap();
    assert!(
        revisions["data"]["records"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["action"] == "retention")
            .all(|v| v["content_applied"] == false)
    );
    let writes_before = cloud
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|(name, _)| {
            !matches!(
                name.as_str(),
                "manage.folder_list" | "manage.query_file_info"
            )
        })
        .count();
    assert_eq!(writes_before, 2); // The original create and move only.
    cloud
        .lock()
        .unwrap()
        .folders
        .get_mut("task_one")
        .unwrap()
        .clear();
    assert!(b.execute(request(read), 7).await.is_err());
    assert!(
        b.execute(
            request(json!({"action":"retention","file_id":"unrelated",
        "retention_class":"memory","operation_id":"deny"})),
            8
        )
        .await
        .is_err()
    );
    assert!(
        !b.root
            .join(format!("retention-{}.json", hash("unrelated")))
            .exists()
    );
}

#[test]
fn retention_defaults_preserve_existing_request_fingerprints() {
    let r = create("legacy");
    assert!(
        serde_json::to_value(r)
            .unwrap()
            .get("retention_class")
            .is_none()
    );
    let write =
        request(json!({"action":"retention","file_id":"file_one","retention_class":"memory"}));
    assert!(write.validate().is_err()); // Explicitly journal every update.
    assert!(
        serde_json::from_value::<Request>(json!({"action":"retention","file_id":"file_one",
        "retention_class":"auto_delete"}))
        .is_err()
    );
}

#[tokio::test]
async fn task_folders_reuse_titles_and_create_only_under_bound_parent() {
    let dir = tempfile::tempdir().unwrap();
    let (b, cloud) = fixture(dir.path());
    let old = b
        .execute(
            request(json!({"action":"task_create","title":" Travel ","operation_id":"reuse-task"})),
            1,
        )
        .await
        .unwrap();
    assert_eq!(old["data"]["task_id"], "task_one");
    assert_eq!(old["data"]["reused"], true);
    let new = b
        .execute(
            request(
                json!({"action":"task_create","title":"Separate task","operation_id":"new-task"}),
            ),
            1,
        )
        .await
        .unwrap();
    assert_eq!(new["data"]["task_id"], "task_new");
    assert_eq!(count(&cloud, "manage.create_file"), 1);
}

#[tokio::test]
async fn continued_deliverable_reuses_original_and_targeted_edits_are_attributed() {
    let dir = tempfile::tempdir().unwrap();
    let (b, cloud) = fixture(dir.path());
    let first = b.execute(create("first"), 1).await.unwrap();
    assert_eq!(first["data"]["task_id"], "task_one");
    let mut continuation = create("next");
    continuation.title = "Travel plan revision two".into();
    continuation.content = "new requirement".into();
    let reused = b.execute(continuation, 2).await.unwrap();
    assert_eq!(reused["data"]["status"], "reuse_existing");
    assert_eq!(reused["data"]["file_id"], first["data"]["file_id"]);
    assert_eq!(count(&cloud, "create_smartcanvas_by_mdx"), 1);
    assert_eq!(count(&cloud, "smartcanvas.edit"), 0);
    let edited=b.execute(request(json!({"action":"append","file_id":"file_1","operation_id":"append","content":"additional requirement"})),2).await.unwrap();
    assert_eq!(edited["ok"], true);
    assert_eq!(count(&cloud, "smartcanvas.edit"), 1);
    let history = b
        .execute(request(json!({"action":"revisions","file_id":"file_1"})), 3)
        .await
        .unwrap();
    assert_eq!(history["data"]["native_revision_note_supported"], false);
    let rows = history["data"]["records"].as_array().unwrap();
    assert!(rows.iter().any(|v| v["action"] == "append"
        && v["source"] == "来自 ClawMingle"
        && v["provider_version"] == 3));
}

#[tokio::test]
async fn missing_or_foreign_task_is_rejected_before_creating_and_revoked_folder_blocks_index() {
    let dir = tempfile::tempdir().unwrap();
    let (b, cloud) = fixture(dir.path());
    let mut missing = create("missing");
    missing.task_id.clear();
    assert_eq!(
        b.execute(missing, 1).await.unwrap_err().to_string(),
        "documents_task_required"
    );
    let mut foreign = create("foreign");
    foreign.task_id = "personal_folder".into();
    assert_eq!(
        b.execute(foreign, 1).await.unwrap_err().to_string(),
        "documents_task_scope_denied"
    );
    assert_eq!(count(&cloud, "create_smartcanvas_by_mdx"), 0);
    b.execute(create("good"), 1).await.unwrap();
    cloud
        .lock()
        .unwrap()
        .folders
        .get_mut("bound")
        .unwrap()
        .clear();
    assert!(b.execute(request(json!({"action":"append","file_id":"file_1","content":"bad","operation_id":"revoked"})),2).await.is_err());
    assert_eq!(count(&cloud, "smartcanvas.edit"), 0);
}

#[tokio::test]
async fn quota_preserves_input_and_allows_only_same_undispatched_operation_retry() {
    let dir = tempfile::tempdir().unwrap();
    let (b, cloud) = fixture(dir.path());
    cloud.lock().unwrap().fail = Some(("create_smartcanvas_by_mdx".into(), true));
    let result = b.execute(create("quota"), 1).await.unwrap();
    assert_eq!(result["error"], "documents_rate_limited");
    assert_eq!(result["status"], "not_dispatched");
    let draft = b
        .execute(request(json!({"action":"draft","operation_id":"quota"})), 2)
        .await
        .unwrap();
    assert_eq!(draft["data"]["source_label"], "来自 ClawMingle");
    assert!(
        draft["data"]["request"]["content"]
            .as_str()
            .unwrap()
            .contains("Original generated")
    );
    let fallback = b
        .execute(
            request(json!({"action":"draft_export","operation_id":"quota"})),
            2,
        )
        .await
        .unwrap();
    assert_eq!(fallback["ok"], true);
    let path = fallback["data"]["file_path"].as_str().unwrap();
    assert!(
        std::fs::read_to_string(b.workspace.join(path))
            .unwrap()
            .contains("Original generated")
    );
    let other = b.execute(create("quota-another-id"), 2).await.unwrap();
    assert_eq!(other["data"]["status"], "previous_operation_pending");
    assert_eq!(b.execute(create("quota"), 3).await.unwrap()["ok"], true);
    assert_eq!(count(&cloud, "create_smartcanvas_by_mdx"), 2);
}

#[tokio::test]
async fn quota_during_move_keeps_created_identity_and_never_recreates() {
    let dir = tempfile::tempdir().unwrap();
    let (b, cloud) = fixture(dir.path());
    cloud.lock().unwrap().fail = Some(("manage.move_file".into(), true));
    let result = b.execute(create("partial"), 1).await.unwrap();
    assert_eq!(result["error"], "documents_rate_limited");
    assert_eq!(result["status"], "created_move_deferred");
    assert_eq!(result["previous_result"]["file_id"], "file_1");
    assert_eq!(b.execute(create("partial"), 2).await.unwrap(), result);
    let other = b.execute(create("new-id"), 2).await.unwrap();
    assert_eq!(other["data"]["status"], "previous_operation_pending");
    assert_eq!(count(&cloud, "create_smartcanvas_by_mdx"), 1);
}

#[tokio::test]
async fn ambiguous_create_reserves_document_role_across_new_operation_ids() {
    let dir = tempfile::tempdir().unwrap();
    let (b, cloud) = fixture(dir.path());
    cloud.lock().unwrap().fail = Some(("create_smartcanvas_by_mdx".into(), false));
    assert_eq!(
        b.execute(create("unknown"), 1).await.unwrap()["status"],
        "unknown"
    );
    assert_eq!(
        b.execute(create("another"), 2).await.unwrap()["data"]["status"],
        "previous_operation_pending"
    );
    assert_eq!(count(&cloud, "create_smartcanvas_by_mdx"), 1);
}

#[tokio::test]
async fn quota_during_preflight_still_retains_generated_content_without_remote_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let (b, cloud) = fixture(dir.path());
    cloud.lock().unwrap().fail = Some(("manage.folder_list".into(), true));
    let result = b.execute(create("preflight"), 1).await.unwrap();
    assert_eq!(result["status"], "not_dispatched");
    assert_eq!(count(&cloud, "create_smartcanvas_by_mdx"), 0);
    assert_eq!(
        b.draft("preflight").unwrap()["data"]["request"]["content"],
        create("preflight").content
    );
}

#[tokio::test]
async fn organize_legacy_document_preserves_identity_and_checks_fresh_version() {
    let dir = tempfile::tempdir().unwrap();
    let (b, cloud) = fixture(dir.path());
    cloud.lock().unwrap().folders.get_mut("bound").unwrap().push(json!({"id":"legacy","title":"Existing plan","is_folder":false,"url":"https://docs.qq.com/doc/legacy"}));
    let mut move_request = request(
        json!({"action":"organize","file_id":"legacy","task_id":"task_one","document_key":"main-plan","operation_id":"move","expected_modified_at":9}),
    );
    assert!(b.execute(move_request.clone(), 1).await.is_err());
    assert_eq!(count(&cloud, "manage.move_file"), 0);
    move_request.operation_id = "move-after-read".into();
    move_request.expected_modified_at = Some(10);
    let result = b.execute(move_request, 2).await.unwrap();
    assert_eq!(result["data"]["identity_preserved"], true);
    let reused = b.execute(create("reuse-legacy"), 3).await.unwrap();
    assert_eq!(reused["data"]["file_id"], "legacy");
    assert_eq!(count(&cloud, "create_smartcanvas_by_mdx"), 0);
}

#[tokio::test]
async fn title_reuse_keeps_original_role_alias_and_definite_move_can_resume_without_recreation() {
    let dir = tempfile::tempdir().unwrap();
    let (b, cloud) = fixture(dir.path());
    cloud.lock().unwrap().fail = Some(("manage.move_file".into(), true));
    b.execute(create("deferred"), 1).await.unwrap();
    let resumed = b
        .execute(
            request(json!({"action":"resume","operation_id":"deferred"})),
            2,
        )
        .await
        .unwrap();
    assert_eq!(resumed["ok"], true);
    assert_eq!(count(&cloud, "create_smartcanvas_by_mdx"), 1);
    let mut alias = create("alias");
    alias.document_key = "alternate-name".into();
    assert_eq!(
        b.execute(alias, 2).await.unwrap()["data"]["file_id"],
        "file_1"
    );
    let mut original = create("original-key");
    original.title = "Changed requested title".into();
    assert_eq!(
        b.execute(original, 3).await.unwrap()["data"]["file_id"],
        "file_1"
    );
    assert_eq!(count(&cloud, "create_smartcanvas_by_mdx"), 1);
}

#[tokio::test]
async fn uncertain_task_creation_reserves_title_and_prevents_duplicate_after_new_operation_id() {
    let dir = tempfile::tempdir().unwrap();
    let (b, cloud) = fixture(dir.path());
    cloud.lock().unwrap().fail = Some(("manage.create_file".into(), false));
    let input =
        json!({"action":"task_create","title":"Another task","operation_id":"task-unknown"});
    assert_eq!(
        b.execute(request(input), 1).await.unwrap()["status"],
        "unknown"
    );
    let result=b.execute(request(json!({"action":"task_create","title":"Another task","operation_id":"different-task-id"})),2).await.unwrap();
    assert_eq!(result["data"]["status"], "previous_operation_pending");
    assert_eq!(count(&cloud, "manage.create_file"), 1);
}

#[tokio::test]
async fn destination_membership_recovers_unknown_move_without_repeating_it() {
    let dir = tempfile::tempdir().unwrap();
    let (b, cloud) = fixture(dir.path());
    cloud.lock().unwrap().fail = Some(("manage.move_file".into(), false));
    b.execute(create("move-unknown"), 1).await.unwrap();
    // Simulate a move that reached the provider before the response was lost.
    let file = cloud
        .lock()
        .unwrap()
        .folders
        .get_mut("home")
        .unwrap()
        .remove(0);
    cloud
        .lock()
        .unwrap()
        .folders
        .get_mut("task_one")
        .unwrap()
        .push(file);
    let reused = b.execute(create("recover-by-list"), 2).await.unwrap();
    assert_eq!(reused["data"]["status"], "reuse_existing");
    assert_eq!(reused["data"]["file_id"], "file_1");
    assert_eq!(count(&cloud, "manage.move_file"), 1);
    assert_eq!(
        b.execute(
            request(json!({"action":"operation","operation_id":"move-unknown"})),
            2
        )
        .await
        .unwrap()["ok"],
        true
    );
}

#[tokio::test]
async fn source_checkpoint_uses_confirmed_created_id_and_scoped_edit_id() {
    let dir = tempfile::tempdir().unwrap();
    let (mut broker, _) = fixture(dir.path());
    private_directory(&broker.root).unwrap();
    let calls = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observed = calls.clone();
    let mut registrar = qq_native::NativeConfig {
        enabled: true,
        browser_state_file: Some("/private/fixture.json".into()),
        groups: BTreeMap::from([("group".into(), "12345".into())]),
    }
    .registrar("group")
    .unwrap();
    registrar.remote = Arc::new(move |action, args| {
        observed.lock().unwrap().push(args);
        Box::pin(async move {
            Ok(if action == "version_list" {
                json!({"retcode":0,"result":{"records":[],"nums":0,"page":1,"pages":0}})
            } else {
                assert_eq!(action, "version_check");
                json!({"retcode":318301})
            })
        })
    });
    broker.registrar = Some(registrar);
    for (action, tool, expected) in [
        ("create", "", "created"),
        ("invoke", "doc.create_with_markdown", "created"),
        ("invoke", "doc.insert_text", "scoped"),
    ] {
        // Native create is catalogued independently of the action name.
        let r: Request=serde_json::from_value(json!({"action":action,"tool":tool,"file_id":"scoped","operation_id":format!("source-{}",calls.lock().unwrap().len())})).unwrap();
        let mut result =
            json!({"ok":true,"data":{"file_id":"created","url":"https://docs.qq.com/doc/created"}});
        broker
            .add_source_checkpoint(
                &r,
                &json!({"url":"https://docs.qq.com/doc/scoped"}),
                &mut result,
            )
            .await;
        assert_eq!(result["ok"], true);
        assert_eq!(result["native_source_checkpoint"]["status"], "no_changes");
        assert_eq!(calls.lock().unwrap().last().unwrap()["file_id"], expected);
    }
}
