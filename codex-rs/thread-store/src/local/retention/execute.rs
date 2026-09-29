//! Reuse a locked inventory for retention while keeping coordinated cleanup per subtree.

use std::collections::HashMap;
use std::collections::HashSet;

use chrono::DateTime;
use codex_rollout::RolloutReferenceIndex;

use super::RetentionGroup;
use super::conflict;
use super::discover;
use super::ineligible;
use crate::DeleteThreadsParams;
use crate::ThreadStoreResult;
use crate::local::LocalThreadStore;
use crate::local::delete_thread::delete_threads;

pub(in crate::local) struct RetentionCheck<'a> {
    pub group: &'a RetentionGroup,
    pub current: &'a RetentionGroup,
    pub index: &'a RolloutReferenceIndex,
    pub connection: &'a mut sqlx::SqliteConnection,
}

impl RetentionGroup {
    pub(in crate::local) async fn validate(
        &self,
        current: &Self,
        connection: &mut sqlx::SqliteConnection,
    ) -> ThreadStoreResult<()> {
        if current.root != self.root || current.conversations != self.conversations {
            return Err(conflict(
                "eligibility or subtree membership changed; rerun gc to preview again",
            ));
        }
        for thread in &self.conversations {
            let updated: Option<i64> =
                sqlx::query_scalar("SELECT updated_at_ms FROM threads WHERE id = ?")
                    .bind(thread.id.to_string())
                    .fetch_optional(&mut *connection)
                    .await
                    .map_err(ineligible)?
                    .flatten();
            if updated.and_then(DateTime::from_timestamp_millis) != thread.updated_at {
                return Err(conflict(
                    "conversation last update changed; rerun gc to preview again",
                ));
            }
            let children: Vec<String> = sqlx::query_scalar(
                "SELECT child_thread_id FROM thread_spawn_edges WHERE parent_thread_id = ?",
            )
            .bind(thread.id.to_string())
            .fetch_all(&mut *connection)
            .await
            .map_err(ineligible)?;
            if children.into_iter().collect::<HashSet<_>>() != thread.database_children {
                return Err(conflict(
                    "subtree membership changed; rerun gc to preview again",
                ));
            }
            for (path, fingerprint) in &thread.files {
                let metadata = std::fs::symlink_metadata(path).map_err(ineligible)?;
                if !metadata.is_file()
                    || (metadata.len(), metadata.modified().map_err(ineligible)?) != *fingerprint
                {
                    return Err(conflict(
                        "conversation file changed; rerun gc to preview again",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl LocalThreadStore {
    /// Delete inspected subtrees through coordinated cleanup, reporting each group's outcome.
    /// Errors returned directly occur before any group is processed. Retaining the exclusive
    /// database connection and maintenance lock makes reference discovery reusable; writer,
    /// age, membership and file checks still precede every deletion under its locks.
    pub async fn delete_retention_groups(
        &self,
        groups: &[RetentionGroup],
        mut on_group: impl FnMut(usize, ThreadStoreResult<()>),
    ) -> ThreadStoreResult<()> {
        let Some(first) = groups.first() else {
            return Ok(());
        };
        if groups.iter().any(|group| group.cutoff != first.cutoff) {
            return Err(ineligible("retention cutoffs must match"));
        }
        let _maintenance =
            codex_rollout::try_acquire_rollout_maintenance_lock(&self.config.codex_home)
                .map_err(ineligible)?
                .ok_or_else(|| conflict("rollout maintenance is busy"))?;
        let state = self
            .state_db
            .as_ref()
            .ok_or_else(|| conflict("state database unavailable"))?;
        let mut connection = state.retention_connection().await.map_err(ineligible)?;
        let (current, index) = discover(&self.config, &mut connection, first.cutoff).await?;
        let current = current
            .groups
            .into_iter()
            .map(|group| (group.root, group))
            .collect::<HashMap<_, _>>();
        for group in groups {
            let result = if let Some(current) = current.get(&group.root) {
                delete_threads(
                    self,
                    DeleteThreadsParams {
                        thread_ids: group.delete_order.clone(),
                    },
                    Some(RetentionCheck {
                        group,
                        current,
                        index: &index,
                        connection: &mut connection,
                    }),
                )
                .await
            } else {
                Err(conflict(
                    "eligibility or subtree membership changed; rerun gc to preview again",
                ))
            };
            on_group(group.conversations.len(), result);
        }
        Ok(())
    }
}
