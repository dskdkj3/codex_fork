use std::io;
use std::path::PathBuf;

use codex_protocol::ThreadId;
use serde_json::Value;
use serde_json::json;

use super::limits;

#[path = "note_helpers.rs"]
mod helpers;
#[path = "note_storage.rs"]
mod storage;

use helpers::*;

const NOTES_DIRECTORY_NAME: &str = "notes";
#[derive(Clone)]
pub(super) struct LocalNoteStore {
    notes_root: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileOrder {
    Name,
    CreatedAt,
    UpdatedAt,
}

#[derive(Debug)]
struct NoteFile {
    path: String,
    absolute_path: PathBuf,
    bytes: u64,
    lines: Option<usize>,
    created_at: u64,
    updated_at: u64,
}

impl LocalNoteStore {
    pub(super) fn new(storage_root: PathBuf, thread_id: ThreadId) -> Self {
        Self {
            notes_root: storage_root
                .join(thread_id.to_string())
                .join(NOTES_DIRECTORY_NAME),
        }
    }

    pub(super) fn list_files(&self, arguments: &Value) -> Result<Value, String> {
        let prefix = match optional_path(arguments, "prefix", true) {
            Ok(prefix) => prefix,
            Err(error) => return Ok(error),
        };
        let maximum = match limits::bounded_usize(
            arguments,
            "max_results",
            limits::MAX_NOTE_FILES,
            limits::MAX_NOTE_FILES,
        ) {
            Ok(maximum) => maximum,
            Err(error) => return Ok(error),
        };
        let order = match parse_file_order(arguments) {
            Ok(order) => order,
            Err(error) => return Ok(error),
        };
        let descending = match limits::optional_string(arguments, "file_order") {
            None | Some("ascending") => false,
            Some("descending") => true,
            Some(_) => {
                return Ok(limits::invalid_argument(
                    "file_order",
                    "must be ascending or descending",
                ));
            }
        };

        let search_root = self.notes_root.clone();
        let directory_is_safe = match self.existing_directory_is_safe(&search_root) {
            Ok(directory_is_safe) => directory_is_safe,
            Err(_) => return Ok(scan_error("unsafe_path")),
        };
        if !directory_is_safe {
            return Ok(json!({
                "status": "ok",
                "prefix": prefix.map(|path| path.display().to_string()),
                "files": [],
                "truncated": false,
            }));
        }

        let mut files = Vec::new();
        let mut scan_truncated = false;
        if self
            .collect_files(&search_root, &mut files, &mut scan_truncated)
            .is_err()
        {
            return Ok(scan_error("io_error"));
        }
        if let Some(prefix) = &prefix {
            let prefix = virtual_path(prefix).map_err(|error| error.to_string())?;
            files.retain(|file| file.path.starts_with(&prefix));
        }
        files.sort_by(|left, right| compare_files(left, right, order, descending));

        let file_count = files.len();
        let mut result = json!({
            "status": "ok",
            "prefix": prefix.map(|path| path.display().to_string()),
            "files": [],
            "truncated": scan_truncated,
        });
        for file in files.into_iter().take(maximum) {
            limits::array_mut(&mut result, "files")
                .map_err(|error| error.to_string())?
                .push(file_value(&file));
            if serialized_len(&result) > limits::MAX_NOTE_RESULT_BYTES {
                limits::array_mut(&mut result, "files")
                    .map_err(|error| error.to_string())?
                    .pop();
                result["truncated"] = Value::Bool(true);
                break;
            }
        }
        if file_count > maximum {
            result["truncated"] = Value::Bool(true);
        }
        Ok(result)
    }

    pub(super) fn read_file(&self, arguments: &Value) -> Result<Value, String> {
        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            return Ok(limits::invalid_argument(
                "path",
                "must be a non-empty string",
            ));
        };
        let relative_path = match validated_relative_path(path, "path", false) {
            Ok(relative_path) => relative_path,
            Err(error) => return Ok(error),
        };
        let absolute_path = self.notes_root.join(&relative_path);
        let contents = match self.read_note(&absolute_path) {
            Ok(contents) => contents,
            Err(error) => return Ok(note_error(path, error)),
        };
        let lines = contents.lines().collect::<Vec<_>>();
        let (start_line, stop_line, selected) = if lines.is_empty() {
            (0, 0, String::new())
        } else {
            let (start_line, stop_line) = match line_range(arguments, lines.len()) {
                Ok(range) => range,
                Err(error) => return Ok(error),
            };
            (
                start_line,
                stop_line,
                lines[start_line - 1..stop_line].join("\n"),
            )
        };
        let mut result = json!({
            "status": "ok",
            "path": path,
            "text": selected,
            "start_line": start_line,
            "stop_line": stop_line,
            "line_count": lines.len(),
            "bytes": contents.len(),
            "truncated": false,
        });
        result = fit_text_field(result, "text");
        Ok(result)
    }

