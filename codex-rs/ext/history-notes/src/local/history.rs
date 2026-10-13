use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::ThreadStore;
use serde_json::Value;
use serde_json::json;

use super::history_scanner::HistoryEntry;
use super::history_scanner::ScanResult;
use super::history_scanner::scan_rollout;
use super::limits;

#[derive(Clone)]
pub(super) struct LocalHistoryReader {
    store: Arc<dyn ThreadStore>,
    thread_id: ThreadId,
}

impl LocalHistoryReader {
    pub(super) fn new(store: Arc<dyn ThreadStore>, thread_id: ThreadId) -> Self {
        Self { store, thread_id }
    }

    pub(super) fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    async fn scan(&self) -> Result<ScanResult, Value> {
        let unavailable = |reason: &str| json!({"status": "unavailable", "reason": reason});
        let Some(local) = self.store.as_any().downcast_ref::<LocalThreadStore>() else {
            return Err(unavailable("unsupported_thread_store"));
        };
        self.store
            .flush_thread(self.thread_id)
            .await
            .map_err(|_| unavailable("thread_flush_failed"))?;
        let path = local
            .live_rollout_path(self.thread_id)
            .await
            .map_err(|_| unavailable("active_rollout_unavailable"))?;
        let scan = scan_rollout(&path, self.thread_id)
            .await
            .map_err(|reason| unavailable(&reason))?;
        if scan.info.lineage_unavailable {
            return Err(unavailable("inherited_history_unavailable"));
        }
        Ok(scan)
    }

    pub(super) async fn list_windows(&self, arguments: &Value) -> Result<Value, String> {
        let result = async {
            let limit = limits::bounded_usize(arguments, "limit", 20, limits::MAX_HISTORY_WINDOWS)?;
            let offset = limits::cursor(arguments)?;
            let scan = self.scan().await?;
            let mut windows: Vec<_> = scan.windows.values().collect();
            if arguments.get("recent_first").and_then(Value::as_bool).unwrap_or(false) {
                windows.reverse();
            }
            let mut result = scan_metadata(&scan);
            result["windows"] = json!([]);
            let mut emitted = 0;
            for window in windows.iter().skip(offset).take(limit) {
                let value = json!({"window_id": window.window_id, "window_number": window.window_number,
                    "context_window_id": window.context_window_id, "item_count": window.item_count,
                    "readable_item_count": window.readable_item_count, "opaque_item_count": window.opaque_item_count});
                if !push_bounded(&mut result, "windows", value)? { break; }
                emitted += 1;
            }
            set_cursor(&mut result, offset, emitted, windows.len());
            Ok(result)
        }.await;
        Ok(result.unwrap_or_else(|error| error))
    }

    pub(super) async fn list_items(&self, arguments: &Value) -> Result<Value, String> {
        self.items(arguments, None).await
    }

    pub(super) async fn search_contents(&self, arguments: &Value) -> Result<Value, String> {
        let Some(query) = arguments.get("query").and_then(Value::as_str) else {
            return Ok(limits::invalid_argument("query", "must be a string"));
        };
        if query.is_empty() || limits::char_count(query) > limits::MAX_HISTORY_QUERY_CHARS {
            return Ok(limits::invalid_argument(
                "query",
                "must be non-empty and at most 1000 characters",
            ));
        }
        self.items(arguments, Some(query)).await
    }

