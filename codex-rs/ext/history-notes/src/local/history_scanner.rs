use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;

use codex_protocol::ThreadId;
use codex_rollout::CompactedItem;
use codex_rollout::RolloutItem;
use codex_rollout::decode_rollout_line;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::BufReader;

use super::history_projection::ProjectedItem;
use super::history_projection::ToolCallRef;
use super::history_projection::project_response_item;
use super::history_projection::project_rollout_item;
use super::limits;

#[derive(Clone, Debug)]
pub(crate) struct SourceRef {
    pub(crate) ordinal: Option<u64>,
    pub(crate) physical_record: usize,
    pub(crate) replacement_index: Option<usize>,
}

impl SourceRef {
    pub(crate) fn item_id(&self) -> String {
        let base = self.ordinal.map_or_else(
            || format!("record:{}", self.physical_record),
            |ordinal| format!("ordinal:{ordinal}"),
        );
        self.replacement_index
            .map_or(base.clone(), |index| format!("{base}:replacement:{index}"))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HistoryEntry {
    pub(crate) item_id: String,
    pub(crate) window_number: u64,
    pub(crate) window_id: String,
    pub(crate) timestamp: String,
    pub(crate) source: SourceRef,
    pub(crate) projection: ProjectedItem,
}

#[derive(Clone, Debug)]
pub(crate) struct WindowSummary {
    pub(crate) window_number: u64,
    pub(crate) window_id: String,
    pub(crate) context_window_id: Option<String>,
    pub(crate) item_count: usize,
    pub(crate) readable_item_count: usize,
    pub(crate) opaque_item_count: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct ScanInfo {
    pub(crate) records_scanned: usize,
    pub(crate) complete: bool,
    pub(crate) truncated: bool,
    pub(crate) corrupt: bool,
    pub(crate) opaque_records: usize,
    pub(crate) inherited_records_skipped: usize,
    pub(crate) errors: Vec<String>,
    pub(crate) lineage_unavailable: bool,
}

#[derive(Debug)]
pub(crate) struct ScanResult {
    pub(crate) entries: Vec<HistoryEntry>,
    pub(crate) windows: BTreeMap<u64, WindowSummary>,
    pub(crate) info: ScanInfo,
}

struct ScannerState {
    thread_id: ThreadId,
    entries: Vec<HistoryEntry>,
    windows: BTreeMap<u64, WindowSummary>,
    info: ScanInfo,
    current_window: u64,
    ordinal_floor: Option<u64>,
    tool_calls: HashMap<String, ToolCallRef>,
}

impl ScannerState {
    fn new(thread_id: ThreadId) -> Self {
        Self {
            thread_id,
            entries: Vec::new(),
            windows: BTreeMap::new(),
            info: ScanInfo {
                records_scanned: 0,
                complete: true,
                truncated: false,
                corrupt: false,
                opaque_records: 0,
                inherited_records_skipped: 0,
                errors: Vec::new(),
                lineage_unavailable: false,
            },
            current_window: 0,
            ordinal_floor: None,
            tool_calls: HashMap::new(),
        }
    }

    fn add_error(&mut self, error: impl Into<String>) {
        self.info.complete = false;
        if self.info.errors.len() >= limits::MAX_HISTORY_SCAN_ERRORS {
            return;
        }
        let error = error.into();
        let (error, _, _) = limits::char_slice(&error, 0, limits::MAX_HISTORY_ERROR_CHARS);
        self.info.errors.push(error.to_string());
    }

    fn ensure_window(&mut self, number: u64, context_window_id: Option<String>) -> bool {
        if let Some(window) = self.windows.get_mut(&number) {
            if context_window_id.is_some() {
                window.context_window_id = context_window_id;
            }
            return true;
        }
        if self.windows.len() >= limits::MAX_HISTORY_SCAN_WINDOWS {
            self.info.truncated = true;
            self.info.complete = false;
            return false;
        }
        self.windows.insert(
            number,
            WindowSummary {
                window_number: number,
                window_id: window_id(self.thread_id, number),
                context_window_id,
                item_count: 0,
                readable_item_count: 0,
                opaque_item_count: 0,
            },
        );
        true
    }

    fn push_entry(
        &mut self,
        timestamp: &str,
        ordinal: Option<u64>,
        physical_record: usize,
        replacement_index: Option<usize>,
        projection: ProjectedItem,
    ) {
        if self.entries.len() >= limits::MAX_HISTORY_SCAN_RECORDS {
            self.info.truncated = true;
            self.info.complete = false;
            return;
        }
        if !self.ensure_window(self.current_window, None) {
            return;
        }
        let source = SourceRef {
            ordinal,
            physical_record,
            replacement_index,
        };
        let entry = HistoryEntry {
            item_id: source.item_id(),
            window_number: self.current_window,
            window_id: window_id(self.thread_id, self.current_window),
            timestamp: timestamp.to_string(),
            source,
            projection,
        };
        if let Some(window) = self.windows.get_mut(&self.current_window) {
            window.item_count = window.item_count.saturating_add(1);
            if entry.projection.content.is_some() {
                window.readable_item_count = window.readable_item_count.saturating_add(1);
            }
            if entry.projection.opaque {
                window.opaque_item_count = window.opaque_item_count.saturating_add(1);
                self.info.opaque_records = self.info.opaque_records.saturating_add(1);
            }
        }
        self.entries.push(entry);
    }

    fn should_skip_ordinal(&mut self, ordinal: Option<u64>) -> bool {
        let Some(floor) = self.ordinal_floor else {
            return false;
        };
        if ordinal.is_some_and(|ordinal| ordinal < floor) {
            self.info.inherited_records_skipped =
                self.info.inherited_records_skipped.saturating_add(1);
            return true;
        }
        false
    }

    fn finish(self) -> ScanResult {
        ScanResult {
            entries: self.entries,
            windows: self.windows,
            info: self.info,
        }
    }
}

pub(crate) async fn scan_rollout(path: &Path, thread_id: ThreadId) -> Result<ScanResult, String> {
    // The active native writer owns a plain rollout, including after resume.
    // Freeze its byte prefix and bound line allocation; keep the native decoder.
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| format!("cannot open active rollout: {error}"))?;
    let snapshot_len = file
        .metadata()
        .await
        .map_err(|error| error.to_string())?
        .len();
    let mut reader = BufReader::new(file.take(snapshot_len.min(limits::MAX_HISTORY_SCAN_BYTES)));
    let mut state = ScannerState::new(thread_id);
    let mut physical_record = 0usize;
    let mut saw_identity = false;
    let mut bytes_scanned = 0u64;

    loop {
        if state.info.records_scanned >= limits::MAX_HISTORY_SCAN_RECORDS {
            state.info.truncated = true;
            state.info.complete = false;
            break;
        }
        let mut line = Vec::new();
        let read = match (&mut reader)
            .take(limits::MAX_HISTORY_RECORD_BYTES + 1)
            .read_until(b'\n', &mut line)
            .await
        {
            Ok(0) => {
                if bytes_scanned < snapshot_len {
                    state.info.truncated = true;
                    state.add_error("rollout scan byte limit reached or snapshot shortened");
                }
                break;
            }
            Ok(read) => read,
            Err(error) => {
                state.add_error(format!("rollout read failed: {error}"));
                state.info.corrupt = true;
                break;
            }
        };
        bytes_scanned = bytes_scanned.saturating_add(read as u64);
        if read as u64 > limits::MAX_HISTORY_RECORD_BYTES {
            state.info.truncated = true;
            state.add_error("rollout record exceeds the bounded reader limit");
            break;
        }
        if !line.ends_with(b"\n") {
            state.info.truncated = true;
            state.add_error("incomplete rollout tail or scan byte limit");
            break;
        }
        let record_number = physical_record;
        physical_record = physical_record.saturating_add(1);
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        state.info.records_scanned = state.info.records_scanned.saturating_add(1);
        let value = match serde_json::from_slice::<Value>(&line) {
            Ok(value) => value,
            Err(error) => {
                if !saw_identity {
                    return Err(format!("rollout identity record is malformed: {error}"));
                }
                state.add_error(format!("malformed rollout record {record_number}: {error}"));
                state.info.corrupt = true;
                break;
            }
        };
        let rollout_line = match decode_rollout_line(value) {
            Ok(line) => line,
            Err(error) => {
                if !saw_identity {
                    return Err(format!("rollout identity record is malformed: {error}"));
                }
                state.add_error(format!(
                    "undecodable rollout record {record_number}: {error}"
                ));
                state.info.corrupt = true;
                break;
            }
        };

        if !saw_identity {
            let RolloutItem::SessionMeta(meta_line) = &rollout_line.item else {
                return Err("rollout does not start with session metadata".to_string());
            };
            if meta_line.meta.id != thread_id {
                return Err(format!(
                    "rollout identity mismatch: expected thread {thread_id}, found {}",
                    meta_line.meta.id
                ));
            }
            saw_identity = true;
            state.info.lineage_unavailable = meta_line.meta.history_base.is_some()
                || meta_line.meta.forked_from_id.is_some()
                || meta_line.meta.forked_from_ordinal_exclusive.is_some()
                || meta_line.meta.subagent_history_start_ordinal.is_some();
            state.ordinal_floor = lineage_ordinal_floor(meta_line);
            state.ensure_window(
                0,
                meta_line
                    .meta
                    .context_window
                    .as_ref()
                    .map(|window| window.window_id.clone()),
            );
            continue;
        }

        match &rollout_line.item {
            RolloutItem::SessionMeta(_) => {
                state.add_error(format!(
                    "duplicate session metadata at record {record_number}"
                ));
            }
            RolloutItem::Compacted(compacted) => {
                process_compaction(&mut state, compacted, &rollout_line, record_number);
            }
            item => {
                if state.should_skip_ordinal(rollout_line.ordinal) {
                    continue;
                }
                if let Some(projection) = project_rollout_item(item, &mut state.tool_calls) {
                    state.push_entry(
                        rollout_line.timestamp.as_str(),
                        rollout_line.ordinal,
                        record_number,
                        None,
                        projection,
                    );
                } else if !matches!(
                    item,
                    RolloutItem::TurnContext(_) | RolloutItem::WorldState(_)
                ) {
                    state.info.opaque_records = state.info.opaque_records.saturating_add(1);
                }
            }
        }
    }

    if !saw_identity {
        return Err("rollout has no session metadata".to_string());
    }
    Ok(state.finish())
}

fn process_compaction(
    state: &mut ScannerState,
    compacted: &CompactedItem,
    line: &codex_rollout::RolloutLine,
    physical_record: usize,
) {
    if state.should_skip_ordinal(line.ordinal) {
        return;
    }
    let Some(window_number) = compacted.window_number else {
        state.add_error(format!(
            "compaction at record {physical_record} has no window number"
        ));
        state.info.corrupt = true;
        state.info.opaque_records = state.info.opaque_records.saturating_add(1);
        return;
    };
    if window_number < state.current_window {
        state.add_error(format!(
            "compaction window moved backwards from {} to {window_number}",
            state.current_window
        ));
        state.info.corrupt = true;
        return;
    }
    state.current_window = window_number;
    if !state.ensure_window(window_number, compacted.window_id.clone()) {
        return;
    }
    state.push_entry(
        line.timestamp.as_str(),
        line.ordinal,
        physical_record,
        None,
        ProjectedItem::opaque("system", "compaction_boundary", "compaction_boundary"),
    );
    let Some(replacement_history) = compacted.replacement_history.as_deref() else {
        return;
    };
    for (index, item) in replacement_history
        .iter()
        .take(limits::MAX_HISTORY_REPLACEMENT_ITEMS)
        .enumerate()
    {
        let projection = project_response_item(&item.item, &mut state.tool_calls);
        state.push_entry(
            line.timestamp.as_str(),
            line.ordinal,
            physical_record,
            Some(index),
            projection,
        );
    }
    if replacement_history.len() > limits::MAX_HISTORY_REPLACEMENT_ITEMS {
        state.info.truncated = true;
        state.info.complete = false;
        state.add_error(format!(
            "compaction replacement history exceeded {} items",
            limits::MAX_HISTORY_REPLACEMENT_ITEMS
        ));
    }
}

fn lineage_ordinal_floor(meta: &codex_protocol::protocol::SessionMetaLine) -> Option<u64> {
    [
        meta.meta
            .history_base
            .map(|position| position.end_ordinal_exclusive),
        meta.meta.forked_from_ordinal_exclusive,
        meta.meta.subagent_history_start_ordinal,
    ]
    .into_iter()
    .flatten()
    .max()
}

fn window_id(thread_id: ThreadId, window_number: u64) -> String {
    format!("{thread_id}:{window_number}")
}

#[cfg(test)]
#[path = "history_scanner_tests.rs"]
mod tests;