    pub(super) fn search_contents(&self, arguments: &Value) -> Result<Value, String> {
        let Some(query) = arguments.get("query").and_then(Value::as_str) else {
            return Ok(limits::invalid_argument(
                "query",
                "must be a non-empty string",
            ));
        };
        if query.is_empty() {
            return Ok(limits::invalid_argument("query", "must not be empty"));
        }
        if query.chars().count() > limits::MAX_NOTE_QUERY_CHARS {
            return Ok(limits::invalid_argument(
                "query",
                "exceeds the maximum query length",
            ));
        }
        let prefix = match optional_path(arguments, "path_prefix", true) {
            Ok(prefix) => prefix,
            Err(error) => return Ok(error),
        };
        let max_files = match limits::bounded_usize(
            arguments,
            "max_files",
            limits::MAX_NOTE_SEARCH_FILES,
            limits::MAX_NOTE_SEARCH_FILES,
        ) {
            Ok(max_files) => max_files,
            Err(error) => return Ok(error),
        };
        let max_matches = match limits::bounded_usize(
            arguments,
            "max_matches_per_file",
            limits::MAX_NOTE_MATCHES_PER_FILE,
            limits::MAX_NOTE_MATCHES_PER_FILE,
        ) {
            Ok(max_matches) => max_matches,
            Err(error) => return Ok(error),
        };
        let recent_first = arguments
            .get("recent_file_first")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let search_root = self.notes_root.clone();
        let directory_is_safe = match self.existing_directory_is_safe(&search_root) {
            Ok(directory_is_safe) => directory_is_safe,
            Err(_) => return Ok(scan_error("unsafe_path")),
        };
        if !directory_is_safe {
            return Ok(json!({
                "status": "ok",
                "matches": [],
                "files_scanned": 0,
                "truncated": false,
            }));
        }

        let mut files = Vec::new();
        let mut scan_truncated = false;
        if self
            .collect_files(&search_root, &mut files, &mut scan_truncated)
            .is_err()
        {
            return Ok(scan_error("io_error"));
        }
        if let Some(prefix) = &prefix {
            let prefix = virtual_path(prefix).map_err(|error| error.to_string())?;
            files.retain(|file| file.path.starts_with(&prefix));
        }
        files.sort_by(|left, right| compare_files(left, right, FileOrder::CreatedAt, recent_first));
        let file_count = files.len();

        let mut result = json!({
            "status": "ok",
            "matches": [],
            "files_scanned": 0,
            "files_with_matches": 0,
            "errors": [],
            "truncated": scan_truncated,
        });
        for file in files.into_iter().take(max_files) {
            result["files_scanned"] = json!(result["files_scanned"].as_u64().unwrap_or(0) + 1);
            let contents = match self.read_note(&file.absolute_path) {
                Ok(contents) => contents,
                Err(error) => {
                    limits::array_mut(&mut result, "errors")
                        .map_err(|error| error.to_string())?
                        .push(json!({"path": file.path.clone(), "reason": error.reason()}));
                    result["truncated"] = Value::Bool(true);
                    continue;
                }
            };
            let mut file_match_count = 0;
            for (line_number, line) in contents.lines().enumerate() {
                if !line.contains(query) {
                    continue;
                }
                file_match_count += 1;
                if file_match_count > max_matches {
                    result["truncated"] = Value::Bool(true);
                    break;
                }
                let (text, text_truncated) = bounded_text(line, limits::MAX_NOTE_MATCH_TEXT_CHARS);
                if text_truncated {
                    result["truncated"] = Value::Bool(true);
                }
                limits::array_mut(&mut result, "matches")
                    .map_err(|error| error.to_string())?
                    .push(json!({
                        "path": file.path.clone(),
                        "line": line_number + 1,
                        "text": text,
                        "truncated": text_truncated,
                    }));
                if serialized_len(&result) > limits::MAX_NOTE_RESULT_BYTES {
                    limits::array_mut(&mut result, "matches")
                        .map_err(|error| error.to_string())?
                        .pop();
                    result["truncated"] = Value::Bool(true);
                    break;
                }
            }
            if file_match_count > 0 {
                result["files_with_matches"] =
                    json!(result["files_with_matches"].as_u64().unwrap_or(0) + 1);
            }
        }
        if max_files < limits::MAX_NOTE_SEARCH_FILES
            && result["files_scanned"].as_u64() == Some(max_files as u64)
        {
            result["truncated"] = Value::Bool(true);
        }
        if file_count > max_files {
            result["truncated"] = Value::Bool(true);
        }
        bound_array_field(&mut result, "matches");
        bound_array_field(&mut result, "errors");
        Ok(result)
    }

