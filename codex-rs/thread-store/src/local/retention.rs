//! Read-only retention planning; execution uses the ordinary coordinated deletion path.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_rollout::RolloutReferenceIndex;
use sqlx::Row;

use super::LocalThreadStoreConfig;
use super::delete_thread::ThreadRollouts;
use super::delete_thread::ensure_no_external_references;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

mod execute;

pub(super) use execute::RetentionCheck;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionConversation {
    pub id: ThreadId,
    pub updated_at: Option<DateTime<Utc>>,
    pub cwd: Option<String>,
    pub title: String,
    pub bytes: u64,
    // Fingerprints also prevent a later deletion from silently including newly published files.
    files: BTreeMap<PathBuf, (u64, SystemTime)>,
    database_children: HashSet<String>,
}

#[derive(Clone, Debug)]
pub enum RetentionSkip {
    Active,
    Referenced,
    Ineligible(String),
}

/// A complete, reviewed spawn subtree. Fields controlling deletion are private deliberately.
#[derive(Clone, Debug)]
pub struct RetentionGroup {
    pub conversations: Vec<RetentionConversation>,
    root: ThreadId,
    cutoff: DateTime<Utc>,
    delete_order: Vec<ThreadId>,
}

#[derive(Default, Debug)]
pub struct RetentionPlan {
    pub groups: Vec<RetentionGroup>,
    pub skipped: HashMap<ThreadId, RetentionSkip>,
}

impl RetentionPlan {
    /// The pool must be a read-only snapshot for preview. No ordinary listing/repair is used.
    pub async fn scan(
        config: &LocalThreadStoreConfig,
        pool: &sqlx::SqlitePool,
        cutoff: DateTime<Utc>,
    ) -> ThreadStoreResult<Self> {
        probe_idle(&config.codex_home.join(".tmp/rollout-maintenance.lock")).map_err(|error| {
            ineligible(format!(
                "rollout maintenance is busy or unavailable: {error}"
            ))
        })?;
        let mut connection = pool.acquire().await.map_err(ineligible)?;
        discover(config, &mut connection, cutoff)
            .await
            .map(|(plan, _)| plan)
    }
}

