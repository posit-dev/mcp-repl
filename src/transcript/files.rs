//! Capture files are created here and written only by the recording writer.
//! A producer creates its output file through a bounded helper, so a storage
//! call that does not return costs it one wait, not its thread.
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::queue::WRITER_WAIT;
use super::{create_private_directory, create_private_file};

#[derive(Default)]
pub(super) struct CaptureStorage {
    files: Mutex<Option<CaptureFiles>>,
    pub(super) public_directory: Arc<Mutex<PathBuf>>,
    /// Creations whose producer stopped waiting and which have not returned.
    abandoned_creates: AtomicUsize,
}

struct CaptureFiles {
    temporary: Option<tempfile::TempDir>,
    directory: PathBuf,
    active: BTreeMap<String, std::sync::Weak<Mutex<CaptureFile>>>,
}

/// Publication relocates live producers while completed files have no writer.
pub(super) struct CaptureFile {
    pub(super) writer: Option<File>,
    pub(super) path: PathBuf,
    complete: bool,
}

impl CaptureStorage {
    fn with_files<T>(
        &self,
        operation: impl FnOnce(&mut CaptureFiles) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut state = self
            .files
            .lock()
            .map_err(|_| "capture storage lock poisoned")?;
        if state.is_none() {
            let temporary = tempfile::tempdir()
                .map_err(|e| format!("failed to create temporary capture: {e}"))?;
            let directory = temporary.path().to_owned();
            create_private_directory(&directory.join("capture/sessions"), true)
                .map_err(|e| e.to_string())?;
            *state = Some(CaptureFiles {
                temporary: Some(temporary),
                directory,
                active: BTreeMap::new(),
            });
        }
        operation(state.as_mut().expect("initialized capture storage"))
    }

    pub(super) fn directory(&self) -> Result<PathBuf, String> {
        self.with_files(|files| Ok(files.directory.clone()))
    }

    pub(super) fn create_file(&self, relative: &str) -> Result<Arc<Mutex<CaptureFile>>, String> {
        self.with_files(|files| {
            let path = files.directory.join(relative);
            create_private_directory(path.parent().expect("capture parent"), true)
                .map_err(|e| e.to_string())?;
            let public_path = self
                .public_directory
                .lock()
                .map_err(|_| "recording path lock poisoned")?
                .join(relative);
            let writer = create_private_file(&path)
                .map_err(|e| format!("failed to create {}: {e}", public_path.display()))?;
            let file = Arc::new(Mutex::new(CaptureFile {
                writer: Some(writer),
                path,
                complete: false,
            }));
            files.active.retain(|_, file| file.strong_count() != 0);
            files
                .active
                .insert(relative.to_owned(), Arc::downgrade(&file));
            Ok(file)
        })
    }

