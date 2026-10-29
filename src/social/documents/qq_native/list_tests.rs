use super::*;
use std::sync::Mutex;

fn permission() -> Value {
    json!({"retcode":0,"data":{"get_rsp":{"cmduin_privilege":1,"shutup_flag":false,"shutup_flag_me":false}}})
}
fn page(next: u64) -> Value {
    json!({"retcode":0,"data":{"next":next,"doc_list":[{
        "doc_meta_data":{"doc_id":{"domain_id":"300000000","pad_id":"file_one"},"title":"Task\nplan","doc_status":2,"creator_uin":"private_account"},
        "doc_url":"https://docs.qq.com/doc/file_one?private=hidden#fragment",
        "creator_nick":"private_nickname","owner_nick":"private_owner","doc_thumbnail":"private_thumbnail"
    }]}})
}
type Calls = Arc<Mutex<Vec<(String, Value)>>>;
fn fake(values: Vec<Value>) -> (Registrar, Calls) {
    let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(values)));
    let calls = Arc::new(Mutex::new(vec![]));
    let record = calls.clone();
    let remote: Remote = Arc::new(move |action, args| {
        record.lock().unwrap().push((action, args));
        let value = queue
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected remote call");
        Box::pin(async move {
            if value == "private_transport_error" {
                anyhow::bail!("private_transport_error")
            }
            Ok(value)
        })
    });
    (
        Registrar {
            group: "12345".into(),
            remote,
        },
        calls,
    )
}

#[test]
fn page_sanitizes_identity_fields_and_preserves_uninterpreted_provider_status() {
    let result = list_result(&page(1), 0).unwrap();
    assert_eq!(result["next_start"], 1);
    assert_eq!(result["verification"], "live_group_list");
    assert_eq!(result["provider_status_interpreted"], false);
    assert_eq!(
        result["entries"][0],
        json!({
            "file_id":"file_one","title":"Taskplan",
            "url":"https://docs.qq.com/doc/file_one","provider_status":2
        })
    );
    assert!(!result.to_string().contains("private"));
    assert!(result["entries"][0].get("document_status").is_none());
    assert!(list_result(&page(0), 0).unwrap()["next_start"].is_null());
    let mut encoded = page(0);
    encoded["data"]["doc_list"][0]["doc_meta_data"]["doc_id"]["pad_id"] = json!("abcdefghijkl");
    encoded["data"]["doc_list"][0]["doc_url"] =
        json!("https://10000.docs.qq.com/doc/DYWJjZGVmZ2hpamts?private=hidden");
    let result = list_result(&encoded, 0).unwrap();
    assert_eq!(result["entries"][0]["file_id"], "abcdefghijkl");
    assert_eq!(
        result["entries"][0]["url"],
        "https://10000.docs.qq.com/doc/DYWJjZGVmZ2hpamts"
    );
    encoded["data"]["doc_list"][0]["doc_meta_data"]["doc_id"]["pad_id"] = json!("different_id");
    assert!(list_result(&encoded, 0).is_err());
}

#[test]
fn rejected_and_malformed_pages_never_become_successful_empty_lists() {
    let mut bad = vec![
        json!({"retcode":1,"msg":"private_provider_message"}),
        json!({"retcode":0}),
    ];
    for (path, value) in [
        (
            "/data/doc_list/0/doc_meta_data/doc_id/pad_id",
            json!("../other"),
        ),
        ("/data/doc_list/0/doc_meta_data/doc_id/domain_id", json!(1)),
        ("/data/doc_list/0/doc_meta_data/title", Value::Null),
        ("/data/doc_list/0/doc_meta_data/doc_status", json!("trash")),
        (
            "/data/doc_list/0/doc_url",
            json!("https://example.com/doc/file_one"),
        ),
        (
            "/data/doc_list/0/doc_url",
            json!("https://docs.qq.com/doc/other_file"),
        ),
        (
            "/data/doc_list/0/doc_url",
            json!("https://user@docs.qq.com/doc/file_one"),
        ),
        (
            "/data/doc_list/0/doc_url",
            json!("https://docs.qq.com:444/doc/file_one"),
        ),
        ("/data/next", json!(-1)),
        ("/data/next", json!(MAX_LIST_START + 1)),
    ] {
        let mut v = page(0);
        *v.pointer_mut(path).unwrap() = value;
        bad.push(v);
    }
    let mut duplicate = page(0);
    let item = duplicate["data"]["doc_list"][0].clone();
    duplicate["data"]["doc_list"]
        .as_array_mut()
        .unwrap()
        .push(item);
    bad.push(duplicate);
    let mut oversized = page(0);
    oversized["data"]["doc_list"] = json!(vec![
        page(0)["data"]["doc_list"][0].clone();
        LIST_PAGE_SIZE + 1
    ]);
    bad.push(oversized);
    for input in bad {
        let error = list_result(&input, 0).unwrap_err().to_string();
        assert!(error.starts_with("documents_native_list_"));
        assert!(!error.contains("private"));
    }
    assert!(list_result(&page(2), 2).is_err());
    assert!(list_result(&json!({"retcode":0,"data":{"doc_list":[],"next":1}}), 0).is_err());
    assert_eq!(
        list_result(&json!({"retcode":0,"data":{"doc_list":[],"next":0}}), 0).unwrap()["entries"],
        json!([])
    );
}

