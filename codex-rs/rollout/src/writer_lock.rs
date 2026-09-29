//! Shared cross-process ownership for local thread writers and rollout publication.
//! Thread lock-file creation and removal uses the existing home coordination lock.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_protocol::ThreadId;
use tracing::warn;

const WRITER_LOCK_DIR: &str = "thread-writer-locks";
const COORDINATION_LOCK_FILE: &str = ".coordination.lock";

/// Coordinates writer ownership within one Codex home.
pub struct WriterLockCoordinator {
    directory: PathBuf,
    cleanup_attempted: AtomicBool,
}

/// Keeps a thread owned until all file work has finished.
pub struct WriterLockGuard {
    coordinator: Arc<WriterLockCoordinator>,
    path: PathBuf,
    file: Option<File>,
    coordination_mode: CoordinationMode,
}

#[derive(Clone, Copy)]
enum CoordinationMode {
    Wait,
    Try,
}

impl WriterLockCoordinator {
    /// Uses the same lock namespace as existing local thread-store writers.
    pub fn new(codex_home: &Path) -> Self {
        Self {
            directory: codex_home.join(WRITER_LOCK_DIR),
            cleanup_attempted: AtomicBool::new(false),
        }
    }

    /// Acquires exclusive writer ownership, returning `WouldBlock` for an active writer.
    pub fn acquire(self: &Arc<Self>, thread_id: ThreadId) -> io::Result<WriterLockGuard> {
        self.acquire_with_mode(thread_id, CoordinationMode::Wait)
    }

    /// Like `acquire`, but also skips busy home coordination instead of waiting for another client.
    pub fn try_acquire(self: &Arc<Self>, thread_id: ThreadId) -> io::Result<WriterLockGuard> {
        self.acquire_with_mode(thread_id, CoordinationMode::Try)
    }

    fn acquire_with_mode(
        self: &Arc<Self>,
        thread_id: ThreadId,
        mode: CoordinationMode,
    ) -> io::Result<WriterLockGuard> {
        let _coordination_lock = self.lock_coordination(mode)?;
        if !self.cleanup_attempted.swap(true, Ordering::Relaxed)
            && let Err(err) = self.remove_stale_thread_locks()
        {
            warn!("failed to clean up stale thread writer locks: {err}");
        }

        let path = self.directory.join(format!("{thread_id}.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| {
                io::Error::other(format!(
                    "failed to open thread writer lock {}: {err}",
                    path.display()
                ))
            })?;

        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("thread {thread_id} already has an active writer"),
                ));
            }
            Err(std::fs::TryLockError::Error(err)) => {
                return Err(io::Error::other(format!(
                    "failed to acquire thread writer lock {}: {err}",
                    path.display()
                )));
            }
        }

        Ok(WriterLockGuard {
            coordinator: Arc::clone(self),
            path,
            file: Some(file),
            coordination_mode: mode,
        })
    }

    /// Holds coordination through publication after probing that the thread is idle.
    /// Every writer takes coordination before opening its thread lock, so the probe
    /// itself can be released. Encoding and verification must finish before this call.
    pub(crate) fn try_acquire_for_publication(
        &self,
        thread_id: ThreadId,
    ) -> io::Result<Option<File>> {
        let coordination_lock = self.lock_coordination(CoordinationMode::Wait)?;
        let path = self.directory.join(format!("{thread_id}.lock"));
        match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => match file.try_lock() {
                Ok(()) => Ok(Some(coordination_lock)),
                Err(std::fs::TryLockError::WouldBlock) => Ok(None),
                Err(std::fs::TryLockError::Error(err)) => Err(err),
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Some(coordination_lock)),
            Err(err) => Err(err),
        }
    }

    fn lock_coordination(&self, mode: CoordinationMode) -> io::Result<File> {
        fs::create_dir_all(&self.directory)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.directory.join(COORDINATION_LOCK_FILE))?;
        match mode {
            CoordinationMode::Wait => file.lock()?,
            CoordinationMode::Try => file.try_lock()?,
        }
        Ok(file)
    }

    fn remove_stale_thread_locks(&self) -> io::Result<()> {
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(thread_id) = file_name.strip_suffix(".lock") else {
                continue;
            };
            if ThreadId::from_string(thread_id).is_err() {
                continue;
            }

            let path = entry.path();
            let file = match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(file) => file,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    warn!(
                        "failed to inspect thread writer lock {}: {err}",
                        path.display()
                    );
                    continue;
                }
            };
            match file.try_lock() {
                Ok(()) => {
                    drop(file);
                    if let Err(err) = fs::remove_file(&path)
                        && err.kind() != io::ErrorKind::NotFound
                    {
                        warn!(
                            "failed to remove stale thread writer lock {}: {err}",
                            path.display()
                        );
                    }
                }
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(err)) => {
                    warn!(
                        "failed to inspect thread writer lock {}: {err}",
                        path.display()
                    );
                }
            }
        }
        Ok(())
    }
}

impl Drop for WriterLockGuard {
    fn drop(&mut self) {
        let coordination_lock = match self.coordinator.lock_coordination(self.coordination_mode) {
            Ok(lock) => lock,
            Err(err) => {
                warn!("failed to coordinate thread writer lock cleanup: {err}");
                return;
            }
        };

        // Close the writer lock before deleting it so cleanup works on Windows too.
        drop(self.file.take());
        if let Err(err) = fs::remove_file(&self.path)
            && err.kind() != io::ErrorKind::NotFound
        {
            warn!(
                "failed to remove thread writer lock {}: {err}",
                self.path.display()
            );
        }
        drop(coordination_lock);
    }
}

#[cfg(test)]
#[path = "writer_lock_tests.rs"]
mod tests;