    async fn items(&self, arguments: &Value, query: Option<&str>) -> Result<Value, String> {
        let result = async {
            let limit = limits::bounded_usize(arguments, "limit", 10, limits::MAX_HISTORY_ITEMS)?;
            let preview = limits::bounded_usize(
                arguments,
                "max_chars_per_item",
                768,
                limits::MAX_HISTORY_ITEM_PREVIEW_CHARS,
            )?;
            let offset = limits::cursor(arguments)?;
            let scan = self.scan().await?;
            let mut entries: Vec<_> = scan
                .entries
                .iter()
                .filter(|entry| {
                    matches_filters(entry, arguments)
                        && query.is_none_or(|query| {
                            entry
                                .projection
                                .content
                                .as_ref()
                                .is_some_and(|text| text.contains(query))
                        })
                })
                .collect();
            if arguments
                .get("recent_first")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                entries.reverse();
            }
            let mut result = scan_metadata(&scan);
            result["items"] = json!([]);
            let mut emitted = 0;
            for entry in entries.iter().skip(offset).take(limit) {
                let mut value = entry_metadata(entry);
                if let Some(text) = &entry.projection.content {
                    let start = query
                        .and_then(|query| text.find(query))
                        .map_or(0, |byte| text[..byte].chars().count().saturating_sub(128));
                    let max_chars = if query.is_some() {
                        limits::MAX_HISTORY_SEARCH_EXCERPT_CHARS
                    } else {
                        preview
                    };
                    let (excerpt, _, truncated) = limits::char_slice(text, start, max_chars);
                    value["truncated_content"] = json!(excerpt);
                    value["offset_chars"] = json!(start);
                    value["truncated"] =
                        json!(start > 0 || truncated || entry.projection.content_truncated);
                }
                if !push_bounded(&mut result, "items", value)? {
                    break;
                }
                emitted += 1;
            }
            set_cursor(&mut result, offset, emitted, entries.len());
            Ok(result)
        }
        .await;
        Ok(result.unwrap_or_else(|error| error))
    }

    pub(super) async fn read_item(&self, arguments: &Value) -> Result<Value, String> {
        let result = async {
            let item_id = arguments.get("item_id").and_then(Value::as_str)
                .ok_or_else(|| limits::invalid_argument("item_id", "must be a returned item ID"))?;
            let window_id = arguments.get("window_id").and_then(Value::as_str)
                .ok_or_else(|| limits::invalid_argument("window_id", "must be a returned window ID"))?;
            let offset = match arguments.get("offset_chars") {
                None => 0,
                Some(value) => value.as_u64().and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| limits::invalid_argument("offset_chars", "must be a non-negative integer"))?,
            };
            let mut limit = limits::bounded_usize(arguments, "limit_chars", 4000, limits::MAX_HISTORY_READ_CHARS)?;
            let scan = self.scan().await?;
            let Some(entry) = scan.entries.iter().find(|entry| entry.item_id == item_id && entry.window_id == window_id) else {
                let mut result = scan_metadata(&scan);
                result["status"] = json!("unavailable");
                result["reason"] = json!("item_not_found_in_scanned_history");
                return Ok(result);
            };
            let mut result = entry_metadata(entry);
            let Some(text) = &entry.projection.content else {
                result["status"] = json!("unavailable");
                return Ok(result);
            };
            loop {
                let (content, consumed, truncated) = limits::char_slice(text, offset, limit);
                result["content"] = json!(content);
                result["offset_chars"] = json!(offset);
                result["next_offset_chars"] = json!(offset.saturating_add(consumed));
                result["truncated"] = json!(truncated || entry.projection.content_truncated);
                result["status"] = json!("ok");
                if result.to_string().len() <= limits::MAX_HISTORY_RESULT_BYTES { break; }
                limit /= 2;
                if limit == 0 {
                    return Ok(json!({"status": "unavailable", "reason": "item_metadata_exceeds_output_limit"}));
                }
            }
            Ok(result)
        }.await;
        Ok(result.unwrap_or_else(|error| error))
    }
}

fn scan_metadata(scan: &ScanResult) -> Value {
    let complete = scan.info.complete
        && !scan
            .entries
            .iter()
            .any(|entry| entry.projection.content_truncated);
    json!({"status": if complete { "ok" } else { "partial" }, "scan_complete": complete,
        "records_scanned": scan.info.records_scanned, "opaque_records": scan.info.opaque_records,
        "corrupt": scan.info.corrupt, "errors": scan.info.errors, "truncated": scan.info.truncated,
        "next_cursor": null})
}

fn entry_metadata(entry: &HistoryEntry) -> Value {
    json!({"item_id": entry.item_id, "window_id": entry.window_id,
        "window_number": entry.window_number, "role": entry.projection.role, "kind": entry.projection.kind,
        "tool_namespace": entry.projection.tool_namespace, "tool_name": entry.projection.tool_name,
        "call_id": entry.projection.call_id, "timestamp": entry.timestamp,
        "unavailable_reason": entry.projection.unavailable_reason,
        "source": {"ordinal": entry.source.ordinal, "physical_record": entry.source.physical_record,
            "replacement_index": entry.source.replacement_index},
        "opaque": entry.projection.opaque, "source_truncated": entry.projection.content_truncated})
}

fn matches_filters(entry: &HistoryEntry, arguments: &Value) -> bool {
    [
        ("window_id", Some(entry.window_id.as_str())),
        ("role", Some(entry.projection.role.as_str())),
        ("tool_namespace", entry.projection.tool_namespace.as_deref()),
        ("tool_name", entry.projection.tool_name.as_deref()),
    ]
    .into_iter()
    .all(|(field, value)| {
        arguments
            .get(field)
            .and_then(Value::as_str)
            .is_none_or(|requested| Some(requested) == value)
    })
}

fn push_bounded(result: &mut Value, field: &str, value: Value) -> Result<bool, Value> {
    limits::array_mut(result, field)?.push(value);
    // Reserve space for the final continuation cursor.
    if result.to_string().len() + 100 > limits::MAX_HISTORY_RESULT_BYTES {
        limits::array_mut(result, field)?.pop();
        return Ok(false);
    }
    Ok(true)
}

fn set_cursor(result: &mut Value, offset: usize, emitted: usize, count: usize) {
    let next = offset.saturating_add(emitted);
    if next < count {
        result["truncated"] = json!(true);
        result["next_cursor"] = if emitted > 0 {
            json!(next.to_string())
        } else {
            Value::Null
        };
        if emitted == 0 {
            result["reason"] = json!("item_exceeds_output_limit; reduce max_chars_per_item");
        }
    }
}
