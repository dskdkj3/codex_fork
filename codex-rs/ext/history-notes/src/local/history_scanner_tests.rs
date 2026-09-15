use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionMeta;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

use super::scan_rollout;

fn record(ordinal: u64, kind: &str, payload: Value) -> String {
    json!({"timestamp": "2026-09-09T00:00:00Z", "ordinal": ordinal, "type": kind, "payload": payload}).to_string() + "\n"
}

fn header(thread_id: ThreadId) -> String {
    record(
        0,
        "session_meta",
        serde_json::to_value(SessionMeta {
            id: thread_id,
            ..SessionMeta::default()
        })
        .unwrap(),
    )
}

#[tokio::test]
async fn fresh_child_parent_identity_does_not_imply_inherited_history() {
    let home = TempDir::new().unwrap();
    let path = home.path().join("rollout.jsonl");
    let id = ThreadId::new();
    let mut meta = serde_json::to_value(SessionMeta {
        id,
        parent_thread_id: Some(ThreadId::new()),
        ..SessionMeta::default()
    })
    .unwrap();
    let own_output = record(
        1,
        "response_item",
        json!({"type": "function_call_output", "call_id": "child-call", "output": "child-only-error"}),
    );
    std::fs::write(&path, record(0, "session_meta", meta.clone()) + &own_output).unwrap();
    let result = scan_rollout(&path, id).await.unwrap();
    assert!(result.info.complete);
    assert!(!result.info.lineage_unavailable);
    assert_eq!(
        result.entries[0].projection.content.as_deref(),
        Some("child-only-error")
    );
    assert!(scan_rollout(&path, ThreadId::new()).await.is_err());

    // Actual inheritance evidence still rejects even with a fresh parent ID.
    for (field, value) in [
        (
            "history_base",
            json!({"thread_id": ThreadId::new(), "end_ordinal_exclusive":7,"end_byte_offset":0}),
        ),
        ("forked_from_id", json!(ThreadId::new())),
        ("forked_from_ordinal_exclusive", json!(7)),
        ("subagent_history_start_ordinal", json!(7)),
    ] {
        meta[field] = value;
        std::fs::write(&path, record(0, "session_meta", meta.clone()) + &own_output).unwrap();
        assert!(
            scan_rollout(&path, id)
                .await
                .unwrap()
                .info
                .lineage_unavailable
        );
        meta[field] = json!(null);
    }
}

#[tokio::test]
async fn originals_keep_ordinals_and_windows_across_compaction() {
    let home = TempDir::new().unwrap();
    let path = home.path().join("rollout.jsonl");
    let id = ThreadId::new();
    let text = header(id)
        + &record(
            1,
            "response_item",
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "原始约束"}]}),
        )
        + &record(
            2,
            "response_item",
            json!({"type": "function_call", "name": "exec_command", "namespace": "functions", "call_id": "call-1", "arguments": "{}"}),
        )
        + &record(
            3,
            "response_item",
            json!({"type": "function_call_output", "call_id": "call-1", "output": "exact stderr: diagnostic-42"}),
        )
        + &record(
            4,
            "compacted",
            json!({"message": "", "window_number": 1, "window_id": "window-1", "first_window_id": "window-0", "previous_window_id": "window-0"}),
        )
        + &record(
            5,
            "response_item",
            json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "continued"}]}),
        );
    std::fs::write(&path, text).unwrap();
    let result = scan_rollout(&path, id).await.unwrap();
    assert!(result.info.complete);
    let original = result
        .entries
        .iter()
        .find(|entry| entry.item_id == "ordinal:3")
        .unwrap();
    assert_eq!(
        original.projection.content.as_deref(),
        Some("exact stderr: diagnostic-42")
    );
    assert_eq!(
        original.projection.tool_name.as_deref(),
        Some("exec_command")
    );
    assert_eq!(original.window_id, format!("{id}:0"));
    assert_eq!(result.entries.last().unwrap().window_id, format!("{id}:1"));
    assert!(scan_rollout(&path, ThreadId::new()).await.is_err());
}

#[tokio::test]
async fn opaque_and_corrupt_records_are_distinct_from_empty_search() {
    let home = TempDir::new().unwrap();
    let path = home.path().join("rollout.jsonl");
    let id = ThreadId::new();
    let text = header(id)
        + &record(
            1,
            "response_item",
            json!({"type": "function_call_output", "call_id": "opaque", "output": [{"type": "encrypted_content", "encrypted_content": "unreadable"}]}),
        )
        + "malformed completed record\n";
    std::fs::write(&path, text).unwrap();
    let result = scan_rollout(&path, id).await.unwrap();
    assert!(!result.info.complete);
    assert!(result.info.corrupt);
    assert_eq!(result.info.opaque_records, 1);
    assert_eq!(result.entries[0].projection.content, None);
}