#[tracing::instrument(name = "retention_scan", skip_all)]
async fn discover(
    config: &LocalThreadStoreConfig,
    connection: &mut sqlx::SqliteConnection,
    cutoff: DateTime<Utc>,
) -> ThreadStoreResult<(RetentionPlan, RolloutReferenceIndex)> {
    probe_idle(
        &config
            .codex_home
            .join("thread-writer-locks/.coordination.lock"),
    )
    .map_err(ineligible)?;
    let rows =
        sqlx::query("SELECT id, updated_at_ms, cwd, title, rollout_path, source FROM threads")
            .fetch_all(&mut *connection)
            .await
            .map_err(ineligible)?;
    let edges: Vec<(String, String)> =
        sqlx::query_as("SELECT parent_thread_id, child_thread_id FROM thread_spawn_edges")
            .fetch_all(&mut *connection)
            .await
            .map_err(ineligible)?;
    let index = RolloutReferenceIndex::scan_for_deletion(&config.codex_home)
        .await
        .map_err(ineligible)?;
    let mut parents = HashMap::new();
    let mut database_children: HashMap<ThreadId, HashSet<String>> = HashMap::new();
    for (parent, child) in edges {
        let parent = ThreadId::from_string(&parent).map_err(ineligible)?;
        parents.insert(ThreadId::from_string(&child).map_err(ineligible)?, parent);
        database_children.entry(parent).or_default().insert(child);
    }
    for (child, parent) in index.thread_parents() {
        if let Some(parent) = parent
            && parents
                .insert(child, parent)
                .is_some_and(|previous| previous != parent)
        {
            return Err(ineligible(format!(
                "inconsistent spawn parents for {child}"
            )));
        }
    }
    let mut threads = HashMap::new();
    let mut skipped = HashMap::new();
    for row in rows {
        let id = ThreadId::from_string(&row.try_get::<String, _>("id").map_err(ineligible)?)
            .map_err(ineligible)?;
        let source: String = row.try_get("source").map_err(ineligible)?;
        let source = serde_json::from_str::<SessionSource>(&source)
            .or_else(|_| serde_json::from_value(serde_json::Value::String(source)));
        if let Some(parent) = source.ok().and_then(|source| source.parent_thread_id())
            && parents
                .insert(id, parent)
                .is_some_and(|previous| previous != parent)
        {
            return Err(ineligible(format!("inconsistent spawn parents for {id}")));
        }
        let updated_at = row
            .try_get::<Option<i64>, _>("updated_at_ms")
            .ok()
            .flatten()
            .and_then(DateTime::from_timestamp_millis);
        let mut thread = RetentionConversation {
            id,
            updated_at,
            cwd: row
                .try_get::<Option<String>, _>("cwd")
                .ok()
                .flatten()
                .filter(|cwd| !cwd.is_empty()),
            title: row.try_get::<String, _>("title").unwrap_or_default(),
            bytes: 0,
            files: BTreeMap::new(),
            database_children: database_children.remove(&id).unwrap_or_default(),
        };
        let recorded: String = row.try_get("rollout_path").map_err(ineligible)?;
        let recorded = codex_rollout::existing_rollout_path(Path::new(&recorded))
            .await
            .and_then(|path| codex_utils_path::normalize_for_path_comparison(path).ok());
        let mut found_recorded = false;
        let mut allocations = HashMap::new();
        for (_, path) in index.rollouts_for_thread(id) {
            found_recorded |= recorded.as_ref()
                == Some(
                    &codex_utils_path::normalize_for_path_comparison(path).map_err(ineligible)?,
                );
            let metadata = std::fs::symlink_metadata(path).map_err(ineligible)?;
            if !metadata.is_file() {
                return Err(ineligible(format!(
                    "not a regular rollout: {}",
                    path.display()
                )));
            }
            if thread
                .files
                .insert(
                    path.to_path_buf(),
                    (metadata.len(), metadata.modified().map_err(ineligible)?),
                )
                .is_none()
            {
                let storage = file_storage(path, &metadata).map_err(ineligible)?;
                let allocation =
                    allocations
                        .entry(storage.id)
                        .or_insert((storage.links, storage.bytes, 0));
                allocation.2 += 1;
            }
        }
        {
            thread.bytes = allocations
                .values()
                .filter(|(links, _, selected)| links == selected)
                .map(|(_, bytes, _)| bytes)
                .sum();
        }
        if thread.files.is_empty() {
            return Err(ineligible(format!(
                "missing rollout files for {id}; retained history references cannot be resolved"
            )));
        }
        let writer_error = probe_idle(
            &config
                .codex_home
                .join("thread-writer-locks")
                .join(format!("{id}.lock")),
        )
        .err();
        let reason = if let Some(error) = writer_error {
            Some(if error.kind() == std::io::ErrorKind::WouldBlock {
                RetentionSkip::Active
            } else {
                RetentionSkip::Ineligible(format!("cannot inspect writer lock: {error}"))
            })
        } else if !found_recorded {
            Some(RetentionSkip::Ineligible(
                "incomplete rollout metadata or missing recorded file".into(),
            ))
        } else if updated_at.is_none_or(|updated| updated >= cutoff) {
            Some(RetentionSkip::Ineligible(
                "unknown last update or last update is not before cutoff".into(),
            ))
        } else {
            None
        };
        if let Some(reason) = reason {
            skipped.insert(id, reason);
        }
        threads.insert(id, thread);
    }
    for (id, _) in index.thread_parents() {
        if !threads.contains_key(&id) {
            skipped.insert(
                id,
                RetentionSkip::Ineligible(
                    "missing database metadata; last update is unknown".into(),
                ),
            );
        }
    }
    let mut children: HashMap<ThreadId, Vec<ThreadId>> = HashMap::new();
    for (child, parent) in parents {
        children.entry(parent).or_default().push(child);
        if !threads.contains_key(&parent) || !threads.contains_key(&child) {
            skipped.insert(
                child,
                RetentionSkip::Ineligible("unresolved spawn parent or child metadata".into()),
            );
        }
    }
    // ponytail: traverse each in-memory subtree; cache closures if very deep agent trees become common.
    let mut subtrees = threads
        .keys()
        .map(|&root| {
            let mut ids = Vec::new();
            let mut stack = vec![root];
            let mut seen = HashSet::new();
            while let Some(id) = stack.pop() {
                if !seen.insert(id) {
                    skipped.insert(root, RetentionSkip::Ineligible("cyclic spawn graph".into()));
                    break;
                }
                ids.push(id);
                stack.extend(children.get(&id).into_iter().flatten());
            }
            (root, ids)
        })
        .collect::<Vec<_>>();
    subtrees.sort_by_key(|(root, ids)| (std::cmp::Reverse(ids.len()), root.to_string()));
    let mut selected = HashSet::new();
    let mut groups = Vec::new();
    for (root, ids) in subtrees {
        if selected.contains(&root) || skipped.contains_key(&root) {
            continue;
        }
        if ids
            .iter()
            .any(|id| skipped.contains_key(id) || !threads.contains_key(id))
        {
            skipped.insert(
                root,
                RetentionSkip::Ineligible(
                    "descendant is active, newer, or has incomplete metadata".into(),
                ),
            );
            continue;
        }
        let rollouts = ids
            .iter()
            .map(|&id| ThreadRollouts::from_index(&index, id))
            .collect::<Vec<_>>();
        if ensure_no_external_references(&index, &rollouts).is_err() {
            skipped.insert(root, RetentionSkip::Referenced);
            continue;
        }
        let mut conversations = ids
            .iter()
            .filter_map(|id| threads.get(id).cloned())
            .collect::<Vec<_>>();
        conversations.sort_by_key(|thread| thread.id.to_string());
        let delete_order = ids.iter().rev().copied().collect();
        selected.extend(ids);
        groups.push(RetentionGroup {
            conversations,
            root,
            cutoff,
            delete_order,
        });
    }
    Ok((RetentionPlan { groups, skipped }, index))
}

