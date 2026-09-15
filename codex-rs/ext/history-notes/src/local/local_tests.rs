use std::sync::Arc;

use codex_config::LoaderOverrides;
use codex_core::config::ConfigBuilder;
use codex_protocol::ThreadId;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::LocalThreadStoreConfig;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

use super::LocalHistoryNotesResult;
use super::LocalHistoryNotesStore;

fn json_result(result: LocalHistoryNotesResult) -> serde_json::Value {
    match result {
        LocalHistoryNotesResult::Json(value) => value,
        LocalHistoryNotesResult::AgentMessageReplay(_) => panic!("expected JSON"),
    }
}

#[tokio::test]
async fn rejects_foreign_agent_before_reading_or_writing() {
    let home = TempDir::new().unwrap();
    let config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(home.path().to_path_buf())
        .build()
        .await
        .unwrap();
    let thread_id = ThreadId::new();
    let store = LocalHistoryNotesStore::new(
        Arc::new(LocalThreadStore::new(
            LocalThreadStoreConfig::from_config(&config),
            /*state_db*/ None,
        )),
        thread_id,
        "/root".to_string(),
        home.path().join("context-management-local"),
    );
    let result = store
        .call(
            "alpha/notes/v2/write_file",
            json!({
                "agent_name": "/root/other", "path": "secret.md", "text": "foreign"
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        json_result(result),
        json!({
            "status": "unavailable", "reason": "cross_thread_history_is_not_available",
            "scope": {"thread_id": thread_id.to_string(), "agent_name": "/root"}
        })
    );
    assert!(!home.path().join("context-management-local").exists());
}

#[tokio::test]
async fn limits_each_plaintext_mutation_but_allows_larger_aggregate_notes() {
    let home = TempDir::new().unwrap();
    let config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(home.path().to_path_buf())
        .build()
        .await
        .unwrap();
    let store = LocalHistoryNotesStore::new(
        Arc::new(LocalThreadStore::new(
            LocalThreadStoreConfig::from_config(&config),
            /*state_db*/ None,
        )),
        ThreadId::new(),
        "/root".to_string(),
        home.path().join("context-management-local"),
    );
    let chunk = "x".repeat(super::MAX_NOTE_CALL_TEXT_BYTES);
    for endpoint in ["alpha/notes/v2/write_file", "alpha/notes/v2/append_to_file"] {
        assert_eq!(
            json_result(
                store
                    .call(endpoint, json!({"path":"progress.md", "text":chunk}))
                    .await
                    .unwrap()
            )["status"],
            "ok"
        );
    }
    let before = store
        .call("alpha/notes/v2/read_file", json!({"path":"progress.md"}))
        .await
        .unwrap();
    assert_eq!(
        json_result(before.clone())["text"].as_str().unwrap().len(),
        2 * super::MAX_NOTE_CALL_TEXT_BYTES
    );
    for endpoint in ["alpha/notes/v2/write_file", "alpha/notes/v2/append_to_file"] {
        for text in [
            "x".repeat(super::MAX_NOTE_CALL_TEXT_BYTES + 1),
            "\u{1}".repeat(700),
        ] {
            let rejected = store
                .call(endpoint, json!({"path":"progress.md", "text":text}))
                .await
                .unwrap();
            assert_eq!(json_result(rejected)["status"], "invalid_argument");
        }
    }
    assert_eq!(
        store
            .call("alpha/notes/v2/read_file", json!({"path":"progress.md"}))
            .await
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn fresh_child_replays_only_exact_native_agent_messages_with_atomic_bounds() {
    use codex_protocol::models::FunctionCallOutputContentItem;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::ContextManagementBackend;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::ThreadHistoryMode;
    use codex_rollout::RolloutItem;
    use codex_thread_store::AppendThreadItemsParams;
    use codex_thread_store::CreateThreadParams;
    use codex_thread_store::PersistContext;
    use codex_thread_store::ThreadPersistenceMetadata;
    use codex_thread_store::ThreadStore;

    let home = TempDir::new().unwrap();
    let config = ConfigBuilder::default()
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .codex_home(home.path().to_path_buf())
        .build()
        .await
        .unwrap();
    let native = Arc::new(LocalThreadStore::new(
        LocalThreadStoreConfig::from_config(&config),
        /*state_db*/ None,
    ));
    let id = ThreadId::new();
    let parent = ThreadId::new();
    native
        .create_thread(CreateThreadParams {
            session_id: parent.into(),
            thread_id: id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: Some(parent),
            source: SessionSource::Exec,
            thread_source: None,
            originator: "synthetic".into(),
            base_instructions: Default::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            context_management_backend: ContextManagementBackend::Local,
            history_mode: ThreadHistoryMode::Paginated,
            history_base: None,
            subagent_history_start_ordinal: None,
            initial_window_id: "synthetic-window".into(),
            metadata: ThreadPersistenceMetadata {
                cwd: Some(home.path().to_path_buf()),
                model_provider: "synthetic".into(),
                memory_mode: codex_protocol::protocol::ThreadMemoryMode::Enabled,
            },
        })
        .await
        .unwrap();
    native
        .persist_thread(id, PersistContext::Standard)
        .await
        .unwrap();
    let store = LocalHistoryNotesStore::new(
        native.clone(),
        id,
        "/root/child".into(),
        home.path().join("local"),
    );
    let fixed = serde_json::to_vec(&vec![FunctionCallOutputContentItem::EncryptedContent {
        encrypted_content: String::new(),
    }])
    .unwrap()
    .len();
    let bound = super::limits::MAX_HISTORY_RESULT_BYTES;
    // Test exact boundary, one byte over, ordered parts, and serialized escaping.
    let cases = vec![
        (vec!["x".repeat(bound - fixed)], true),
        (vec!["x".repeat(bound - fixed + 1)], false),
        (
            vec!["ciphertext-first".into(), "ciphertext-second".into()],
            true,
        ),
        (vec!["\"".repeat(bound / 2)], false),
    ];
    for (encrypted, accepted) in cases {
        let mut content = vec![json!({"type":"input_text", "text":"synthetic parent metadata"})];
        content.extend(
            encrypted
                .iter()
                .map(|part| json!({"type":"encrypted_content", "encrypted_content":part})),
        );
        let message: ResponseItem = serde_json::from_value(json!({
            "type":"agent_message", "author":"/root", "recipient":"/root/child", "content":content,
        }))
        .unwrap();
        native
            .append_items(AppendThreadItemsParams {
                thread_id: id,
                items: vec![RolloutItem::ResponseItem(message.into())],
            })
            .await
            .unwrap();
        let listed = json_result(
            store
                .call(
                    "alpha/history/v2/list_items",
                    json!({
                        "role":"assistant", "recent_first":true, "limit":1,
                    }),
                )
                .await
                .unwrap(),
        );
        assert_eq!(listed["status"], "ok");
        let item = &listed["items"][0];
        assert_eq!(item["encrypted_agent_message_replay"], true);
        assert_eq!(item["truncated_content"], "synthetic parent metadata");
        assert!(!listed.to_string().contains(&encrypted[0]));
        let args =
            json!({"item_id":item["item_id"], "window_id":item["window_id"], "limit_chars":1});
        let result = store
            .call("alpha/history/v2/read_item", args.clone())
            .await
            .unwrap();
        if accepted {
            assert_eq!(
                result,
                LocalHistoryNotesResult::AgentMessageReplay(
                    encrypted
                        .iter()
                        .map(|part| {
                            FunctionCallOutputContentItem::EncryptedContent {
                                encrypted_content: part.clone(),
                            }
                        })
                        .collect()
                )
            );
        } else {
            assert_eq!(
                json_result(result),
                json!({"status":"unavailable", "reason":"encrypted_item_exceeds_replay_limit"})
            );
        }
        for (field, value, expected) in [
            ("offset_chars", json!(1), "invalid_argument"),
            ("item_id", json!("foreign-item"), "unavailable"),
            ("window_id", json!("foreign-window"), "unavailable"),
            ("agent_name", json!("/root/sibling"), "unavailable"),
        ] {
            let mut invalid = args.clone();
            invalid[field] = value;
            assert_eq!(
                json_result(
                    store
                        .call("alpha/history/v2/read_item", invalid)
                        .await
                        .unwrap()
                )["status"],
                expected
            );
        }
    }
    let hidden = json_result(
        store
            .call(
                "alpha/history/v2/search_contents",
                json!({"query":"ciphertext-first"}),
            )
            .await
            .unwrap(),
    );
    assert_eq!(hidden["items"], json!([]));
}
