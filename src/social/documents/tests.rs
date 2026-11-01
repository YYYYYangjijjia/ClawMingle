use super::*;
use std::sync::Mutex;
type Calls = Arc<Mutex<Vec<(String, Value)>>>;

#[test]
fn remote_routes_preserve_legacy_names_and_reject_unregistered_targets() {
    for name in [
        "manage.folder_list",
        "manage.create_file",
        "create_smartcanvas_by_mdx",
        "manage.move_file",
        "manage.query_file_info",
        "manage.rename_file_title",
        "manage.delete_file",
        "smartcanvas.get_top_level_pages",
        "smartcanvas.read",
        "smartcanvas.find",
        "smartcanvas.edit",
        "create_mind_by_markdown",
        "create_flowchart_by_mermaid",
        "manage.pre_import",
        "manage.async_import",
        "manage.import_progress",
        "manage.export_file",
        "manage.export_progress",
    ] {
        assert_eq!(
            remote_route(name).unwrap(),
            ("https://docs.qq.com/openapi/mcp", name)
        );
    }
    for name in ["", "unknown", "https://example.com", "manage.unregistered"] {
        assert!(remote_route(name).is_err());
    }
}

/// Private operator harness. Request and response stay in protected local files;
/// it neither runs a model nor sends QQ messages. Opt-in integration only.
#[tokio::test]
#[ignore = "requires explicit Tencent Docs authorization and private request/response files"]
async fn live_document_operation() {
    let config_path = std::env::var("CLAWMINGLE_DOCUMENTS_EVAL_CONFIG").unwrap();
    let input = std::env::var("CLAWMINGLE_DOCUMENTS_EVAL_INPUT").unwrap();
    let output = std::env::var("CLAWMINGLE_DOCUMENTS_EVAL_OUTPUT").unwrap();
    let config = crate::social::SocialConfig::load(Path::new(&config_path)).unwrap();
    assert_eq!(config.documents.bindings.len(), 1);
    let group = config.documents.bindings.keys().next().unwrap();
    let raw = private_read_limit(Path::new(&input), 200000).unwrap();
    let result = query(&config, group, 0)(raw).await;
    atomic_write(Path::new(&output), &result.to_string()).unwrap();
    println!(
        "Private document operation result saved; ok={}",
        result["ok"]
    );
}