/// Hard links may have retained names outside the deletion set, so estimate no freed blocks.
pub fn removable_file_bytes(path: &Path, metadata: &std::fs::Metadata) -> std::io::Result<u64> {
    let storage = file_storage(path, metadata)?;
    Ok(if storage.links > 1 { 0 } else { storage.bytes })
}

struct FileStorage {
    id: (u64, u128),
    links: u64,
    bytes: u64,
}

#[cfg(unix)]
fn file_storage(_path: &Path, metadata: &std::fs::Metadata) -> std::io::Result<FileStorage> {
    use std::os::unix::fs::MetadataExt;
    Ok(FileStorage {
        id: (metadata.dev(), metadata.ino().into()),
        links: metadata.nlink(),
        bytes: metadata.blocks().saturating_mul(/*rhs*/ 512),
    })
}

#[cfg(windows)]
fn file_storage(path: &Path, _metadata: &std::fs::Metadata) -> std::io::Result<FileStorage> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
    use windows_sys::Win32::Storage::FileSystem::FILE_STANDARD_INFO;
    use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
    use windows_sys::Win32::Storage::FileSystem::FileStandardInfo;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
    let file = File::open(path)?;
    let mut id = std::mem::MaybeUninit::<FILE_ID_INFO>::uninit();
    let mut info = std::mem::MaybeUninit::<FILE_STANDARD_INFO>::uninit();
    // SAFETY: both buffers match their information classes and stay alive for these synchronous
    // calls. They are read only after Windows reports successful initialization.
    unsafe {
        if GetFileInformationByHandleEx(
            file.as_raw_handle() as _,
            FileIdInfo,
            id.as_mut_ptr().cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        ) == 0
            || GetFileInformationByHandleEx(
                file.as_raw_handle() as _,
                FileStandardInfo,
                info.as_mut_ptr().cast(),
                std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
            ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let (id, info) = (id.assume_init(), info.assume_init());
        Ok(FileStorage {
            id: (
                id.VolumeSerialNumber,
                u128::from_ne_bytes(id.FileId.Identifier),
            ),
            links: info.NumberOfLinks.into(),
            bytes: u64::try_from(info.AllocationSize).map_err(std::io::Error::other)?,
        })
    }
}

#[cfg(not(any(unix, windows)))]
fn file_storage(_path: &Path, _metadata: &std::fs::Metadata) -> std::io::Result<FileStorage> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "file allocation and identity cannot be inspected on this platform",
    ))
}

fn probe_idle(path: &Path) -> std::io::Result<()> {
    match File::open(path) {
        Ok(file) => file.try_lock_shared().map_err(Into::into),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn ineligible(error: impl std::fmt::Display) -> ThreadStoreError {
    ThreadStoreError::InvalidRequest {
        message: format!("retention skipped: {error}"),
    }
}

pub(super) fn conflict(message: &str) -> ThreadStoreError {
    ThreadStoreError::Conflict {
        message: message.into(),
    }
}

#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;
