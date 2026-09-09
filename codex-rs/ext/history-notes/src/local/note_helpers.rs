use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use serde_json::json;

use super::FileOrder;
use super::NoteError;
use super::NoteFile;
use super::limits;

pub(super) fn optional_path(
    arguments: &Value,
    field: &str,
    allow_empty: bool,
) -> Result<Option<PathBuf>, Value> {
    let Some(value) = arguments.get(field) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(value) = value.as_str() else {
        return Err(limits::invalid_argument(field, "must be a string or null"));
    };
    if value.is_empty() && allow_empty {
        return Ok(None);
    }
    validated_relative_path(value, field, allow_empty).map(Some)
}

pub(super) fn validated_relative_path(
    value: &str,
    field: &str,
    allow_empty: bool,
) -> Result<PathBuf, Value> {
    if value.is_empty() {
        return if allow_empty {
            Ok(PathBuf::new())
        } else {
            Err(limits::invalid_argument(
                field,
                "must be a non-empty string",
            ))
        };
    }
    if value.len() > limits::MAX_NOTE_PATH_BYTES || value.contains('\0') || value.contains('\\') {
        return Err(limits::invalid_argument(field, "contains an unsafe path"));
    }
    let components = value.split('/').collect::<Vec<_>>();
    if components.len() > limits::MAX_NOTE_PATH_COMPONENTS
        || components
            .iter()
            .any(|component| component.is_empty() || matches!(*component, "." | ".."))
    {
        return Err(limits::invalid_argument(
            field,
            "contains an unsafe path component",
        ));
    }
    let path = Path::new(value);
    if path.components().any(|component| {
        matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::CurDir | Component::ParentDir
        )
    }) {
        return Err(limits::invalid_argument(
            field,
            "must be a safe relative path",
        ));
    }
    Ok(path.to_path_buf())
}

pub(super) fn parse_file_order(arguments: &Value) -> Result<FileOrder, Value> {
    match limits::optional_string(arguments, "file_order_by") {
        None | Some("name") => Ok(FileOrder::Name),
        Some("created_at") => Ok(FileOrder::CreatedAt),
        Some("updated_at") => Ok(FileOrder::UpdatedAt),
        Some(_) => Err(limits::invalid_argument(
            "file_order_by",
            "must be name, created_at, or updated_at",
        )),
    }
}

pub(super) fn line_range(arguments: &Value, line_count: usize) -> Result<(usize, usize), Value> {
    let start = line_number(arguments, "start_line", 1, line_count)?;
    let stop_default = line_count.max(start);
    let stop = line_number(arguments, "stop_line", stop_default, line_count)?;
    if start > stop {
        return Err(limits::invalid_argument(
            "start_line",
            "must not be after stop_line",
        ));
    }
    Ok((start, stop))
}

pub(super) fn line_number(
    arguments: &Value,
    field: &str,
    default: usize,
    line_count: usize,
) -> Result<usize, Value> {
    let Some(value) = arguments.get(field) else {
        return Ok(default);
    };
    if value.is_null() {
        return Ok(default);
    }
    let Some(value) = value.as_i64() else {
        return Err(limits::invalid_argument(
            field,
            "must be an integer or null",
        ));
    };
    if value == 0 {
        return Err(limits::invalid_argument(field, "must not be zero"));
    }
    let line = if value < 0 {
        let distance = value.unsigned_abs() as usize;
        line_count
            .checked_sub(distance.saturating_sub(1))
            .ok_or_else(|| limits::invalid_argument(field, "is outside the file"))?
    } else {
        value as usize
    };
    if line == 0 || line > line_count {
        return Err(limits::invalid_argument(field, "is outside the file"));
    }
    Ok(line)
}

