use super::*;
use std::sync::Mutex;

type Calls = Arc<Mutex<Vec<(String, Value)>>>;
fn permission() -> Value {
    json!({"retcode":0,"data":{"get_rsp":{"cmduin_privilege":1,"shutup_flag":false,"shutup_flag_me":false}}})
}
fn page(files: &[&str], next: u64) -> Value {
    let entries: Vec<_> = files.iter().map(|file| json!({
        "doc_meta_data":{"doc_id":{"domain_id":300000000,"pad_id":file},"title":"fixture","doc_status":2},
        "doc_url":format!("https://docs.qq.com/doc/{file}")
    })).collect();
    json!({"retcode":0,"data":{"doc_list":entries,"next":next}})
}
fn remote(results: Vec<(&str, Value)>) -> (Remote, Calls) {
    let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(
        results
            .into_iter()
            .map(|(action, value)| (action.to_owned(), value))
            .collect::<Vec<_>>(),
    )));
    let calls: Calls = Arc::new(Mutex::new(vec![]));
    let observed = calls.clone();
    let transport: Remote = Arc::new(move |action, args| {
        observed.lock().unwrap().push((action.clone(), args));
        let (expected, value) = queue.lock().unwrap().pop_front().expect("unexpected call");
        assert_eq!(action, expected);
        Box::pin(async move {
            if value == "timeout" {
                anyhow::bail!("secret_transport_error");
            }
            Ok(value)
        })
    });
    (transport, calls)
}
fn setup(results: Vec<(&str, Value)>) -> (tempfile::TempDir, PathBuf, Registrar, Calls) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    private_directory(&root).unwrap();
    let (remote, calls) = remote(results);
    let registrar = Registrar {
        group: "12345".into(),
        remote,
    };
    atomic_write(
        &registrar.path(&root, "file_one"),
        &state("registered").to_string(),
    )
    .unwrap();
    (dir, root, registrar, calls)
}
fn document_results(count: usize) -> (Remote, Calls) {
    remote(
        (0..count)
            .map(|_| ("manage.query_file_info", json!({"status":"trash"})))
            .collect(),
    )
}

#[test]
fn deletion_form_pins_scope_domain_and_no_copy() {
    let session = Cookies {
        header: HeaderValue::from_static("TOK=fixture"),
        xsrf: "fixture".into(),
        bkn: "5381".into(),
    };
    let form = delete_form(
        "12345",
        &json!({"file_id":"file_one","group_code":"54321","domain_id":42,"save_back":true}),
        &session,
    )
    .unwrap();
    let request = reqwest::Client::new()
        .post("http://127.0.0.1/")
        .form(&form)
        .build()
        .unwrap();
    assert_eq!(request.method(), reqwest::Method::POST);
    assert_eq!(
        request.headers()["content-type"],
        "application/x-www-form-urlencoded"
    );
    let body = std::str::from_utf8(request.body().unwrap().as_bytes().unwrap()).unwrap();
    assert!(body.contains("group_code=12345&domain_id=300000000&pad_id=file_one&save_back=false"));
    assert!(!body.contains("true"));
    assert!(delete_form("12345", &json!({"file_id":"../x"}), &session).is_err());
}

#[tokio::test]
async fn managed_trash_cleanup_confirms_absence_and_preserves_tombstone() {
    let (_dir, root, r, calls) = setup(vec![
        ("permission", permission()),
        ("list", page(&["file_one", "file_two"], 0)),
        ("cleanup_trashed", json!({"retcode":0})),
        ("permission", permission()),
        ("list", page(&["file_two"], 20)),
        ("list", page(&[], 0)),
    ]);
    let (documents, reads) = document_results(2);
    let result = r
        .cleanup_trashed(&root, "file_one", &documents)
        .await
        .unwrap();
    assert_eq!(result["status"], "removed");
    assert_eq!(result["cleanup_status"], "removed");
    assert_eq!(result["cleanup_pending"], false);
    assert_eq!(result["group_list_present"], false);
    assert_eq!(reads.lock().unwrap().len(), 2);
    assert!(
        reads
            .lock()
            .unwrap()
            .iter()
            .all(|(_, args)| args == &json!({"file_id":"file_one"}))
    );
    assert_eq!(
        calls.lock().unwrap()[2],
        ("cleanup_trashed".into(), json!({"file_id":"file_one"}))
    );
    assert_eq!(
        r.register(&root, "file_one", "https://docs.qq.com/doc/file_one")
            .await
            .unwrap(),
        result
    );
    assert!(
        !r.mark_trashed(&root, "file_one").unwrap()["cleanup_pending"]
            .as_bool()
            .unwrap()
    );
}

