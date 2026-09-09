use std::sync::Arc;

use codex_config::LoaderOverrides;
use codex_core::config::ConfigBuilder;
use codex_protocol::ThreadId;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::LocalThreadStoreConfig;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

use super::LocalHistoryNotesStore;

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
        result,
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
            store
                .call(endpoint, json!({"path":"progress.md", "text":chunk}))
                .await
                .unwrap()["status"],
            "ok"
        );
    }
    let before = store
        .call("alpha/notes/v2/read_file", json!({"path":"progress.md"}))
        .await
        .unwrap();
    assert_eq!(
        before["text"].as_str().unwrap().len(),
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
            assert_eq!(rejected["status"], "invalid_argument");
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
