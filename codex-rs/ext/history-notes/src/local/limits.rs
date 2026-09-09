use serde_json::Value;

pub(super) fn array_mut<'a>(
    value: &'a mut Value,
    field: &str,
) -> Result<&'a mut Vec<Value>, Value> {
    value.get_mut(field).and_then(Value::as_array_mut).ok_or_else(|| {
        serde_json::json!({"status": "unavailable", "reason": "invalid_result_array", "field": field})
    })
}

pub(super) const MAX_HISTORY_WINDOWS: usize = 100;
pub(super) const MAX_HISTORY_ITEMS: usize = 20;
pub(super) const MAX_HISTORY_ITEM_PREVIEW_CHARS: usize = 4_000;
pub(super) const MAX_HISTORY_READ_CHARS: usize = 20_000;
pub(super) const MAX_HISTORY_QUERY_CHARS: usize = 1_000;
pub(super) const MAX_HISTORY_RESULT_BYTES: usize = 8_000;
pub(super) const MAX_HISTORY_SCAN_RECORDS: usize = 10_000;
pub(super) const MAX_HISTORY_SCAN_WINDOWS: usize = 512;
pub(super) const MAX_HISTORY_SCAN_ERRORS: usize = 8;
pub(super) const MAX_HISTORY_ERROR_CHARS: usize = 256;
pub(super) const MAX_HISTORY_REPLACEMENT_ITEMS: usize = 256;
pub(super) const MAX_HISTORY_TOOL_CALLS: usize = 10_000;
pub(super) const MAX_HISTORY_SCAN_BYTES: u64 = 64 * 1024 * 1024;
pub(super) const MAX_HISTORY_RECORD_BYTES: u64 = 4 * 1024 * 1024;
pub(super) const MAX_HISTORY_SEARCH_EXCERPT_CHARS: usize = 768;
pub(super) const MAX_NOTE_BYTES: usize = 1_000_000;
pub(super) const MAX_NOTE_FILES: usize = 100;
pub(super) const MAX_NOTE_SEARCH_FILES: usize = 20;
pub(super) const MAX_NOTE_MATCHES_PER_FILE: usize = 10;
pub(super) const MAX_NOTE_QUERY_CHARS: usize = 1_000;
pub(super) const MAX_NOTE_RESULT_BYTES: usize = 8_000;
pub(super) const MAX_NOTE_PATH_BYTES: usize = 1_024;
pub(super) const MAX_NOTE_PATH_COMPONENTS: usize = 64;
pub(super) const MAX_NOTE_SCAN_FILES: usize = 256;
pub(super) const MAX_NOTE_MATCH_TEXT_CHARS: usize = 768;
pub(super) const MAX_THREAD_HINT_BYTES: usize = 4_000;
pub(super) const MAX_THREAD_HINT_FILES: usize = 5;

pub(super) fn bounded_usize(
    arguments: &Value,
    field: &str,
    default: usize,
    maximum: usize,
) -> Result<usize, Value> {
    let Some(value) = arguments.get(field) else {
        return Ok(default.min(maximum));
    };
    let Some(value) = value.as_u64().and_then(|value| usize::try_from(value).ok()) else {
        return Err(invalid_argument(field, "must be a non-negative integer"));
    };
    if value == 0 {
        return Err(invalid_argument(field, "must be greater than zero"));
    }
    Ok(value.min(maximum))
}

pub(super) fn optional_string<'a>(arguments: &'a Value, field: &str) -> Option<&'a str> {
    arguments.get(field).and_then(Value::as_str)
}

pub(super) fn cursor(arguments: &Value) -> Result<usize, Value> {
    let Some(cursor) = optional_string(arguments, "cursor") else {
        return Ok(0);
    };
    cursor
        .parse::<usize>()
        .map_err(|_| invalid_argument("cursor", "must be a non-negative integer"))
}

pub(super) fn invalid_argument(field: &str, reason: &str) -> Value {
    serde_json::json!({
        "status": "invalid_argument",
        "field": field,
        "reason": reason,
    })
}

pub(super) fn char_count(text: &str) -> usize {
    text.chars().count()
}

pub(super) fn char_slice(text: &str, offset: usize, limit: usize) -> (&str, usize, bool) {
    let start = byte_index_at_char(text, offset).unwrap_or(text.len());
    let end = byte_index_at_char(&text[start..], limit)
        .map(|relative| start + relative)
        .unwrap_or(text.len());
    let consumed = text[start..end].chars().count();
    (&text[start..end], consumed, end < text.len())
}

fn byte_index_at_char(text: &str, char_index: usize) -> Option<usize> {
    if char_index == text.chars().count() {
        return Some(text.len());
    }
    text.char_indices().nth(char_index).map(|(index, _)| index)
}