#[tokio::test]
async fn live_unknown_and_unreadable_statuses_never_delete_despite_local_trash() {
    for info in [json!({"status":"normal"}), json!({}), json!("timeout")] {
        let (_dir, root, r, calls) = setup(vec![
            ("permission", permission()),
            ("list", page(&["file_one"], 0)),
        ]);
        r.mark_trashed(&root, "file_one").unwrap();
        let (documents, _) = remote(vec![("manage.query_file_info", info)]);
        let error = r
            .cleanup_trashed(&root, "file_one", &documents)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("documents_native_cleanup_"));
        assert!(!error.contains("secret"));
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|(action, _)| action != "cleanup_trashed")
        );
    }
}

#[tokio::test]
async fn observed_unmanaged_entries_and_other_groups_do_not_gain_delete_authority() {
    let (_dir, root, r, calls) = setup(vec![]);
    let (documents, reads) = document_results(0);
    r.observed_in_list(&root, "file_other").unwrap();
    r.mark_trashed(&root, "file_other").unwrap();
    assert!(
        r.cleanup_trashed(&root, "file_other", &documents)
            .await
            .is_err()
    );
    let other = Registrar {
        group: "54321".into(),
        remote: r.remote.clone(),
    };
    assert!(
        other
            .cleanup_trashed(&root, "file_one", &documents)
            .await
            .is_err()
    );
    assert!(
        r.cleanup_trashed(&root, "../bad", &documents)
            .await
            .is_err()
    );
    // Old list-only records whose creation provenance was lost stay denied.
    let mut record = state("registered");
    record["verification"] = json!("live_group_list");
    atomic_write(&r.path(&root, "file_unknown"), &record.to_string()).unwrap();
    assert!(
        r.cleanup_trashed(&root, "file_unknown", &documents)
            .await
            .is_err()
    );
    assert!(calls.lock().unwrap().is_empty());
    assert!(reads.lock().unwrap().is_empty());
    // A list read of a real host registration preserves its provenance.
    assert_eq!(
        r.observed_in_list(&root, "file_one").unwrap()["managed_registration"],
        true
    );
}

#[tokio::test]
async fn already_absent_trashed_entry_is_verified_without_post() {
    let (_dir, root, r, calls) = setup(vec![
        ("permission", permission()),
        ("list", page(&["file_two"], 0)),
    ]);
    let (documents, _) = document_results(1);
    assert_eq!(
        r.cleanup_trashed(&root, "file_one", &documents)
            .await
            .unwrap()["cleanup_status"],
        "removed"
    );
    assert_eq!(calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn ambiguous_dispatch_reconciles_but_never_repeats_post() {
    for response in [json!("timeout"), json!({"retcode":0}), json!({})] {
        let (_dir, root, r, calls) = setup(vec![
            ("permission", permission()),
            ("list", page(&["file_one"], 0)),
            ("cleanup_trashed", response),
            ("permission", permission()),
            ("list", page(&["file_one"], 0)),
            ("permission", permission()),
            ("list", page(&["file_one"], 0)),
            ("permission", permission()),
            ("list", page(&[], 0)),
        ]);
        let (documents, _) = document_results(3);
        for _ in 0..2 {
            assert_eq!(
                r.cleanup_trashed(&root, "file_one", &documents)
                    .await
                    .unwrap()["cleanup_status"],
                "unknown"
            );
        }
        assert_eq!(
            r.cleanup_trashed(&root, "file_one", &documents)
                .await
                .unwrap()["cleanup_status"],
            "removed"
        );
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(name, _)| name == "cleanup_trashed")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn timeout_with_verified_absence_is_success_and_failed_readback_stays_unknown() {
    for absent in [true, false] {
        let (_dir, root, r, _) = setup(vec![
            ("permission", permission()),
            ("list", page(&["file_one"], 0)),
            ("cleanup_trashed", json!("timeout")),
            ("permission", permission()),
            (
                "list",
                if absent {
                    page(&[], 0)
                } else {
                    json!("timeout")
                },
            ),
        ]);
        let (documents, _) = document_results(if absent { 2 } else { 1 });
        let result = r
            .cleanup_trashed(&root, "file_one", &documents)
            .await
            .unwrap();
        assert_eq!(
            result["cleanup_status"],
            if absent { "removed" } else { "unknown" }
        );
        if !absent {
            assert!(result["group_list_present"].is_null());
        }
        assert!(!result.to_string().contains("secret"));
    }
}

#[tokio::test]
async fn incomplete_list_or_denied_permission_never_authorizes_cleanup() {
    for sequence in [
        vec![("permission", json!({"retcode":1}))],
        vec![
            ("permission", permission()),
            ("list", page(&["file_two"], 20)),
            ("list", json!("timeout")),
        ],
        vec![("permission", permission()), ("list", json!({"retcode":0}))],
    ] {
        let (_dir, root, r, calls) = setup(sequence);
        let (documents, _) = document_results(0);
        assert!(
            r.cleanup_trashed(&root, "file_one", &documents)
                .await
                .is_err()
        );
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|(name, _)| name != "cleanup_trashed")
        );
    }
}

#[tokio::test]
async fn cancellation_at_dispatch_leaves_unknown_journal() {
    let (_dir, root, mut r, _) = setup(vec![]);
    r.remote = Arc::new(|action, _| {
        Box::pin(async move {
            match action.as_str() {
                "permission" => Ok(permission()),
                "list" => Ok(page(&["file_one"], 0)),
                "cleanup_trashed" => std::future::pending().await,
                _ => panic!("unexpected call"),
            }
        })
    });
    let (documents, _) = document_results(1);
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(25),
            r.cleanup_trashed(&root, "file_one", &documents)
        )
        .await
        .is_err()
    );
    assert_eq!(
        r.status(&root, "file_one").unwrap()["cleanup_status"],
        "unknown"
    );
}

