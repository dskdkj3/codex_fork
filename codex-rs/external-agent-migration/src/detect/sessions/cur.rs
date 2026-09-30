use super::common::SessionFileCandidate;
use super::common::detect_recent_sessions;
use crate::model::ExternalAgentSessionImportLimits;
use crate::sessions::ExternalAgentSessionMigration;
use crate::sessions::SessionRecordFormat;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

const MAX_CUR_PROJECT_PATH_PROBES: usize = 4096;
const MAX_CUR_PROJECT_PATH_STATES: usize = 256;
const CUR_PROJECT_SEPARATORS: [&str; 11] =
    ["-", "_", ".", " ", "--", "..", "__", "  ", "+", "@", "&"];

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
    let components = encoded.split('-').map(str::to_owned).collect::<Vec<_>>();
    for component in &components {
        if component.is_empty()
            || matches!(component.as_str(), "." | "..")
            || component.contains(['/', '\\', ':'])
        {
            return None;
        }
    }

    // Only expand a joined component when its parent and the joined directory
    // exist. This allows several punctuated ancestors without enumerating their
    // contents, while a global probe bound keeps ambiguous encodings cheap.
    let mut pending = vec![components];
    let mut seen = HashSet::new();
    let mut matched_path = None;
    let mut probes = 0;
    while let Some(components) = pending.pop() {
        if !seen.insert(components.clone()) {
            continue;
        }
        if seen.len() > MAX_CUR_PROJECT_PATH_STATES {
            return None;
        }
        let candidate = components
            .iter()
            .fold(root.clone(), |path, component| path.join(component));
        if probes >= MAX_CUR_PROJECT_PATH_PROBES {
            return None;
        }
        probes += 1;
        if cur_directory_exists(&candidate)? {
            if matched_path
                .as_ref()
                .is_some_and(|matched_path| matched_path != &candidate)
            {
                return None;
            }
            matched_path = Some(candidate);
        }

        let mut parent = root.clone();
        for start in 0..components.len() {
            if probes >= MAX_CUR_PROJECT_PATH_PROBES {
                return None;
            }
            probes += 1;
            if !cur_directory_exists(&parent)? {
                break;
            }
            for end in start + 2..=components.len() {
                for separator in CUR_PROJECT_SEPARATORS {
                    if probes >= MAX_CUR_PROJECT_PATH_PROBES {
                        return None;
                    }
                    probes += 1;
                    let merged = components[start..end].join(separator);
                    if !cur_directory_exists(&parent.join(&merged))? {
                        continue;
                    }
                    let mut next = components[..start].to_vec();
                    next.push(merged);
                    next.extend_from_slice(&components[end..]);
                    if !seen.contains(&next) {
                        pending.push(next);
                    }
                }
            }
            parent.push(&components[start]);
        }
    }

    matched_path
}

fn cur_directory_exists(path: &Path) -> Option<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Some(metadata.is_dir()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Some(false)
        }
        // An inaccessible candidate could hide another match. Reject the
        // encoding when the search cannot establish that it is unambiguous.
        Err(_) => None,
    }
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
