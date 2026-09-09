use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

use super::*;

fn store(temp_dir: &TempDir, value: u128) -> LocalNoteStore {
    LocalNoteStore::new(temp_dir.path().to_path_buf(), ThreadId::from_u128(value))
}

#[test]
fn writes_appends_reads_after_restart_and_lists_files() {
    let temp_dir = TempDir::new().expect("temporary notes root");
    let first = store(&temp_dir, 1);
    assert_eq!(
        first.write_file(&json!({"path": "INDEX.md", "text": "Goal\nNext"})),
        Ok(json!({"status": "ok", "path": "INDEX.md", "bytes": 9}))
    );
    assert_eq!(
        first.append_file(&json!({"path": "INDEX.md", "text": "\nDone"})),
        Ok(json!({
            "status": "ok",
            "path": "INDEX.md",
            "bytes": 14,
            "appended_bytes": 5
        }))
    );
    assert_eq!(
        first.write_file(&json!({
            "path": "progress/checkpoint.md",
            "text": "durable checkpoint"
        })),
        Ok(json!({
            "status": "ok",
            "path": "progress/checkpoint.md",
            "bytes": 18
        }))
    );

    let restarted = store(&temp_dir, 1);
    let read = restarted
        .read_file(&json!({"path": "INDEX.md"}))
        .expect("read result");
    assert_eq!(read["status"], "ok");
    assert_eq!(read["text"], "Goal\nNext\nDone");
    assert_eq!(read["line_count"], 3);

    let listed = restarted
        .list_files(&json!({"file_order_by": "name"}))
        .expect("list result");
    assert_eq!(listed["status"], "ok");
    assert_eq!(
        listed["files"]
            .as_array()
            .expect("file list")
            .iter()
            .map(|file| file["path"].as_str().expect("path"))
            .collect::<Vec<_>>(),
        vec!["INDEX.md", "progress/checkpoint.md"]
    );
}

#[test]
fn reads_line_ranges_and_searches_literal_matches() {
    let temp_dir = TempDir::new().expect("temporary notes root");
    let notes = store(&temp_dir, 2);
    notes
        .write_file(&json!({
            "path": "notes.md",
            "text": "first\nsecond target\nthird target"
        }))
        .expect("write note");

    let selected = notes
        .read_file(&json!({
            "path": "notes.md",
            "start_line": 2,
            "stop_line": -1
        }))
        .expect("read range");
    assert_eq!(selected["text"], "second target\nthird target");
    assert_eq!(selected["start_line"], 2);
    assert_eq!(selected["stop_line"], 3);

    let searched = notes
        .search_contents(&json!({"query": "target"}))
        .expect("search result");
    assert_eq!(searched["status"], "ok");
    assert_eq!(searched["files_scanned"], 1);
    assert_eq!(searched["files_with_matches"], 1);
    assert_eq!(searched["matches"][0]["line"], 2);
    assert_eq!(searched["matches"][1]["line"], 3);
}

#[test]
fn list_prefix_matches_partial_file_names() {
    let home = TempDir::new().unwrap();
    let notes = store(&home, 20);
    notes
        .write_file(&json!({"path": "INDEX.md", "text": "checkpoint"}))
        .unwrap();
    let result = notes.list_files(&json!({"prefix": "IND"})).unwrap();
    assert_eq!(result["files"][0]["path"], "INDEX.md");
}

#[test]
fn search_prefix_matches_partial_and_full_file_names() {
    let home = TempDir::new().unwrap();
    let notes = store(&home, 21);
    notes
        .write_file(&json!({"path": "progress/checkpoint.md", "text": "target"}))
        .unwrap();
    notes
        .write_file(&json!({"path": "unrelated.md", "text": "target"}))
        .unwrap();
    for prefix in ["pro", "progress/checkpoint.md"] {
        let result = notes
            .search_contents(&json!({"query": "target", "path_prefix": prefix}))
            .unwrap();
        assert_eq!(result["files_with_matches"], 1);
        assert_eq!(result["matches"][0]["path"], "progress/checkpoint.md");
    }
}