#[test]
fn list_query_pins_group_root_and_limit_instead_of_accepting_caller_targets() {
    let session = Cookies {
        header: HeaderValue::from_static("TOK=test_csrf"),
        xsrf: "test_csrf".into(),
        bkn: "5381".into(),
    };
    let query: BTreeMap<_, _> = list_query(
        "12345",
        &json!({"start":7,"group_code":"other","folder_id":"other","limit":100}),
        &session,
    )
    .unwrap()
    .into_iter()
    .collect();
    assert_eq!(query["group_code"], "12345");
    assert_eq!(query["folder_id"], "/");
    assert_eq!(query["limit"], "20");
    assert_eq!(query["start"], "7");
    assert!(!query.contains_key("num"));
    assert!(list_query("12345", &json!({"start":10001}), &session).is_err());
    assert!(list_query("12345", &json!({"start":-1}), &session).is_err());
}

#[tokio::test]
async fn list_requires_fresh_permission_and_keeps_failed_reads_distinct() {
    let (r, calls) = fake(vec![permission(), page(8)]);
    assert_eq!(r.list(7).await.unwrap()["next_start"], 8);
    assert_eq!(
        *calls.lock().unwrap(),
        vec![
            ("permission".into(), Value::Null),
            ("list".into(), json!({"start":7}))
        ]
    );
    let (r, calls) = fake(vec![json!({"retcode":1})]);
    assert_eq!(
        r.list(0).await.unwrap_err().to_string(),
        "documents_native_permission_denied"
    );
    assert_eq!(calls.lock().unwrap().len(), 1);
    let (r, calls) = fake(vec![]);
    assert!(r.list(MAX_LIST_START + 1).await.is_err());
    assert!(calls.lock().unwrap().is_empty());
    let (r, _) = fake(vec![permission(), json!("private_transport_error")]);
    assert_eq!(
        r.list(0).await.unwrap_err().to_string(),
        "documents_native_unavailable"
    );
    assert_eq!(
        list_remote_error(anyhow::anyhow!("documents_native_login_required")).to_string(),
        "documents_native_login_required"
    );
}

#[tokio::test]
async fn trash_annotation_is_local_scope_bound_and_preserves_registration_idempotency() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state");
    private_directory(&root).unwrap();
    let (r, calls) = fake(vec![]);
    assert!(!r.known_registration(&root, "file_one").unwrap());
    assert!(r.mark_trashed(&root, "file_one").is_err());
    assert!(r.known_registration(&root, "../file_one").is_err());
    for status in ["registered", "unknown"] {
        atomic_write(&r.path(&root, "file_one"), &state(status).to_string()).unwrap();
        assert!(r.known_registration(&root, "file_one").unwrap());
        let value = r.mark_trashed(&root, "file_one").unwrap();
        assert_eq!(value["status"], status);
        assert_eq!(value["document_status"], "trash");
        assert_eq!(value["cleanup_pending"], true);
        assert_eq!(value["verification"], "last_registration_ack_not_live_list");
        assert!(value.get("unlinked").is_none());
        assert_eq!(
            r.register(&root, "file_one", "https://docs.qq.com/doc/file_one")
                .await
                .unwrap(),
            value
        );
        let other = Registrar {
            group: "54321".into(),
            remote: r.remote.clone(),
        };
        assert!(!other.known_registration(&root, "file_one").unwrap());
    }
    atomic_write(&r.path(&root, "file_one"), &state("rejected").to_string()).unwrap();
    assert!(!r.known_registration(&root, "file_one").unwrap());
    assert!(calls.lock().unwrap().is_empty());
}
