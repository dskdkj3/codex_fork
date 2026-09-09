use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering as AtomicOrdering;

use super::LocalNoteStore;
use super::NoteError;
use super::NoteFile;
use super::limits;
use super::note_line_count;
use super::timestamp;
use super::virtual_path;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

impl LocalNoteStore {
    pub(super) fn read_note(&self, path: &Path) -> Result<String, NoteError> {
        let Some(contents) = self.read_note_if_present(path)? else {
            return Err(NoteError::NotFound);
        };
        Ok(contents)
    }

    pub(super) fn read_note_if_present(&self, path: &Path) -> Result<Option<String>, NoteError> {
        if !self.existing_path_is_safe(path)? {
            return Ok(None);
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(NoteError::from_io(error)),
        };
        if metadata.file_type().is_symlink() {
            return Err(NoteError::UnsafePath);
        }
        if !metadata.is_file() {
            return Err(NoteError::NotAFile);
        }
        if metadata.len() > (limits::MAX_NOTE_BYTES as u64) {
            return Err(NoteError::TooLarge);
        }
        let file = File::open(path).map_err(NoteError::from_io)?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take((limits::MAX_NOTE_BYTES as u64) + 1)
            .read_to_end(&mut bytes)
            .map_err(NoteError::from_io)?;
        if bytes.len() > limits::MAX_NOTE_BYTES {
            return Err(NoteError::TooLarge);
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| NoteError::NonUtf8)
    }

    pub(super) fn atomic_write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > limits::MAX_NOTE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "note exceeds byte limit",
            ));
        }
        self.ensure_directory_chain(&self.notes_root)?;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "note has no parent"))?;
        self.ensure_directory_chain(parent)?;
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "note path is a symlink",
                ));
            }
            if !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "note path is not a file",
                ));
            }
        }

        let counter = TEMP_FILE_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("note");
        let temporary_path = parent.join(format!(
            ".{file_name}.codex-tmp-{}-{counter}",
            std::process::id()
        ));
        let result = (|| {
            let mut temporary = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary_path)?;
            temporary.write_all(bytes)?;
            temporary.sync_all()?;
            drop(temporary);
            fs::rename(&temporary_path, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        result
    }

    pub(super) fn ensure_directory_chain(&self, path: &Path) -> io::Result<()> {
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component.as_os_str());
            match fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "note directory contains a symlink",
                        ));
                    }
                    if !metadata.is_dir() {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "note directory component is not a directory",
                        ));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub(super) fn existing_directory_is_safe(&self, path: &Path) -> Result<bool, String> {
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component.as_os_str());
            match fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() {
                        return Err("note directory contains a symlink".to_string());
                    }
                    if !metadata.is_dir() {
                        return Err("note path is not a directory".to_string());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(true)
    }

    pub(super) fn existing_path_is_safe(&self, path: &Path) -> Result<bool, NoteError> {
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component.as_os_str());
            match fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() {
                        return Err(NoteError::UnsafePath);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(NoteError::from_io(error)),
            }
        }
        Ok(true)
    }

    pub(super) fn collect_files(
        &self,
        directory: &Path,
        files: &mut Vec<NoteFile>,
        scan_truncated: &mut bool,
    ) -> Result<(), String> {
        if files.len() >= limits::MAX_NOTE_SCAN_FILES {
            *scan_truncated = true;
            return Ok(());
        }
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        for entry in entries {
            if files.len() >= limits::MAX_NOTE_SCAN_FILES {
                *scan_truncated = true;
                break;
            }
            let entry = entry.map_err(|error| error.to_string())?;
            let file_type = entry.file_type().map_err(|error| error.to_string())?;
            if file_type.is_symlink() {
                *scan_truncated = true;
                continue;
            }
            if file_type.is_dir() {
                self.collect_files(&entry.path(), files, scan_truncated)?;
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let absolute_path = entry.path();
            let relative_path = absolute_path
                .strip_prefix(&self.notes_root)
                .map_err(|error| error.to_string())?;
            let path = virtual_path(relative_path).map_err(|error| error.to_string())?;
            let metadata =
                fs::symlink_metadata(&absolute_path).map_err(|error| error.to_string())?;
            files.push(NoteFile {
                path,
                absolute_path,
                bytes: metadata.len(),
                lines: note_line_count(&entry.path()),
                created_at: timestamp(metadata.created().ok()),
                updated_at: timestamp(metadata.modified().ok()),
            });
        }
        Ok(())
    }
}