pub(super) fn compare_files(
    left: &NoteFile,
    right: &NoteFile,
    order: FileOrder,
    descending: bool,
) -> Ordering {
    let ordering = match order {
        FileOrder::Name => left.path.cmp(&right.path),
        FileOrder::CreatedAt => left
            .created_at
            .cmp(&right.created_at)
            .then_with(|| left.path.cmp(&right.path)),
        FileOrder::UpdatedAt => left
            .updated_at
            .cmp(&right.updated_at)
            .then_with(|| left.path.cmp(&right.path)),
    };
    if descending {
        ordering.reverse()
    } else {
        ordering
    }
}

pub(super) fn file_value(file: &NoteFile) -> Value {
    json!({
        "path": file.path,
        "bytes": file.bytes,
        "lines": file.lines,
        "created_at": file.created_at,
        "updated_at": file.updated_at,
    })
}

pub(super) fn note_line_count(path: &Path) -> Option<usize> {
    if fs::symlink_metadata(path).ok()?.len() > limits::MAX_NOTE_BYTES as u64 {
        return None;
    }
    let contents = fs::read_to_string(path).ok()?;
    Some(contents.lines().count())
}

pub(super) fn virtual_path(path: &Path) -> Result<String, io::Error> {
    let components = path
        .components()
        .map(|component| match component {
            Component::Normal(value) => Ok(value.to_string_lossy().into_owned()),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsafe note path",
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(components.join("/"))
}

pub(super) fn timestamp(time: Option<SystemTime>) -> u64 {
    time.and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs())
}

pub(super) fn bounded_text(text: &str, maximum_bytes: usize) -> (String, bool) {
    if text.len() <= maximum_bytes {
        return (text.to_string(), false);
    }
    let marker = "\n[truncated]";
    let budget = maximum_bytes.saturating_sub(marker.len());
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut output = text[..end].to_string();
    output.push_str(marker);
    (output, true)
}

pub(super) fn fit_text_field(mut value: Value, field: &str) -> Value {
    if serialized_len(&value) <= limits::MAX_NOTE_RESULT_BYTES {
        return value;
    }
    let Some(original) = value.get(field).and_then(Value::as_str).map(str::to_owned) else {
        return value;
    };
    let mut low = 0;
    let mut high = original.chars().count();
    let mut best = String::new();
    while low <= high {
        let middle = (low + high) / 2;
        let candidate = original.chars().take(middle).collect::<String>();
        value[field] = Value::String(candidate.clone());
        if serialized_len(&value) <= limits::MAX_NOTE_RESULT_BYTES {
            best = candidate;
            low = middle + 1;
        } else if middle == 0 {
            break;
        } else {
            high = middle - 1;
        }
    }
    value[field] = Value::String(best);
    value["truncated"] = Value::Bool(true);
    value
}

pub(super) fn bound_array_field(value: &mut Value, field: &str) {
    let mut removed_any = false;
    while serialized_len(value) > limits::MAX_NOTE_RESULT_BYTES {
        let removed = value
            .get_mut(field)
            .and_then(Value::as_array_mut)
            .and_then(Vec::pop);
        if removed.is_none() {
            break;
        }
        removed_any = true;
    }
    if serialized_len(value) > limits::MAX_NOTE_RESULT_BYTES {
        *value = json!({"status": "ok", field: [], "truncated": true});
    } else if removed_any {
        value["truncated"] = Value::Bool(true);
    }
}

pub(super) fn serialized_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

pub(super) fn note_error(path: &str, error: NoteError) -> Value {
    json!({
        "status": "unavailable",
        "path": path,
        "reason": error.reason(),
    })
}

pub(super) fn note_too_large(path: &str) -> Value {
    json!({
        "status": "invalid_argument",
        "field": "text",
        "path": path,
        "reason": "note exceeds the maximum UTF-8 byte size",
        "max_bytes": limits::MAX_NOTE_BYTES,
    })
}

pub(super) fn scan_error(reason: &str) -> Value {
    json!({
        "status": "unavailable",
        "reason": reason,
        "truncated": true,
    })
}
