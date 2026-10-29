use super::*;
use std::sync::Mutex;

fn permission() -> Value {
    json!({"retcode":0,"data":{"get_rsp":{"cmduin_privilege":1,"shutup_flag":false,"shutup_flag_me":false}}})
}
fn registered(file: &str) -> Value {
    json!({"retcode":0,"data":{"doc_info":{"doc_meta_data":{"doc_id":{"domain_id":300000000,"pad_id":file}}}}})
}
fn fake(results: Vec<(&str, Value)>) -> (Registrar, Arc<Mutex<Vec<String>>>) {
    let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(
        results
            .into_iter()
            .map(|(s, v)| (s.to_owned(), v))
            .collect::<Vec<_>>(),
    )));
    let calls = Arc::new(Mutex::new(vec![]));
    let record = calls.clone();
    let remote: Remote = Arc::new(move |action, _| {
        record.lock().unwrap().push(action.clone());
        let (expected, value) = queue
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected native call");
        assert_eq!(expected, action);
        Box::pin(async move {
            if value == "timeout" {
                anyhow::bail!("transport")
            } else {
                Ok(value)
            }
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
fn validates_bound_groups_and_credential_not_arbitrary_model_parameters() {
    let mut c = NativeConfig::default();
    let bindings = BTreeMap::from([("test_group".into(), "folder".into())]);
    c.groups.insert("other_group".into(), "12345".into());
    assert!(c.validate(&bindings).is_err());
    c.groups = BTreeMap::from([("test_group".into(), "12345".into())]);
    assert!(c.validate(&bindings).is_ok());
    c.enabled = true;
    assert!(c.validate(&bindings).is_err());
    c.browser_state_file = Some("relative".into());
    assert!(c.validate(&bindings).is_err());
    c.browser_state_file = Some("/private/state.json".into());
    assert!(c.validate(&bindings).is_ok());
    assert!(c.registrar("other_group").is_none());
}
#[test]
fn provider_ack_must_identify_same_original_document() {
    assert_eq!(
        registration_result(&registered("file_one"), "file_one")["status"],
        "registered"
    );
    assert_eq!(
        registration_result(&registered("copy_two"), "file_one")["status"],
        "unknown"
    );
    assert_eq!(
        registration_result(&json!({"retcode":0}), "file_one")["status"],
        "unknown"
    );
    assert_eq!(
        registration_result(&json!({"retcode":1}), "file_one")["status"],
        "rejected"
    );
    let mut v = registered("file_one");
    v["data"]["doc_info"]["doc_meta_data"]["doc_id"]["domain_id"] = json!(1);
    assert_eq!(registration_result(&v, "file_one")["status"], "unknown");
}
#[test]
fn only_explicit_group_permission_and_verified_types_pass() {
    assert!(group_allowed(&permission()));
    for path in [
        "/retcode",
        "/data/get_rsp/shutup_flag_me",
        "/data/get_rsp/cmduin_privilege",
    ] {
        let mut p = permission();
        *p.pointer_mut(path).unwrap() = Value::Null;
        assert!(!group_allowed(&p));
    }
    let mut p = permission();
    p["data"]["get_rsp"]["cmduin_privilege"] = json!(0);
    p["data"]["get_rsp"]["shutup_flag"] = json!(true);
    assert!(!group_allowed(&p));
    assert!(verified_url("//docs.qq.com/doc/test"));
    for bad in [
        "https://example.com/doc/test",
        "https://docs.qq.com:444/doc/test",
        "https://user@docs.qq.com/doc/test",
        "https://docs.qq.com/pdf/test",
        "",
    ] {
        assert!(!verified_url(bad));
    }
}
#[test]
fn cookies_are_endpoint_scoped_expiry_checked_and_header_safe() {
    let cookie = |name: &str, domain: &str, value: &str, expires: f64| json!({"name":name,"domain":domain,"path":"/","expires":expires,"value":value});
    let list = vec![
        cookie("TOK", ".docs.qq.com", "test_csrf", -1.0),
        cookie("session", ".qq.com", "test_session", 200.0),
        cookie("excluded", ".graph.qq.com", "not_sent", -1.0),
        cookie("expired", ".qq.com", "not_sent", 99.0),
    ];
    let c = cookies(&json!({"cookies":list}).to_string(), 100.0).unwrap();
    assert_eq!(
        c.header.to_str().unwrap(),
        "TOK=test_csrf; session=test_session"
    );
    assert_eq!(c.bkn, "5381");
    assert!(c.header.is_sensitive());
    assert!(
        cookies(
            &json!({"cookies":[cookie("TOK",".docs.qq.com","a\r\nb",-1.0)]}).to_string(),
            100.0
        )
        .is_err()
    );
    assert!(
        cookies(
            &json!({"cookies":[cookie("TOK",".docs.qq.com","gone",99.0)]}).to_string(),
            100.0
        )
        .is_err()
    );
}
#[tokio::test]
async fn successful_and_uncertain_posts_never_repeat_even_with_different_operation() {
    for response in [
        registered("file_one"),
        json!("timeout"),
        registered("unexpected_copy"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            dir.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let (r, calls) = fake(vec![("permission", permission()), ("register", response)]);
        let first = r
            .register(dir.path(), "file_one", "//docs.qq.com/doc/test")
            .await
            .unwrap();
        assert_eq!(
            r.register(dir.path(), "file_one", "//docs.qq.com/doc/test")
                .await
                .unwrap(),
            first
        );
        assert_eq!(*calls.lock().unwrap(), vec!["permission", "register"]);
    }
}
#[tokio::test]
async fn rejected_permission_does_not_send_post_and_explicit_retry_can_recover() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let (r, calls) = fake(vec![
        ("permission", json!({"retcode":1})),
        ("permission", permission()),
        ("register", registered("file_one")),
    ]);
    assert_eq!(
        r.register(dir.path(), "file_one", "//docs.qq.com/doc/test")
            .await
            .unwrap()["status"],
        "rejected"
    );
    assert_eq!(
        r.register(dir.path(), "file_one", "//docs.qq.com/doc/test")
            .await
            .unwrap()["status"],
        "registered"
    );
    assert_eq!(
        *calls.lock().unwrap(),
        vec!["permission", "permission", "register"]
    );
}
#[tokio::test]
async fn cancellation_after_post_is_durable_unknown() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let remote: Remote = Arc::new(|action, _| {
        Box::pin(async move {
            if action == "permission" {
                Ok(permission())
            } else {
                std::future::pending().await
            }
        })
    });
    let r = Registrar {
        group: "12345".into(),
        remote,
    };
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(25),
            r.register(dir.path(), "file_one", "//docs.qq.com/doc/test")
        )
        .await
        .is_err()
    );
    assert_eq!(
        r.status(dir.path(), "file_one").unwrap()["status"],
        "unknown"
    );
}

#[tokio::test]
async fn interrupted_registration_preserves_created_document_and_never_recreates() {
    let dir = tempfile::tempdir().unwrap();
    let count = Arc::new(Mutex::new(0));
    let counter = count.clone();
    let remote: Remote = Arc::new(move |name, args| {
        *counter.lock().unwrap() += 1;
        Box::pin(async move {
            Ok(match name.as_str() {
                "doc.create_with_markdown" => {
                    json!({"file_id":"file_one","file_url":"//docs.qq.com/doc/test"})
                }
                "manage.move_file" => json!({}),
                "manage.folder_list" if args["folder_id"] == "folder" => {
                    json!({"list":[{"id":"task_one","is_folder":true}],"finish":true})
                }
                "manage.folder_list" => {
                    json!({"list":[{"id":"file_one","is_folder":false,"url":"//docs.qq.com/doc/test"}],"finish":true})
                }
                _ => panic!("unexpected call"),
            })
        })
    });
    let native: Remote = Arc::new(|action, _| {
        Box::pin(async move {
            if action == "permission" {
                Ok(permission())
            } else {
                std::future::pending().await
            }
        })
    });
    let broker = Broker {
        folder: "folder".into(),
        root: dir.path().join("journal"),
        workspace: dir.path().join("workspace"),
        remote,
        registrar: Some(Registrar {
            group: "12345".into(),
            remote: native,
        }),
    };
    let request = || {
        serde_json::from_value(json!({"action":"create","task_id":"task_one","document_key":"main","document_type":"doc","title":"Test","operation_id":"test-create"})).unwrap()
    };
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(25),
            broker.execute(request(), 0)
        )
        .await
        .is_err()
    );
    let result = broker.execute(request(), 0).await.unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["data"]["file_id"], "file_one");
    assert_eq!(*count.lock().unwrap(), 5);
    assert_eq!(
        broker
            .registrar
            .as_ref()
            .unwrap()
            .status(&broker.root, "file_one")
            .unwrap()["status"],
        "unknown"
    );
}