/// Explicit opt-in. Creates one uniquely named test document in the sole bound
/// group folder, exercises the production callback, then moves it to the recycle
/// bin. Never touches existing documents or sends QQ messages.
#[tokio::test]
#[ignore = "requires explicit Tencent Docs authorization and CLAWMINGLE_DOCUMENTS_EVAL_CONFIG"]
async fn live_document_crud() {
    let path = std::env::var("CLAWMINGLE_DOCUMENTS_EVAL_CONFIG").unwrap();
    let operation = std::env::var("CLAWMINGLE_DOCUMENTS_EVAL_OPERATION").unwrap();
    let config = crate::social::SocialConfig::load(Path::new(&path)).unwrap();
    assert_eq!(config.documents.bindings.len(), 1);
    assert!(operation.starts_with("validation-") && valid_id(&operation));
    let group = config.documents.bindings.keys().next().unwrap();
    let query = query(&config, group, 0);
    let run = |input: Value| {
        let query = query.clone();
        async move {
            let result = query(input.to_string()).await;
            assert_eq!(result["ok"], true, "document step failed: {result}");
            result["data"].clone()
        }
    };
    run(json!({"action":"list"})).await;
    let create = json!({"action":"create","operation_id":format!("{operation}-create"),
        "title":"ClawMingle 接入验证临时文档","content":"# 接入验证\n\n校验原文甲\n\n- [ ] 测试待办"});
    let created = run(create.clone()).await;
    let file = created["file_id"].as_str().unwrap();
    assert_eq!(run(create).await["file_id"], file);
    let pages = run(json!({"action":"read","file_id":file})).await;
    let page = pages["pages"][0]["page_id"]
        .as_str()
        .expect("test document must contain a page");
    let mut token = String::new();
    let mut content = String::new();
    for _ in 0..10 {
        let part =
            run(json!({"action":"read","file_id":file,"page_id":page,"next_token":token})).await;
        content.push_str(part["content"].as_str().expect("page content"));
        token = part["next_token"].as_str().unwrap_or_default().to_owned();
        if token.is_empty() {
            break;
        }
    }
    assert!(
        token.is_empty(),
        "test document unexpectedly exceeded ten pages"
    );
    assert!(content.contains("校验原文甲"));
    println!("document create, idempotent replay and read passed");
    run(
        json!({"action":"append","file_id":file,"operation_id":format!("{operation}-append"),
        "content":"追加验证乙"}),
    )
    .await;
    let found = run(json!({"action":"find","file_id":file,"query":"校验原文甲"})).await;
    let mut edit = json!({"action":"edit","file_id":file,"operation_id":format!("{operation}-edit"),
        "query":"校验原文甲","content":"已修订原文甲"});
    if let Some(block) = found["blocks"].as_array().unwrap().first() {
        edit["block_id"] = block["id"].clone();
        edit["expected_content"] = block["content"].clone();
    } else {
        let mut cursor = String::new();
        for _ in 0..10 {
            let part =
                run(json!({"action":"read","file_id":file,"page_id":page,"next_token":cursor}))
                    .await;
            let text = part["content"].as_str().unwrap();
            let block = text
                .lines()
                .filter_map(|line| page_tag(line).ok())
                .filter_map(|tag| tag.id)
                .find(|id| page_block(text, id).is_ok_and(|block| block.contains("校验原文甲")));
            if let Some(block) = block {
                edit["block_id"] = json!(block);
                edit["expected_content"] = json!(text);
                edit["page_id"] = json!(page);
                edit["next_token"] = json!(cursor);
                break;
            }
            cursor = part["next_token"].as_str().unwrap_or_default().to_owned();
            if cursor.is_empty() {
                break;
            }
        }
        assert!(
            edit["block_id"].is_string(),
            "test paragraph must exist in fresh page content"
        );
    }
    run(edit.clone()).await;
    let mut observed = false;
    for _ in 0..5 {
        let mut cursor = String::new();
        for _ in 0..10 {
            let changed =
                run(json!({"action":"read","file_id":file,"page_id":page,"next_token":cursor}))
                    .await;
            observed |= changed["content"]
                .as_str()
                .unwrap()
                .contains("已修订原文甲");
            cursor = changed["next_token"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if cursor.is_empty() {
                break;
            }
        }
        if observed {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(
        observed,
        "provider acknowledged edit but subsequent reads did not show it"
    );
    println!("document append, find and conflict-checked edit passed");
    let info = run(json!({"action":"inspect","file_id":file})).await;
    run(
        json!({"action":"rename","file_id":file,"operation_id":format!("{operation}-rename"),
        "expected_modified_at":info["last_modify_time"],"title":"ClawMingle 验证完成待回收"}),
    )
    .await;
    let info = run(json!({"action":"inspect","file_id":file})).await;
    run(
        json!({"action":"delete","file_id":file,"operation_id":format!("{operation}-delete"),
        "expected_modified_at":info["last_modify_time"]}),
    )
    .await;
    let unavailable = query(json!({"action":"inspect","file_id":file}).to_string()).await;
    assert_eq!(unavailable["error"], "documents_scope_denied");
    println!(
        "document rename and recycle-bin delete passed; test document no longer accessible through group tool"
    );
}

#[test]
fn pages_normalize_actual_and_documented_responses_without_account_metadata() {
    let actual = json!({"top_level_pages":[{"id":"page_one","created_by":"private_account","children":["block_one"]}]});
    assert_eq!(
        page_list(actual).unwrap(),
        json!({"pages":[{"page_id":"page_one","title":null}]})
    );
    assert_eq!(
        page_list(json!({"pages":[{"page_id":"page_one","title":"Plan"}]})).unwrap(),
        json!({"pages":[{"page_id":"page_one","title":"Plan"}]})
    );
    assert!(page_list(json!({"top_level_pages":[{"id":"../invalid"}]})).is_err());
}

#[test]
fn page_edit_accepts_only_complete_unique_top_level_text_blocks() {
    let text = "<Unsupported readonly id=\"other\">\n  old table\n</Unsupported>\n\n<Paragraph id=\"target\">\n  hello\n</Paragraph>";
    assert_eq!(
        page_block(text, "target").unwrap(),
        "<Paragraph id=\"target\">\n  hello\n</Paragraph>"
    );
    assert!(!readonly_component(&page_block(text, "target").unwrap()));
    for text in [
        "```md\n<Paragraph id=\"target\">\n  fake\n</Paragraph>\n```",
        "<Callout readonly id=\"parent\">\n  <Paragraph id=\"target\">\n  nested\n  </Paragraph>\n</Callout>",
        "<Paragraph id=\"target\">\n  incomplete",
        "<Paragraph id=\"target\">\n  one\n</Paragraph>\n<Paragraph id=\"target\">\n  two\n</Paragraph>",
        "`\n<Paragraph id=\"target\">\n  fake\n</Paragraph>\n`",
        "{/* example\n<Paragraph id=\"target\">\n  fake\n</Paragraph>\n*/}",
        "text <!--\n<Paragraph id=\"target\">\n  fake\n</Paragraph>\n-->",
        "```text\n``` ```\n<Paragraph id=\"target\">\n  fake\n</Paragraph>",
        "    <Paragraph id=\"target\">\n      fake\n    </Paragraph>",
        "<Paragraph label=' id=\"target\"'>\n  fake\n</Paragraph>",
    ] {
        assert!(page_block(text, "target").is_err());
    }
    assert_eq!(
        modified_at(&json!({"last_modify_time":"12345"})),
        Some(12345)
    );
    assert_eq!(modified_at(&json!({"last_modify_time":12345})), Some(12345));
}

#[tokio::test]
async fn page_edit_rechecks_fragment_and_sends_only_requested_block_update() {
    let dir = tempfile::tempdir().unwrap();
    let old = "<Paragraph id=\"block_one\">\n  old\n</Paragraph>";
    let input = json!({"action":"edit","operation_id":"page_edit","file_id":"doc_one","page_id":"page_one",
        "next_token":"cursor","block_id":"block_one","expected_content":old,"content":"new"});
    let (b, calls) = broker(
        dir.path(),
        vec![
            ("manage.folder_list", member_page()),
            ("smartcanvas.read", json!({"content":old})),
            ("smartcanvas.edit", json!({})),
        ],
    );
    assert_eq!(
        b.execute(request(input.clone()), 1).await.unwrap()["ok"],
        true
    );
    assert_eq!(calls.lock().unwrap()[1].1["next_token"], "cursor");
    assert_eq!(
        calls.lock().unwrap()[2].1,
        json!({"file_id":"doc_one","action":"UPDATE","id":"block_one","content":"new"})
    );
    let mut changed = input;
    changed["operation_id"] = json!("changed");
    let (b, calls) = broker(
        dir.path(),
        vec![
            ("manage.folder_list", member_page()),
            ("smartcanvas.read", json!({"content":"changed"})),
        ],
    );
    assert!(b.execute(request(changed), 1).await.is_err());
    assert_eq!(calls.lock().unwrap().len(), 2);
}

fn broker(root: &Path, responses: Vec<(&str, Value)>) -> (Broker, Calls) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let queue = Arc::new(Mutex::new(
        responses
            .into_iter()
            .map(|(s, v)| (s.to_owned(), v))
            .collect::<std::collections::VecDeque<_>>(),
    ));
    let remote: Remote = Arc::new(move |name, args| {
        recorded.lock().unwrap().push((name.clone(), args));
        let (expected, value) = queue
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected remote call");
        assert_eq!(expected, name);
        Box::pin(async move {
            if value == "before_dispatch" {
                return Err(anyhow::anyhow!(
                    crate::codex::app_server::scoped_integrations::DocumentNotDispatched
                ));
            }
            if value == "fail" {
                anyhow::bail!("transport_failed")
            } else {
                Ok(value)
            }
        })
    });
    (
        Broker {
            registrar: None,
            folder: "group_folder".into(),
            root: root.join("journal"),
            remote,
            workspace: root.join("workspace"),
        },
        calls,
    )
}
fn request(value: Value) -> Request {
    serde_json::from_value(value).unwrap()
}
fn member_page() -> Value {
    json!({"list":[{"id":"doc_one","is_folder":false}],"finish":true})
}

#[tokio::test]
async fn native_writes_check_scope_freshness_and_replay_without_second_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let input = json!({"action":"invoke","tool":"doc.insert_markdown","file_id":"doc_one",
        "arguments":{"idx":1,"markdown":"updated"},"operation_id":"native1","expected_modified_at":10});
    let (b, calls) = broker(
        dir.path(),
        vec![
            ("manage.folder_list", member_page()),
            ("manage.query_file_info", json!({"last_modify_time":10})),
            ("doc.insert_markdown", json!({"err_msg":""})),
        ],
    );
    let result = b.execute(request(input.clone()), 1).await.unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(
        calls.lock().unwrap()[2].1,
        json!({"file_id":"doc_one","idx":1,"markdown":"updated"})
    );
    assert_eq!(b.execute(request(input), 2).await.unwrap(), result);
    assert_eq!(calls.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn native_compare_requires_both_documents_in_current_folder() {
    let dir = tempfile::tempdir().unwrap();
    let (b, calls) = broker(
        dir.path(),
        vec![
            ("manage.folder_list", member_page()),
            ("manage.folder_list", member_page()),
        ],
    );
    let input = json!({"action":"invoke","file_id":"doc_one","tool":"doc.compare_documents",
        "arguments":{"other_file_id":"other_group_doc"}});
    assert_eq!(
        b.execute(request(input), 1).await.unwrap_err().to_string(),
        "documents_scope_denied"
    );
    assert_eq!(calls.lock().unwrap().len(), 2);
}

#[test]
fn new_fields_preserve_existing_operation_fingerprints_and_do_not_ignore_content() {
    let r = request(json!({"action":"create","title":"Plan","operation_id":"create1"}));
    let serialized = serde_json::to_value(&r).unwrap();
    for key in [
        "tool",
        "arguments",
        "document_type",
        "file_path",
        "source_operation_id",
    ] {
        assert!(serialized.get(key).is_none());
    }
    for typ in ["sheet", "slide", "form", "smartsheet", "board", "pdf"] {
        assert!(
            request(
                json!({"action":"create","title":"Plan","operation_id":"create1",
            "document_type":typ,"content":"cannot silently discard this"})
            )
            .validate()
            .is_err()
        );
    }
}

#[tokio::test]
async fn pdf_metadata_is_explicitly_versionless_and_delete_checks_fresh_title() {
    let dir = tempfile::tempdir().unwrap();
    let page = json!({"list":[{"id":"pdf_one","is_folder":false,"title":"Example.pdf",
        "url":"https://docs.qq.com/pdf/pdf_one"}],"finish":true});
    let (b, calls) = broker(
        dir.path(),
        vec![
            ("manage.folder_list", page.clone()),
            ("manage.folder_list", page.clone()),
            ("manage.folder_list", page),
            ("manage.delete_file", json!({})),
        ],
    );
    let info = b
        .execute(request(json!({"action":"inspect","file_id":"pdf_one"})), 1)
        .await
        .unwrap();
    assert_eq!(info["data"]["content_version_available"], false);
    assert!(info["data"]["last_modify_time"].is_null());
    assert!(
        b.execute(
            request(
                json!({"action":"delete","file_id":"pdf_one","operation_id":"pdf-delete",
        "expected_title":"old.pdf"})
            ),
            1
        )
        .await
        .is_err()
    );
    assert_eq!(
        b.execute(
            request(
                json!({"action":"delete","file_id":"pdf_one","operation_id":"pdf-delete",
        "expected_title":"Example.pdf"})
            ),
            1
        )
        .await
        .unwrap()["ok"],
        true
    );
    assert_eq!(calls.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn form_global_id_is_obtained_from_the_scoped_form_not_model_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let (b, calls) = broker(
        dir.path(),
        vec![
            ("manage.folder_list", member_page()),
            (
                "form.get_context",
                json!({"meta":{"file_id":"300000000$example"}}),
            ),
            ("form.attachment_summary", json!({"attachments":[]})),
        ],
    );
    assert_eq!(
        b.execute(
            request(json!({"action":"invoke","file_id":"doc_one",
        "tool":"form.attachment_summary","arguments":{}})),
            1
        )
        .await
        .unwrap()["ok"],
        true
    );
    assert_eq!(calls.lock().unwrap()[2].1["file_id"], "300000000$example");
}

#[tokio::test]
async fn outside_files_and_folders_cannot_be_read_or_deleted() {
    let dir = tempfile::tempdir().unwrap();
    for id in ["personal_doc", "nested_folder"] {
        let (b, calls) = broker(
            dir.path(),
            vec![(
                "manage.folder_list",
                json!({"list":[{"id":"nested_folder","is_folder":true}],"finish":true}),
            )],
        );
        assert!(
            b.execute(request(json!({"action":"read","file_id":id})), 1)
                .await
                .is_err()
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
    }
    let (b, calls) = broker(dir.path(), vec![]);
    assert!(b.execute(request(json!({"action":"delete","file_id":"group_folder","operation_id":"delete","expected_modified_at":1})),1).await.is_err());
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn empty_provider_page_does_not_repeat_cursor_or_grant_membership() {
    let dir = tempfile::tempdir().unwrap();
    let (b, calls) = broker(
        dir.path(),
        vec![
            ("manage.folder_list", json!({"list":[],"finish":false})),
            ("manage.folder_list", json!({"list":[],"finish":false})),
        ],
    );
    let page = b.list(0).await.unwrap();
    assert_eq!(page["finished"], true);
    assert!(page["next_start"].is_null());
    assert_eq!(
        b.member("missing").await.unwrap_err().to_string(),
        "documents_scope_denied"
    );
    assert_eq!(calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn membership_and_read_pagination_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let (b, calls) = broker(
        dir.path(),
        vec![
            (
                "manage.folder_list",
                json!({"list":[{"id":"another","is_folder":false}],"finish":false}),
            ),
            ("manage.folder_list", member_page()),
            (
                "smartcanvas.read",
                json!({"content":"next part","next_token":"cursor2"}),
            ),
        ],
    );
    let output=b.execute(request(json!({"action":"read","file_id":"doc_one","page_id":"page1","next_token":"cursor1"})),1).await.unwrap();
    assert_eq!(output["data"]["next_token"], "cursor2");
    let calls = calls.lock().unwrap();
    assert_eq!(calls[1].1["start"], 1);
    assert_eq!(calls[2].1["next_token"], "cursor1");
}

#[tokio::test]
async fn edit_rechecks_exact_block_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let input = json!({"action":"edit","file_id":"doc_one","operation_id":"update","block_id":"block1","query":"计划","expected_content":"旧计划","content":"新计划"});
    let (b, calls) = broker(
        dir.path(),
        vec![
            ("manage.folder_list", member_page()),
            (
                "smartcanvas.find",
                json!({"blocks":[{"id":"block1","content":"用户刚改的计划"}]}),
            ),
        ],
    );
    assert!(b.execute(request(input.clone()), 1).await.is_err());
    assert_eq!(calls.lock().unwrap().len(), 2);
    let (b, calls) = broker(
        dir.path(),
        vec![
            ("manage.folder_list", member_page()),
            (
                "smartcanvas.find",
                json!({"blocks":[{"id":"block1","content":"旧计划"}]}),
            ),
            ("smartcanvas.edit", json!({})),
        ],
    );
    assert_eq!(b.execute(request(input), 1).await.unwrap()["ok"], true);
    assert_eq!(calls.lock().unwrap()[2].1["action"], "UPDATE");
}

#[tokio::test]
async fn uncertain_mutation_is_persistent_and_not_repeated() {
    let dir = tempfile::tempdir().unwrap();
    let input =
        json!({"action":"append","file_id":"doc_one","operation_id":"append1","content":"todo"});
    let (b, _) = broker(
        dir.path(),
        vec![
            ("manage.folder_list", member_page()),
            ("smartcanvas.edit", json!("fail")),
        ],
    );
    let first = b.execute(request(input.clone()), 3).await.unwrap();
    assert_eq!(first["status"], "unknown");
    let (restarted, calls) = broker(dir.path(), vec![]);
    assert_eq!(
        restarted.execute(request(input.clone()), 4).await.unwrap(),
        first
    );
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(
        restarted
            .execute(
                request(json!({"action":"operation","operation_id":"append1"})),
                9
            )
            .await
            .unwrap(),
        first
    );
    let mut different = input;
    different["content"] = json!("changed");
    assert!(restarted.execute(request(different), 3).await.is_err());
}

#[tokio::test]
async fn initialization_failure_can_be_retried_after_credentials_are_fixed() {
    let dir = tempfile::tempdir().unwrap();
    let input =
        json!({"action":"append","file_id":"doc_one","operation_id":"retry1","content":"todo"});
    let (b, _) = broker(
        dir.path(),
        vec![
            ("manage.folder_list", member_page()),
            ("smartcanvas.edit", json!("before_dispatch")),
        ],
    );
    assert_eq!(
        b.execute(request(input.clone()), 1).await.unwrap()["status"],
        "not_dispatched"
    );
    let (b, _) = broker(
        dir.path(),
        vec![
            ("manage.folder_list", member_page()),
            ("smartcanvas.edit", json!({})),
        ],
    );
    assert_eq!(b.execute(request(input), 2).await.unwrap()["ok"], true);
}

#[tokio::test]
async fn partial_create_returns_known_file_without_creating_twice() {
    let dir = tempfile::tempdir().unwrap();
    let input = json!({"action":"create","operation_id":"create1","task_id":"task_one","document_key":"main","title":"计划","content":"# 目标"});
    let (b, calls) = broker(
        dir.path(),
        vec![
            (
                "manage.folder_list",
                json!({"list":[{"id":"task_one","title":"Task","is_folder":true}],"finish":true}),
            ),
            ("manage.folder_list", json!({"list":[],"finish":true})),
            (
                "create_smartcanvas_by_mdx",
                json!({"file_id":"doc_one","url":"https://docs.qq.com/doc/doc_one"}),
            ),
            ("manage.move_file", json!("fail")),
        ],
    );
    let result = b.execute(request(input.clone()), 8).await.unwrap();
    assert_eq!(result["status"], "created_move_unknown");
    assert_eq!(result["file_id"], "doc_one");
    assert_eq!(calls.lock().unwrap()[2].1["mdx"], "# 目标");
    assert_eq!(
        calls.lock().unwrap()[3].1,
        json!({"file_id":"doc_one","target_folder_id":"task_one"})
    );
    assert_eq!(b.execute(request(input), 8).await.unwrap(), result);
    assert_eq!(calls.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn delete_checks_latest_timestamp_and_uses_recycle_bin() {
    let dir = tempfile::tempdir().unwrap();
    let input = json!({"action":"delete","operation_id":"delete1","file_id":"doc_one","expected_modified_at":10});
    let (b, calls) = broker(
        dir.path(),
        vec![
            ("manage.folder_list", member_page()),
            ("manage.query_file_info", json!({"last_modify_time":11})),
        ],
    );
    assert!(b.execute(request(input.clone()), 1).await.is_err());
    assert_eq!(calls.lock().unwrap().len(), 2);
    let (b, calls) = broker(
        dir.path(),
        vec![
            ("manage.folder_list", member_page()),
            ("manage.query_file_info", json!({"last_modify_time":10})),
            ("manage.delete_file", json!({})),
        ],
    );
    assert!(
        b.execute(request(input), 1).await.unwrap()["ok"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(
        calls.lock().unwrap()[2].1,
        json!({"file_id":"doc_one","delete_type":"origin"})
    );
}

#[test]
fn binding_and_input_validation_rejects_scope_escapes() {
    let mut config = DocumentConfig::default();
    config.bindings.insert("group1".into(), "folder".into());
    config.bindings.insert("group2".into(), "folder".into());
    assert!(config.validate().is_err());
    for input in [
        json!({"action":"read","file_id":"../secret"}),
        json!({"action":"create","title":"hello","operation_id":"../x"}),
        json!({"action":"delete","file_id":"doc_one","operation_id":"delete"}),
    ] {
        assert!(request(input).validate().is_err());
    }
    assert!(
        serde_json::from_value::<Request>(
            json!({"action":"read","file_id":"doc_one","folder_id":"another_group"})
        )
        .is_err()
    );
}

#[test]
fn readonly_attribute_is_distinct_from_prose_and_other_attributes() {
    assert!(!readonly_component("TypeScript readonly 可以防止属性赋值"));
    assert!(!readonly_component(
        "<Callout title=\"readonly 用法\">解释</Callout>"
    ));
    assert!(readonly_component(
        "<Table id=\"one\" readonly>内容</Table>"
    ));
    assert!(readonly_component("<Table readonly={true}>内容</Table>"));
}