    pub(super) fn append_file(&self, arguments: &Value) -> Result<Value, String> {
        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            return Ok(limits::invalid_argument(
                "path",
                "must be a non-empty string",
            ));
        };
        let Some(text) = arguments.get("text").and_then(Value::as_str) else {
            return Ok(limits::invalid_argument("text", "must be a string"));
        };
        let relative_path = match validated_relative_path(path, "path", false) {
            Ok(relative_path) => relative_path,
            Err(error) => return Ok(error),
        };
        if text.len() > limits::MAX_NOTE_BYTES {
            return Ok(note_too_large(path));
        }
        let absolute_path = self.notes_root.join(&relative_path);
        let existing = match self.read_note_if_present(&absolute_path) {
            Ok(Some(contents)) => contents,
            Ok(None) => String::new(),
            Err(error) => return Ok(note_error(path, error)),
        };
        if existing.len().saturating_add(text.len()) > limits::MAX_NOTE_BYTES {
            return Ok(note_too_large(path));
        }
        let mut combined = existing;
        combined.push_str(text);
        match self.atomic_write(&absolute_path, combined.as_bytes()) {
            Ok(()) => Ok(json!({
                "status": "ok",
                "path": path,
                "bytes": combined.len(),
                "appended_bytes": text.len(),
            })),
            Err(error) => Ok(note_error(path, NoteError::from_io(error))),
        }
    }

    pub(super) fn write_file(&self, arguments: &Value) -> Result<Value, String> {
        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            return Ok(limits::invalid_argument(
                "path",
                "must be a non-empty string",
            ));
        };
        let Some(text) = arguments.get("text").and_then(Value::as_str) else {
            return Ok(limits::invalid_argument("text", "must be a string"));
        };
        let relative_path = match validated_relative_path(path, "path", false) {
            Ok(relative_path) => relative_path,
            Err(error) => return Ok(error),
        };
        if text.len() > limits::MAX_NOTE_BYTES {
            return Ok(note_too_large(path));
        }
        let absolute_path = self.notes_root.join(relative_path);
        match self.atomic_write(&absolute_path, text.as_bytes()) {
            Ok(()) => Ok(json!({
                "status": "ok",
                "path": path,
                "bytes": text.len(),
            })),
            Err(error) => Ok(note_error(path, NoteError::from_io(error))),
        }
    }

    pub(super) fn thread_hint(&self) -> Value {
        let mut text = String::from("Local notes progress:\n");
        let index_path = self.notes_root.join("INDEX.md");
        match self.read_note(&index_path) {
            Ok(index) => {
                let (excerpt, truncated) = bounded_text(&index, limits::MAX_THREAD_HINT_BYTES / 2);
                text.push_str(&excerpt);
                if truncated {
                    text.push_str("\n[INDEX.md excerpt truncated]");
                }
            }
            Err(NoteError::NotFound) => text.push_str("(INDEX.md is not present)"),
            Err(error) => text.push_str(&format!("(INDEX.md unavailable: {})", error.reason())),
        }
        text.push_str("\nFiles:\n");
        let mut files = Vec::new();
        let mut scan_truncated = false;
        if self
            .existing_directory_is_safe(&self.notes_root)
            .ok()
            .unwrap_or(false)
        {
            let _ = self.collect_files(&self.notes_root, &mut files, &mut scan_truncated);
        }
        files.sort_by(|left, right| compare_files(left, right, FileOrder::UpdatedAt, true));
        for file in files.into_iter().take(limits::MAX_THREAD_HINT_FILES) {
            text.push_str(&format!("- {} ({} bytes)\n", file.path, file.bytes));
            if text.len() > limits::MAX_THREAD_HINT_BYTES {
                break;
            }
        }
        if scan_truncated {
            text.push_str("[file inventory truncated]\n");
        }
        let (text, _) = bounded_text(&text, limits::MAX_THREAD_HINT_BYTES);
        json!({"text": text})
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NoteError {
    NotFound,
    UnsafePath,
    NotAFile,
    TooLarge,
    NonUtf8,
    Io,
}

impl NoteError {
    fn from_io(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::InvalidInput | io::ErrorKind::PermissionDenied => Self::UnsafePath,
            _ => Self::Io,
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::UnsafePath => "unsafe_path",
            Self::NotAFile => "not_a_file",
            Self::TooLarge => "too_large",
            Self::NonUtf8 => "non_utf8",
            Self::Io => "io_error",
        }
    }
}

#[cfg(test)]
#[path = "notes_tests.rs"]
mod tests;
