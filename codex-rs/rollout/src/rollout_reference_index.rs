//! Indexes direct fork references found in local rollout files.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::RolloutId;
use codex_protocol::ThreadId;
use codex_protocol::protocol::HistoryPosition;

use crate::ARCHIVED_SESSIONS_SUBDIR;
use crate::SESSIONS_SUBDIR;
use crate::compression::RolloutFile;
use crate::rollout_file_name::RolloutFileName;

/// Direct history-base edges discovered from local rollout metadata.
///
/// This indexes immutable rollout IDs, not a thread's selected lineage. Callers use it to answer
/// cheap inverse-reference questions without each reimplementing rollout discovery.
#[derive(Debug, Default)]
pub struct RolloutReferenceIndex {
    rollouts_by_id: HashMap<RolloutId, IndexedRollout>,
    rollout_ids_by_thread: HashMap<ThreadId, Vec<RolloutId>>,
    reference_counts_by_rollout: HashMap<RolloutId, usize>,
}

#[derive(Debug)]
struct IndexedRollout {
    thread_id: ThreadId,
    paths: Vec<PathBuf>,
    history_base: Option<HistoryPosition>,
    parent_thread_id: Option<ThreadId>,
}

#[derive(Clone, Copy, PartialEq)]
enum ScanMode {
    BestEffort,
    Strict,
}

impl RolloutReferenceIndex {
    /// Scans active and archived local rollout metadata.
    pub async fn scan(codex_home: &Path) -> io::Result<Self> {
        Self::scan_paths(
            vec![
                codex_home.join(ARCHIVED_SESSIONS_SUBDIR),
                codex_home.join(SESSIONS_SUBDIR),
            ],
            /*thread_ids*/ None,
            ScanMode::BestEffort,
        )
        .await
    }

    /// Refuse automatic deletion when metadata or a history reference cannot be resolved.
    pub async fn scan_for_deletion(codex_home: &Path) -> io::Result<Self> {
        let index = Self::scan_paths(
            vec![
                codex_home.join(ARCHIVED_SESSIONS_SUBDIR),
                codex_home.join(SESSIONS_SUBDIR),
            ],
            /*thread_ids*/ None,
            ScanMode::Strict,
        )
        .await?;
        for &id in index.rollouts_by_id.keys() {
            let mut seen = HashSet::from([id]);
            let mut current = id;
            while let Some(base) = index.history_base(current) {
                if !index.rollouts_by_id.contains_key(&base.thread_id)
                    || !seen.insert(base.thread_id)
                {
                    return Err(io::Error::other(format!(
                        "unresolved or cyclic history reference from {id}"
                    )));
                }
                current = base.thread_id;
            }
        }
        Ok(index)
    }

    /// Scans only unarchived rollouts to locate files that still need to be archived.
    ///
    /// Reference counts exclude archived history and must not be used to decide whether a
    /// rollout can be deleted or compressed.
    pub async fn scan_unarchived(codex_home: &Path) -> io::Result<Self> {
        Self::scan_paths(
            vec![codex_home.join(SESSIONS_SUBDIR)],
            /*thread_ids*/ None,
            ScanMode::BestEffort,
        )
        .await
    }

    /// Scans unarchived files whose canonical filenames belong to the requested threads.
    ///
    /// Skips unrelated rollout contents, including compressed files. Metadata still determines
    /// ownership among the candidates. Reference counts are partial and must not be used to
    /// decide whether a rollout can be deleted or compressed.
    pub async fn scan_unarchived_threads(
        codex_home: &Path,
        thread_ids: &[ThreadId],
    ) -> io::Result<Self> {
        let thread_ids = thread_ids.iter().copied().collect();
        Self::scan_paths(
            vec![codex_home.join(SESSIONS_SUBDIR)],
            Some(&thread_ids),
            ScanMode::BestEffort,
        )
        .await
    }

