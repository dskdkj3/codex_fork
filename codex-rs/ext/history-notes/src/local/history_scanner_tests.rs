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