#[tokio::test]
async fn retains_long_original_text_for_ranged_reads() {
    let home = TempDir::new().unwrap();
    let path = home.path().join("rollout.jsonl");
    let id = ThreadId::new();
    let original = "x".repeat(25_000) + " recoverable-tail";
    std::fs::write(
        &path,
        header(id)
            + &record(
                1,
                "response_item",
                json!({
                    "type": "function_call_output", "call_id": "long-output", "output": original
                }),
            ),
    )
    .unwrap();
    let result = scan_rollout(&path, id).await.unwrap();
    assert_eq!(
        result.entries[0].projection.content.as_deref(),
        Some(original.as_str())
    );
}

#[tokio::test]
async fn unfinished_tail_is_partial_but_not_a_corrupt_completed_record() {
    let home = TempDir::new().unwrap();
    let path = home.path().join("rollout.jsonl");
    let id = ThreadId::new();
    std::fs::write(&path, header(id) + "{\"type\":").unwrap();
    let result = scan_rollout(&path, id).await.unwrap();
    assert!(!result.info.complete);
    assert!(!result.info.corrupt);
}

#[tokio::test]
async fn oversized_record_stops_with_explicit_incomplete_scan() {
    let home = TempDir::new().unwrap();
    let path = home.path().join("rollout.jsonl");
    let id = ThreadId::new();
    let large = "x".repeat(super::limits::MAX_HISTORY_RECORD_BYTES as usize + 1);
    std::fs::write(
        &path,
        header(id)
            + &record(
                1,
                "response_item",
                json!({
                    "type": "function_call_output", "call_id": "oversized", "output": large
                }),
            ),
    )
    .unwrap();
    let result = scan_rollout(&path, id).await.unwrap();
    assert!(!result.info.complete);
    assert!(result.info.truncated);
    assert!(!result.info.corrupt);
    assert!(result.entries.is_empty());
    assert!(!result.info.errors.is_empty());
}

#[tokio::test]
async fn encrypted_projection_only_replays_native_agent_messages_and_keeps_replacement_identity() {
    let home = TempDir::new().unwrap();
    let path = home.path().join("rollout.jsonl");
    let id = ThreadId::new();
    let mixed = json!({"type":"agent_message", "author":"/root", "recipient":"/root/child", "content":[
        {"type":"input_text", "text":"parent metadata"},
        {"type":"encrypted_content", "encrypted_content":"native-first"},
        {"type":"encrypted_content", "encrypted_content":"native-second"}
    ]});
    let hidden = vec![
        json!({"type":"reasoning", "summary":[], "encrypted_content":"hidden-reasoning"}),
        json!({"type":"function_call", "name":"opaque", "call_id":"opaque-call", "arguments":"{}", "encrypted_function_args":["hidden-args"]}),
        json!({"type":"function_call_output", "call_id":"opaque-call", "output":[{"type":"encrypted_content", "encrypted_content":"hidden-output"}]}),
        json!({"type":"compaction", "encrypted_content":"hidden-compaction"}),
    ];
    let mut text = header(id) + &record(1, "response_item", mixed.clone());
    for (index, item) in hidden.into_iter().enumerate() {
        text += &record(index as u64 + 2, "response_item", item);
    }
    text += &record(
        6,
        "compacted",
        json!({"message":"", "replacement_history":[mixed], "window_number":1}),
    );
    std::fs::write(&path, text).unwrap();
    let scan = scan_rollout(&path, id).await.unwrap();
    assert!(scan.info.complete, "{:?}", scan.info.errors);
    let replayable: Vec<_> = scan
        .entries
        .iter()
        .filter(|entry| !entry.projection.agent_message_encrypted_parts.is_empty())
        .collect();
    assert_eq!(replayable.len(), 2);
    for entry in &replayable {
        assert_eq!(
            entry.projection.agent_message_encrypted_parts,
            vec!["native-first", "native-second"]
        );
        assert_eq!(entry.projection.content.as_deref(), Some("parent metadata"));
    }
    assert_eq!(replayable[0].item_id, "ordinal:1");
    assert_eq!(replayable[1].item_id, "ordinal:6:replacement:0");
    assert_eq!(replayable[1].source.replacement_index, Some(0));
    assert_ne!(replayable[0].window_id, replayable[1].window_id);
}
