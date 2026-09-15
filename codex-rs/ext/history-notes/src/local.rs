//! Thread-scoped local implementation of the history and notes semantics.
//!
//! The host must pass the active [`ThreadStore`] instance. This module never
//! discovers sessions globally and never opens a second rollout writer.

use std::path::PathBuf;
use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_thread_store::ThreadStore;
use serde_json::Value;
use serde_json::json;

mod history;
mod history_projection;
mod history_scanner;
mod limits;
mod notes;

use history::LocalHistoryReader;
use notes::LocalNoteStore;

pub(crate) const MAX_LOCAL_ARGUMENT_BYTES: usize = 4_000;
pub(crate) const MAX_NOTE_CALL_TEXT_BYTES: usize = 3_000;

/// A local result can replay only a typed, exactly selected native AgentMessage.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum LocalHistoryNotesResult {
    Json(Value),
    AgentMessageReplay(Vec<FunctionCallOutputContentItem>),
}

/// Small internal API consumed by the history-notes backend selector.
#[derive(Clone)]
pub(crate) struct LocalHistoryNotesStore {
    history: LocalHistoryReader,
    notes: LocalNoteStore,
    current_agent_name: String,
}

impl LocalHistoryNotesStore {
    pub(crate) fn new(
        thread_store: Arc<dyn ThreadStore>,
        thread_id: ThreadId,
        current_agent_name: String,
        storage_root: PathBuf,
    ) -> Self {
        Self {
            history: LocalHistoryReader::new(thread_store, thread_id),
            notes: LocalNoteStore::new(storage_root, thread_id),
            current_agent_name,
        }
    }

    /// Executes one of the existing nine semantic operations.
    ///
    /// Results are JSON or an atomic typed native AgentMessage replay. Expected missing, opaque, corrupt,
    /// and unsupported states are represented in the value instead of being
    /// collapsed into an empty successful response.
    pub(crate) async fn call(
        &self,
        endpoint: &str,
        arguments: Value,
    ) -> Result<LocalHistoryNotesResult, String> {
        if arguments.to_string().len() > MAX_LOCAL_ARGUMENT_BYTES {
            return Ok(LocalHistoryNotesResult::Json(limits::invalid_argument(
                "arguments",
                "local call JSON exceeds 4000 UTF-8 bytes",
            )));
        }
        if matches!(
            endpoint,
            "alpha/notes/v2/write_file" | "alpha/notes/v2/append_to_file"
        ) && arguments
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.len() > MAX_NOTE_CALL_TEXT_BYTES)
        {
            return Ok(LocalHistoryNotesResult::Json(limits::invalid_argument(
                "text",
                "local note text exceeds 3000 UTF-8 bytes per call; append smaller chunks",
            )));
        }
        if let Some(error) = self.reject_foreign_agent(&arguments) {
            return Ok(LocalHistoryNotesResult::Json(error));
        }

        if endpoint == "alpha/history/v2/read_item" {
            return self.history.read_item(&arguments).await;
        }
        let result = match endpoint {
            "alpha/history/v2/list_windows" => self.history.list_windows(&arguments).await,
            "alpha/history/v2/list_items" => self.history.list_items(&arguments).await,
            "alpha/history/v2/search_contents" => self.history.search_contents(&arguments).await,
            "alpha/notes/v2/list_files_by_prefix" => self.notes.list_files(&arguments),
            "alpha/notes/v2/read_file" => self.notes.read_file(&arguments),
            "alpha/notes/v2/search_contents" => self.notes.search_contents(&arguments),
            "alpha/notes/v2/append_to_file" => self.notes.append_file(&arguments),
            "alpha/notes/v2/write_file" => self.notes.write_file(&arguments),
            _ => Err(format!(
                "unsupported local history-notes endpoint: {endpoint}"
            )),
        }?;
        Ok(LocalHistoryNotesResult::Json(result))
    }

    /// Returns a bounded recovery hint for the existing context contribution
    /// slot. It may include a short `INDEX.md` progress excerpt and note-file
    /// inventory, but never an unbounded note body.
    pub(crate) fn thread_hint(&self) -> Value {
        self.notes.thread_hint()
    }

    fn reject_foreign_agent(&self, arguments: &Value) -> Option<Value> {
        let requested = arguments.get("agent_name")?;
        if requested.is_null() || requested.as_str() == Some(self.current_agent_name.as_str()) {
            return None;
        }

        Some(json!({
            "status": "unavailable",
            "reason": "cross_thread_history_is_not_available",
            "scope": {
                "thread_id": self.history.thread_id().to_string(),
                "agent_name": self.current_agent_name,
            },
        }))
    }
}

#[cfg(test)]
#[path = "local/local_tests.rs"]
mod tests;