#[test]
fn cookies_match_api_path_without_leaking_cgi_only_cookies() {
    let cookie = |name: &str, path: &str| json!({"name":name,"value":"fixture","domain":".docs.qq.com","path":path,"expires":-1.0});
    let raw = json!({"cookies":[cookie("TOK","/"),cookie("root","/"),cookie("cgi","/cgi-bin"),cookie("api","/api"),cookie("record","/api/file/record/"),cookie("lookalike","/api/file/records"),cookie("invalid",""),cookie("sibling","/api/file/record/version/save")] }).to_string();
    let session = cookies_for_path(&raw, 100.0, "/api/file/record/version/get_list").unwrap();
    assert_eq!(
        session.header.to_str().unwrap(),
        "record=fixture; api=fixture; TOK=fixture; root=fixture"
    );
    let native = cookies(&raw, 100.0).unwrap();
    assert_eq!(
        native.header.to_str().unwrap(),
        "cgi=fixture; TOK=fixture; root=fixture"
    );
    assert!(cookies_for_path(&raw, 100.0, "api/file").is_err());
}

#[tokio::test]
async fn explicit_rejection_is_retryable_but_only_after_fresh_trash_check() {
    let (_dir, root, r, calls) = setup(vec![
        ("permission", permission()),
        ("list", page(&["file_one"], 0)),
        (
            "cleanup_trashed",
            json!({"retcode":403,"message":"secret_provider_text"}),
        ),
        ("permission", permission()),
        ("list", page(&["file_one"], 0)),
        ("permission", permission()),
        ("list", page(&["file_one"], 0)),
    ]);
    let (documents, _) = remote(vec![
        ("manage.query_file_info", json!({"status":"trash"})),
        ("manage.query_file_info", json!({"status":"normal"})),
    ]);
    let result = r
        .cleanup_trashed(&root, "file_one", &documents)
        .await
        .unwrap();
    assert_eq!(result["cleanup_status"], "rejected");
    assert!(!result.to_string().contains("secret"));
    assert!(
        r.cleanup_trashed(&root, "file_one", &documents)
            .await
            .is_err()
    );
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(action, _)| action == "cleanup_trashed")
            .count(),
        1
    );
}

#[tokio::test]
async fn disappearance_without_confirmed_trash_is_not_completed() {
    let (_dir, root, r, _) = setup(vec![
        ("permission", permission()),
        ("list", page(&["file_one"], 0)),
        ("cleanup_trashed", json!({"retcode":0})),
        ("permission", permission()),
        ("list", page(&[], 0)),
    ]);
    let (documents, _) = remote(vec![
        ("manage.query_file_info", json!({"status":"trash"})),
        ("manage.query_file_info", json!({"status":"normal"})),
    ]);
    let result = r
        .cleanup_trashed(&root, "file_one", &documents)
        .await
        .unwrap();
    assert_eq!(result["cleanup_status"], "unknown");
    assert_eq!(result["group_list_present"], false);
    assert_eq!(result["cleanup_pending"], true);
    assert!(result["document_status"].is_null());
}

#[tokio::test]
async fn broker_reconciles_unknown_cleanup_with_same_operation_without_redeleting() {
    let (dir, root, registrar, calls) = setup(vec![
        ("permission", permission()),
        ("list", page(&["file_one"], 0)),
        ("cleanup_trashed", json!("timeout")),
        ("permission", json!("timeout")),
        ("permission", permission()),
        ("list", page(&[], 0)),
    ]);
    let (documents, _) = document_results(2);
    let broker = Broker {
        registrar: Some(registrar),
        folder: "bound".into(),
        root,
        remote: documents,
        workspace: dir.path().join("workspace"),
    };
    let request = || {
        serde_json::from_value(
            json!({"action":"qq_cleanup","file_id":"file_one","operation_id":"cleanup-one"}),
        )
        .unwrap()
    };
    let first = broker.execute(request(), 0).await.unwrap();
    assert_eq!(first["ok"], false);
    assert_eq!(first["status"], "unknown");
    let second = broker.execute(request(), 0).await.unwrap();
    assert_eq!(second["ok"], true);
    assert_eq!(second["status"], "removed");
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == "cleanup_trashed")
            .count(),
        1
    );
    let revisions = broker.revisions("file_one").unwrap();
    assert_eq!(revisions["records"][0]["content_applied"], false);
}