    async fn scan_paths(
        mut stack: Vec<PathBuf>,
        thread_ids: Option<&HashSet<ThreadId>>,
        mode: ScanMode,
    ) -> io::Result<Self> {
        let mut rollouts_by_id = HashMap::new();
        while let Some(directory) = stack.pop() {
            let mut entries = match tokio::fs::read_dir(directory.as_path()).await {
                Ok(entries) => entries,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            loop {
                let Some(entry) = entries.next_entry().await? else {
                    break;
                };
                let path = entry.path();
                let file_type = entry.file_type().await?;
                if file_type.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !file_type.is_file() {
                    if mode == ScanMode::Strict {
                        return Err(io::Error::other(format!(
                            "unresolved rollout entry: {}",
                            path.display()
                        )));
                    }
                    continue;
                }
                let Some(rollout_file) = RolloutFile::from_path(path) else {
                    continue;
                };
                let Some(file_name) = RolloutFileName::parse(rollout_file.plain_file_name()) else {
                    if mode == ScanMode::Strict {
                        return Err(io::Error::other(format!(
                            "invalid rollout filename: {}",
                            rollout_file.path().display()
                        )));
                    }
                    continue;
                };
                if thread_ids.is_some_and(|ids| !ids.contains(&file_name.thread_id())) {
                    continue;
                }
                let rollout_id = file_name.rollout_id();
                let meta = match crate::read_session_meta_line(rollout_file.path()).await {
                    Ok(meta) => meta,
                    Err(error) if mode == ScanMode::Strict => {
                        return Err(io::Error::other(format!(
                            "incomplete metadata at {}: {error}",
                            rollout_file.path().display()
                        )));
                    }
                    Err(_) => continue,
                };
                let source_parent = meta.meta.source.parent_thread_id();
                if mode == ScanMode::Strict
                    && (file_name.thread_id() != meta.meta.id
                        || matches!((meta.meta.parent_thread_id, source_parent), (Some(a), Some(b)) if a != b))
                {
                    return Err(io::Error::other(format!(
                        "inconsistent ownership at {}",
                        rollout_file.path().display()
                    )));
                }
                let parent_thread_id = meta.meta.parent_thread_id.or(source_parent);
                match rollouts_by_id.entry(rollout_id) {
                    Entry::Vacant(entry) => {
                        entry.insert(IndexedRollout {
                            thread_id: meta.meta.id,
                            paths: vec![rollout_file.into_path()],
                            history_base: meta.meta.history_base,
                            parent_thread_id,
                        });
                    }
                    Entry::Occupied(mut entry) if mode == ScanMode::Strict => {
                        let previous = entry.get_mut();
                        if previous.thread_id != meta.meta.id
                            || previous.history_base != meta.meta.history_base
                            || previous.parent_thread_id != parent_thread_id
                        {
                            return Err(io::Error::other(format!(
                                "conflicting metadata for rollout {rollout_id}"
                            )));
                        }
                        previous.paths.push(rollout_file.into_path());
                    }
                    Entry::Occupied(_) => {}
                }
            }
        }

        let mut reference_counts_by_rollout = HashMap::new();
        let mut rollout_ids_by_thread: HashMap<ThreadId, Vec<RolloutId>> = HashMap::new();
        for (rollout_id, rollout) in &rollouts_by_id {
            rollout_ids_by_thread
                .entry(rollout.thread_id)
                .or_default()
                .push(*rollout_id);
            let Some(history_base) = rollout.history_base else {
                continue;
            };
            if history_base.thread_id == *rollout_id {
                continue;
            }
            *reference_counts_by_rollout
                .entry(history_base.thread_id)
                .or_default() += 1;
        }
        Ok(Self {
            rollouts_by_id,
            rollout_ids_by_thread,
            reference_counts_by_rollout,
        })
    }

    /// Returns how many other discovered rollouts directly reference `rollout_id`.
    pub fn reference_count(&self, rollout_id: RolloutId) -> usize {
        self.reference_counts_by_rollout
            .get(&rollout_id)
            .copied()
            .unwrap_or_default()
    }

    /// Returns the direct history-base edge for `rollout_id`, if one was discovered.
    pub fn history_base(&self, rollout_id: RolloutId) -> Option<&HistoryPosition> {
        self.rollouts_by_id
            .get(&rollout_id)
            .and_then(|rollout| rollout.history_base.as_ref())
    }

    /// Returns rollout IDs and paths whose session metadata belongs to `thread_id`.
    pub fn rollouts_for_thread(
        &self,
        thread_id: ThreadId,
    ) -> impl Iterator<Item = (RolloutId, &Path)> {
        self.rollout_ids_by_thread
            .get(&thread_id)
            .into_iter()
            .flatten()
            .flat_map(|rollout_id| {
                self.rollouts_by_id[rollout_id]
                    .paths
                    .iter()
                    .map(move |path| (*rollout_id, path.as_path()))
            })
    }

    /// Thread ownership and spawn parents recorded in rollout metadata, without listing repairs.
    pub fn thread_parents(&self) -> impl Iterator<Item = (ThreadId, Option<ThreadId>)> + '_ {
        self.rollouts_by_id
            .values()
            .map(|rollout| (rollout.thread_id, rollout.parent_thread_id))
    }
}

#[cfg(test)]
#[path = "rollout_reference_index_tests.rs"]
mod tests;