    /// Create a producer's output file, waiting at most `WRITER_WAIT`.
    ///
    /// The creation runs on a helper thread. A helper that outlives the wait
    /// is abandoned: it may still leave an empty file, and later creations
    /// fail at once until it returns, so a stalled filesystem holds one thread.
    pub(super) fn create_output_file(
        self: &Arc<Self>,
        relative: &str,
    ) -> Result<Arc<Mutex<CaptureFile>>, String> {
        if self.abandoned_creates.load(Ordering::Acquire) != 0 {
            return Err("recording storage is not responding".to_string());
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        // Decides, under one lock, whether the helper delivers or is abandoned.
        let abandoned = Arc::new(Mutex::new(false));
        let helper = (self.clone(), relative.to_owned(), abandoned.clone());
        std::thread::spawn(move || {
            let (storage, relative, abandoned) = helper;
            let result = storage.create_file(&relative);
            let abandoned = abandoned.lock().expect("output creation lock");
            if *abandoned {
                storage.abandoned_creates.fetch_sub(1, Ordering::Release);
            } else {
                let _ = sender.send(result);
            }
        });
        if let Ok(result) = receiver.recv_timeout(WRITER_WAIT) {
            return result;
        }
        let mut abandoned = abandoned.lock().expect("output creation lock");
        if let Ok(result) = receiver.try_recv() {
            return result;
        }
        *abandoned = true;
        self.abandoned_creates.fetch_add(1, Ordering::Release);
        Err(format!(
            "recording storage did not respond within {} seconds",
            WRITER_WAIT.as_secs()
        ))
    }

    /// Writer only.
    pub(super) fn write_image(&self, relative: &str, bytes: &[u8]) -> Result<(), String> {
        let file = self.create_file(relative)?;
        let mut file = file.lock().map_err(|_| "image lock poisoned")?;
        file.writer
            .as_mut()
            .expect("image writer")
            .write_all(bytes)
            .map_err(|e| e.to_string())?;
        file.finish()
    }

    pub(super) fn relocate(&self, destination: &Path) -> Result<(), String> {
        self.with_files(|files| {
            // No journal or projection write runs while this registry is held.
            for (relative, file) in &files.active {
                if let Some(file) = file.upgrade() {
                    file.lock()
                        .map_err(|_| "capture file lock poisoned")?
                        .relocate(&destination.join(relative))?;
                }
            }
            copy_capture(&files.directory, destination)?;
            files.directory = destination.to_owned();
            Ok(())
        })
    }

    pub(super) fn finish_publication(&self) -> Result<(), String> {
        // Keep scratch ownership if reopening a journal/projection fails. Its
        // read-only files still need explicit cleanup after those handles retire.
        let temporary = self.with_files(|files| Ok(files.temporary.take()))?;
        if let Some(temporary) = temporary {
            discard_temporary_capture(temporary);
        }
        Ok(())
    }

    pub(super) fn finalize(&self) -> Result<(), String> {
        let mut state = self
            .files
            .lock()
            .map_err(|_| "capture storage lock poisoned")?;
        let Some(files) = state.as_mut() else {
            return Ok(());
        };
        {
            let mut errors = Vec::new();
            for file in files.active.values().filter_map(std::sync::Weak::upgrade) {
                if let Err(error) = file
                    .lock()
                    .map_err(|_| "capture file lock poisoned".to_string())
                    .and_then(|mut f| f.finish())
                {
                    errors.push(error);
                }
            }
            files.active.clear();
            if let Err(error) = finalize_files(&files.directory) {
                errors.push(error);
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            }
        }
    }
}

impl Drop for CaptureFiles {
    fn drop(&mut self) {
        if let Some(temporary) = self.temporary.take() {
            discard_temporary_capture(temporary);
        }
    }
}

pub(super) fn discard_temporary_capture(temporary: tempfile::TempDir) {
    // Windows deletion can reject read-only files. This tree is private scratch
    // storage; clearing the flag here never changes the published recording.
    #[cfg(windows)]
    // This changes the Windows file attribute, preserving its existing ACL.
    // The lint's world-writable Unix behavior cannot occur in this function.
    #[allow(clippy::permissions_set_readonly_false)]
    fn clear_readonly(directory: &Path) -> std::io::Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                clear_readonly(&entry.path())?;
            } else {
                let mut permissions = entry.metadata()?.permissions();
                permissions.set_readonly(false);
                std::fs::set_permissions(entry.path(), permissions)?;
            }
        }
        Ok(())
    }
    #[cfg(windows)]
    if let Err(error) = clear_readonly(temporary.path()) {
        let _ = writeln!(
            std::io::stderr().lock(),
            "mcp-console: temporary recording cleanup failed: {error}"
        );
    }
    if let Err(error) = temporary.close() {
        let _ = writeln!(
            std::io::stderr().lock(),
            "mcp-console: temporary recording cleanup failed: {error}"
        );
    }
}
impl CaptureFile {
    pub(super) fn finish(&mut self) -> Result<(), String> {
        if self.complete {
            return Ok(());
        }
        if let Some(mut writer) = self.writer.take() {
            writer.flush().map_err(|e| e.to_string())?;
        }
        make_read_only(&self.path)?;
        self.complete = true;
        Ok(())
    }
    fn relocate(&mut self, destination: &Path) -> Result<(), String> {
        if let Some(writer) = &mut self.writer {
            writer.flush().map_err(|e| e.to_string())?;
        }
        create_private_directory(destination.parent().expect("capture parent"), true)
            .map_err(|e| e.to_string())?;
        std::fs::copy(&self.path, destination).map_err(|e| e.to_string())?;
        self.writer = if self.complete {
            None
        } else {
            Some(
                OpenOptions::new()
                    .append(true)
                    .open(destination)
                    .map_err(|e| e.to_string())?,
            )
        };
        self.path = destination.to_owned();
        Ok(())
    }
}
fn copy_capture(source: &Path, destination: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(source).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = destination.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            create_private_directory(&target, true).map_err(|e| e.to_string())?;
            copy_capture(&entry.path(), &target)?;
        } else if !target.exists() {
            std::fs::copy(entry.path(), target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
fn finalize_files(directory: &Path) -> Result<(), String> {
    let mut errors = Vec::new();
    for entry in std::fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        let result = if kind.is_dir() {
            finalize_files(&entry.path())
        } else if kind.is_file() {
            make_read_only(&entry.path())
        } else {
            Ok(())
        };
        if let Err(error) = result {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}
fn make_read_only(path: &Path) -> Result<(), String> {
    let mut permissions = std::fs::metadata(path)
        .map_err(|e| e.to_string())?
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        permissions.set_mode(permissions.mode() & !0o222);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions)
        .map_err(|e| format!("failed to make {} read-only: {e}", path.display()))
}

impl super::Transcript {
    pub(crate) fn persist_decoded_image(
        &self,
        call_id: Option<u64>,
        bytes: &[u8],
        mime_type: &str,
        encoded_bytes: usize,
    ) -> Option<super::Artifact> {
        let (id, owner, stem) = self.reserve_image(bytes.len(), encoded_bytes, mime_type)?;
        self.persist_reserved_image(
            id,
            super::ImageOwner::Interval(owner),
            call_id,
            stem,
            bytes,
            mime_type,
        )
    }

    fn persist_reserved_image(
        &self,
        id: u64,
        owner: super::ImageOwner,
        call_id: Option<u64>,
        stem: String,
        bytes: &[u8],
        mime_type: &str,
    ) -> Option<super::Artifact> {
        let extension = if mime_type == "image/png" {
            "png"
        } else {
            "bin"
        };
        let path = format!("{stem}.{extension}");
        // The writer creates and writes the file before journaling it. A
        // failure there disables recording; this receipt is already issued.
        self.queue_data(super::queue::QueuedEvent::ImageRetained {
            owner,
            artifact_id: id,
            call_id,
            path: path.clone(),
            mime_type: mime_type.to_owned(),
            bytes: bytes.len(),
            data: bytes.to_vec(),
        });
        Some(super::Artifact {
            id,
            path,
            mime_type: mime_type.to_owned(),
            owner: match owner {
                super::ImageOwner::Interval(owner) => Some(owner),
                super::ImageOwner::Call(_) => None,
            },
            public_directory: self.3.public_directory.clone(),
        })
    }
    pub(crate) fn persist_session_image(
        &self,
        data: &str,
        mime_type: &str,
    ) -> Result<Option<super::Artifact>, String> {
        let bytes = super::decode_image_data(data)?;
        Ok(self.persist_decoded_image(None, &bytes, mime_type, data.len()))
    }
}

impl super::CallImages {
    pub(crate) fn persist(
        &mut self,
        data: &str,
        mime_type: &str,
    ) -> Result<Option<super::Artifact>, String> {
        let bytes = super::decode_image_data(data)?;
        let Some((id, stem)) = self.transcript.reserve_call_image(
            self.owner,
            &mut self.budget,
            bytes.len(),
            data.len(),
            mime_type,
        ) else {
            return Ok(None);
        };
        Ok(self.transcript.persist_reserved_image(
            id,
            super::ImageOwner::Call(self.owner),
            Some(self.owner.call_id),
            stem,
            &bytes,
            mime_type,
        ))
    }
}
