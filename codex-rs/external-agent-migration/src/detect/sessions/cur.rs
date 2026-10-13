use super::common::SessionFileCandidate;
use super::common::detect_recent_sessions;
use crate::model::ExternalAgentSessionImportLimits;
use crate::sessions::ExternalAgentSessionMigration;
use crate::sessions::SessionRecordFormat;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

const MAX_CUR_PROJECT_PATH_ENTRIES: usize = 4096;
const MAX_CUR_PROJECT_PATH_STATES: usize = 256;

pub fn detect_recent_cur_sessions(
    external_agent_home: &Path,
    codex_home: &Path,
) -> io::Result<Vec<ExternalAgentSessionMigration>> {
    detect_recent_cur_sessions_with_limits(
        external_agent_home,
        codex_home,
        ExternalAgentSessionImportLimits::default(),
    )
}

pub(crate) fn detect_recent_cur_sessions_with_limits(
    external_agent_home: &Path,
    codex_home: &Path,
    limits: ExternalAgentSessionImportLimits,
) -> io::Result<Vec<ExternalAgentSessionMigration>> {
    let projects_root = external_agent_home.join("projects");
    if !projects_root.is_dir() {
        return Ok(Vec::new());
    }

    let mut candidates = Vec::new();
    for project_entry in fs::read_dir(projects_root)? {
        let Ok(project_entry) = project_entry else {
            continue;
        };
        let project_storage = project_entry.path();
        if !project_storage.is_dir() {
            continue;
        }
        let fallback_cwd = cur_project_cwd(&project_storage, external_agent_home);
        for path in cur_transcript_files(&project_storage.join("agent-transcripts")) {
            candidates.push(SessionFileCandidate {
                path,
                fallback_cwd: fallback_cwd.clone(),
                record_format: SessionRecordFormat::Cur,
            });
        }
    }
    detect_recent_sessions(
        codex_home, candidates, /*require_existing_cwd*/ false, limits,
    )
}

fn cur_transcript_files(transcripts_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![transcripts_root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if entry.file_name() != "subagents" {
                    pending.push(path);
                }
            } else if file_type.is_file()
                && path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn cur_project_cwd(project_storage: &Path, external_agent_home: &Path) -> Option<PathBuf> {
    let encoded = project_storage.file_name()?.to_str()?;
    // Cursor stores projectless chats under this reserved project name.
    if encoded == "empty-window" {
        let external_agent_home = if external_agent_home.is_absolute() {
            external_agent_home.to_path_buf()
        } else {
            std::env::current_dir().ok()?.join(external_agent_home)
        };
        return external_agent_home.parent().map(Path::to_path_buf);
    }
    decode_cur_project_path(encoded)
}

fn decode_cur_project_path(encoded: &str) -> Option<PathBuf> {
    #[cfg(not(windows))]
    let root = PathBuf::from("/");

    #[cfg(windows)]
    let (encoded, root) = {
        let (drive, encoded) = decode_cur_windows_project_drive(encoded)?;
        (encoded, PathBuf::from(format!("{drive}:\\")))
    };

    let encoded = encoded.strip_prefix('-').unwrap_or(encoded);
    if encoded.is_empty()
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return None;
    }

    // Cursor's slug loses separators and punctuation. Compare each complete
    // native path against both observed encodings; encoding components on
    // their own would miss runs spanning a directory boundary.
    let mut pending = vec![root];
    let mut matched_path = None;
    let mut entries_seen = 0;
    let mut states_seen = 1;
    while let Some(parent) = pending.pop() {
        // A traversable but unlistable directory may hide another matching
        // project. Never accept a match until every possible branch is read.
        for entry in fs::read_dir(&parent).ok()? {
            let entry = entry.ok()?;
            if entries_seen >= MAX_CUR_PROJECT_PATH_ENTRIES {
                return None;
            }
            entries_seen += 1;
            let name = entry.file_name();
            // Cursor operates on Unicode paths. An unrepresentable native name
            // cannot safely be ruled out as another spelling of this slug.
            name.to_str()?;
            let candidate = entry.path();
            let path = candidate.to_str()?;
            let (per_unit, collapsed) = encode_cur_project_path(path)?;
            let matches = [per_unit.as_str(), collapsed.as_str()];
            let relevant = matches.iter().any(|candidate| {
                encoded
                    .get(..candidate.len())
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(candidate))
            });
            if !relevant {
                continue;
            }
            if !fs::metadata(&candidate).ok()?.is_dir() {
                continue;
            }
            let exact = matches.contains(&encoded);
            let possible = matches
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(encoded));
            if possible && !exact {
                // A differently cased on-disk name can be the same cwd on a
                // case-insensitive filesystem. It may hide a second match.
                return None;
            }
            if exact {
                if matched_path
                    .as_ref()
                    .is_some_and(|matched_path| matched_path != &candidate)
                {
                    return None;
                }
                matched_path = Some(candidate.clone());
            }
            if matches.iter().any(|candidate| {
                encoded
                    .get(..candidate.len())
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(candidate))
                    && (candidate.len() < encoded.len() || candidate.ends_with('-'))
            }) {
                if states_seen >= MAX_CUR_PROJECT_PATH_STATES {
                    return None;
                }
                states_seen += 1;
                pending.push(candidate);
            }
        }
    }

    matched_path
}

fn encode_cur_project_path(path: &str) -> Option<(String, String)> {
    let mut per_unit = String::with_capacity(path.len());
    let mut collapsed = String::with_capacity(path.len());
    for unit in path.encode_utf16() {
        let byte = u8::try_from(unit).ok();
        let ascii = byte.filter(u8::is_ascii_alphanumeric);
        if let Some(byte) = ascii {
            let character = char::from(byte);
            per_unit.push(character);
            collapsed.push(character);
        } else {
            per_unit.push('-');
            if !collapsed.ends_with('-') {
                collapsed.push('-');
            }
        }
    }
    #[cfg(windows)]
    let (per_unit, collapsed) = (per_unit.get(2..)?, collapsed.get(2..)?);
    #[cfg(not(windows))]
    let (per_unit, collapsed) = (per_unit.as_str(), collapsed.as_str());
    Some((
        per_unit.strip_prefix('-').unwrap_or(per_unit).to_string(),
        collapsed.strip_prefix('-').unwrap_or(collapsed).to_string(),
    ))
}

#[cfg(any(windows, test))]
fn decode_cur_windows_project_drive(encoded: &str) -> Option<(char, &str)> {
    let drive = encoded.as_bytes().first().copied()?;
    if !drive.is_ascii_alphabetic() || encoded.as_bytes().get(1) != Some(&b'-') {
        return None;
    }

    Some((char::from(drive), encoded.get(2..)?))
}

#[cfg(test)]
#[path = "cur_tests.rs"]
mod tests;