#[test]
fn keeps_note_results_and_hints_bounded() {
    let temp_dir = TempDir::new().expect("temporary notes root");
    let notes = store(&temp_dir, 3);
    let long_line = "target ".repeat(2_000);
    notes
        .write_file(&json!({"path": "INDEX.md", "text": long_line}))
        .expect("write index");
    for index in 0..limits::MAX_THREAD_HINT_FILES + 3 {
        notes
            .write_file(&json!({
                "path": format!("progress/{index}.md"),
                "text": "checkpoint"
            }))
            .expect("write progress note");
    }

    let searched = notes
        .search_contents(&json!({"query": "target", "max_files": 20}))
        .expect("bounded search result");
    assert!(
        serde_json::to_vec(&searched)
            .expect("serialize search")
            .len()
            <= limits::MAX_NOTE_RESULT_BYTES
    );
    assert_eq!(searched["truncated"], true);

    let hint = notes.thread_hint();
    let text = hint["text"].as_str().expect("hint text");
    assert!(text.len() <= limits::MAX_THREAD_HINT_BYTES);
    assert!(text.contains("INDEX.md"));
    assert!(text.contains("truncated"));
}

#[test]
fn isolates_threads_and_rejects_unsafe_paths() {
    let temp_dir = TempDir::new().expect("temporary notes root");
    let first = store(&temp_dir, 4);
    let second = store(&temp_dir, 5);
    first
        .write_file(&json!({"path": "private.md", "text": "thread one"}))
        .expect("write first thread note");

    let missing = second
        .read_file(&json!({"path": "private.md"}))
        .expect("isolated read result");
    assert_eq!(missing["status"], "unavailable");
    assert_eq!(missing["reason"], "not_found");

    for path in [
        "../escape.md",
        "/tmp/escape.md",
        "./dot.md",
        "nested/./dot.md",
        "a//b.md",
        "a\\b.md",
    ] {
        let result = first
            .write_file(&json!({"path": path, "text": "nope"}))
            .expect("unsafe path result");
        assert_eq!(result["status"], "invalid_argument", "path={path}");
    }
}

#[test]
fn rejects_oversized_replacement_without_destroying_existing_note() {
    let temp_dir = TempDir::new().expect("temporary notes root");
    let notes = store(&temp_dir, 6);
    notes
        .write_file(&json!({"path": "stable.md", "text": "keep me"}))
        .expect("initial write");

    let oversized = "x".repeat(limits::MAX_NOTE_BYTES + 1);
    let result = notes
        .write_file(&json!({"path": "stable.md", "text": oversized}))
        .expect("oversized write result");
    assert_eq!(result["status"], "invalid_argument");
    assert_eq!(result["reason"], "note exceeds the maximum UTF-8 byte size");

    let read = notes
        .read_file(&json!({"path": "stable.md"}))
        .expect("read preserved note");
    assert_eq!(read["text"], "keep me");
}

#[cfg(unix)]
#[test]
fn refuses_symlinked_note_paths() {
    use std::os::unix::fs::symlink;

    let temp_dir = TempDir::new().expect("temporary notes root");
    let notes = store(&temp_dir, 7);
    notes
        .write_file(&json!({"path": "visible.md", "text": "inside"}))
        .expect("write visible note");
    let outside = temp_dir.path().join("outside.md");
    std::fs::write(&outside, "outside").expect("write outside note");
    symlink(&outside, notes.notes_root.join("link.md")).expect("create symlink");

    let read = notes
        .read_file(&json!({"path": "link.md"}))
        .expect("symlink read result");
    assert_eq!(read["status"], "unavailable");
    assert_eq!(read["reason"], "unsafe_path");
    let write = notes
        .write_file(&json!({"path": "link.md", "text": "overwrite"}))
        .expect("symlink write result");
    assert_eq!(write["status"], "unavailable");
    assert_eq!(
        std::fs::read_to_string(outside).expect("outside note"),
        "outside"
    );
}
