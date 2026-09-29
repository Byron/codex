use super::super::LocalThreadStore;
use super::super::test_support::test_config;
use super::super::test_support::write_archived_session_file;
use super::super::test_support::write_session_file;
use super::*;
use codex_rollout::WriterLockCoordinator;
use codex_state::StateRuntime;
use pretty_assertions::assert_eq;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::TempDir;
use uuid::Uuid;

async fn seed(
    executor: impl sqlx::Executor<'_, Database = sqlx::Sqlite>,
    home: &Path,
    number: u128,
    updated: i64,
    archived: bool,
) -> (ThreadId, PathBuf) {
    let uuid = Uuid::from_u128(number);
    let id = ThreadId::from_string(&uuid.to_string()).unwrap();
    let path = if archived {
        write_archived_session_file(home, "2025-01-03T12-00-00", uuid).unwrap()
    } else {
        write_session_file(home, "2025-01-03T12-00-00", uuid).unwrap()
    };
    sqlx::query("INSERT INTO threads (id, rollout_path, created_at, updated_at, updated_at_ms, source, model_provider, cwd, title, sandbox_policy, approval_mode, archived) VALUES (?, ?, 1, 1, ?, 'exec', 'test-provider', '/project', 'title', 'read-only', 'on-request', ?)")
        .bind(id.to_string()).bind(path.to_str().unwrap()).bind(updated).bind(archived).execute(executor).await.unwrap();
    (id, path)
}

async fn edge(
    executor: impl sqlx::Executor<'_, Database = sqlx::Sqlite>,
    parent: ThreadId,
    child: ThreadId,
) {
    sqlx::query("INSERT INTO thread_spawn_edges (parent_thread_id, child_thread_id, status) VALUES (?, ?, 'closed')")
        .bind(parent.to_string()).bind(child.to_string()).execute(executor).await.unwrap();
}

async fn delete_group(store: &LocalThreadStore, group: &RetentionGroup) -> ThreadStoreResult<()> {
    let mut result = Ok(());
    store
        .delete_retention_groups(std::slice::from_ref(group), |_, outcome| result = outcome)
        .await?;
    result
}

struct ScanCounter(Arc<AtomicUsize>);

impl tracing::Subscriber for ScanCounter {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.name() == "retention_scan"
    }

    fn new_span(&self, _attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64((self.0.fetch_add(/*val*/ 1, Ordering::Relaxed) + 1) as u64)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, _event: &tracing::Event<'_>) {}
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

#[tokio::test]
async fn retention_batch_scans_once_and_rechecks_each_group_before_deletion() {
    let home = TempDir::new().unwrap();
    let config = test_config(home.path());
    let state = StateRuntime::init(config.sqlite.clone(), "test-provider".into())
        .await
        .unwrap();
    let pool = config
        .sqlite
        .open_existing_pool(&config.sqlite.state_db_path())
        .await
        .unwrap();
    let mut fixtures = Vec::new();
    for number in 1..=64 {
        fixtures.push(
            seed(
                &pool,
                home.path(),
                number,
                /*updated*/ 999,
                number % 2 == 0,
            )
            .await,
        );
    }
    let cutoff = DateTime::from_timestamp_millis(/*millis*/ 1000).unwrap();
    let plan = RetentionPlan::scan(&config, &pool, cutoff).await.unwrap();
    // An invocation must not reuse an inventory without exclusive database ownership.
    let ordinary = LocalThreadStore::new(config.clone(), Some(state.clone()));
    assert!(
        ordinary
            .delete_retention_groups(&plan.groups, |_, _| panic!(
                "no deletion without exclusive access"
            ))
            .await
            .unwrap_err()
            .to_string()
            .contains("exclusive state database access")
    );

    // A newly retained fork now needs a formerly eligible conversation's history.
    let mut metadata: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(&fixtures[61].1)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    metadata["payload"]["history_base"] = serde_json::json!({"thread_id": fixtures[60].0, "end_ordinal_exclusive": 1, "end_byte_offset": 1});
    std::fs::write(&fixtures[61].1, format!("{metadata}\n")).unwrap();
    sqlx::query("UPDATE threads SET updated_at_ms=1000 WHERE id=?")
        .bind(fixtures[61].0.to_string())
        .execute(&pool)
        .await
        .unwrap();
    // Simulate changes after the batch inventory was built, on the locked connection itself.
    // Both a newer timestamp and a newly attached descendant must be caught per group.
    let (first, redated, parent, child) = (
        fixtures[0].0,
        fixtures[58].0,
        fixtures[59].0,
        fixtures[63].0,
    );
    // SQLite triggers cannot bind parameters; these interpolated ThreadIds are fixture UUIDs.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER change_remaining_candidates AFTER DELETE ON threads WHEN OLD.id='{first}' BEGIN
         UPDATE threads SET updated_at_ms=1000 WHERE id='{redated}';
         INSERT INTO thread_spawn_edges (parent_thread_id, child_thread_id, status) VALUES ('{parent}', '{child}', 'closed');
         END"
    ))).execute(&pool).await.unwrap();
    pool.close().await;
    state.close().await;
    let state =
        StateRuntime::open_existing_for_retention(config.sqlite.clone(), "test-provider".into())
            .await
            .unwrap();
    let store = LocalThreadStore::new(config.clone(), Some(state.clone()));
    let locks = Arc::new(WriterLockCoordinator::new(home.path()));
    let mut active = None;
    let mut outcomes = Vec::new();
    let scans = Arc::new(AtomicUsize::new(/*v*/ 0));
    let subscriber = tracing::subscriber::set_default(ScanCounter(scans.clone()));
    store
        .delete_retention_groups(&plan.groups, |count, result| {
            // Compression/migration must remain excluded even between committed groups.
            assert!(
                codex_rollout::try_acquire_rollout_maintenance_lock(home.path())
                    .unwrap()
                    .is_none()
            );
            if outcomes.is_empty() {
                active = Some(locks.acquire(fixtures[63].0).unwrap());
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&fixtures[62].1)
                    .unwrap()
                    .write_all(b"\n")
                    .unwrap();
            }
            let deleted = match result {
                Ok(()) => true,
                Err(
                    ThreadStoreError::Conflict { .. } | ThreadStoreError::InvalidRequest { .. },
                ) => false,
                Err(error) => panic!("unexpected cleanup failure: {error}"),
            };
            outcomes.push((count, deleted));
        })
        .await
        .unwrap();
    drop(subscriber);
    assert_eq!(scans.load(Ordering::Relaxed), 1);
    assert_eq!(
        outcomes,
        (0..64).map(|index| (1, index < 58)).collect::<Vec<_>>()
    );
    let mut connection = state.retention_connection().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT id FROM threads ORDER BY id")
            .fetch_all(&mut *connection)
            .await
            .unwrap(),
        fixtures[58..]
            .iter()
            .map(|(id, _)| id.to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        fixtures
            .iter()
            .map(|(_, path)| path.exists())
            .collect::<Vec<_>>(),
        (0..64).map(|index| index >= 58).collect::<Vec<_>>()
    );
    drop(active);
    drop(connection);
    state.close().await;
}

#[tokio::test]
async fn retention_accounts_for_archives_active_references_and_descendants() {
    let home = TempDir::new().unwrap();
    let config = test_config(home.path());
    let state = StateRuntime::init(config.sqlite.clone(), "test-provider".into())
        .await
        .unwrap();
    let pool = config
        .sqlite
        .open_existing_pool(&config.sqlite.state_db_path())
        .await
        .unwrap();
    let mut fixtures = Vec::new();
    for number in 1..=11 {
        fixtures.push(
            seed(
                &pool,
                home.path(),
                number,
                if matches!(number, 4 | 8) { 1000 } else { 999 },
                number == 5,
            )
            .await,
        );
    }
    // Discover parents from both source metadata and the explicit rollout parent, even if
    // backfill has not populated the spawn-edge table yet.
    sqlx::query("UPDATE threads SET source=? WHERE id=?")
        .bind(serde_json::json!({"subagent":{"thread_spawn":{"parent_thread_id":fixtures[0].0,"depth":1}}}).to_string())
        .bind(fixtures[1].0.to_string()).execute(&pool).await.unwrap();
    let mut child_meta: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(&fixtures[3].1)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    child_meta["payload"]["parent_thread_id"] = serde_json::json!(fixtures[2].0);
    std::fs::write(&fixtures[3].1, format!("{child_meta}\n")).unwrap();
    edge(&pool, fixtures[10].0, fixtures[5].0).await;
    let locks = Arc::new(WriterLockCoordinator::new(home.path()));
    let _active = locks.acquire(fixtures[5].0).unwrap();
    let mut meta: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(&fixtures[7].1)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    meta["payload"]["history_base"] = serde_json::json!({"thread_id": fixtures[6].0, "end_ordinal_exclusive": 1, "end_byte_offset": 1});
    std::fs::write(&fixtures[7].1, format!("{meta}\n")).unwrap();
    sqlx::query("UPDATE threads SET updated_at_ms=NULL WHERE id=?")
        .bind(fixtures[8].0.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE threads SET cwd='' WHERE id=?")
        .bind(fixtures[9].0.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let plan = RetentionPlan::scan(
        &config,
        &pool,
        DateTime::from_timestamp_millis(/*millis*/ 1000).unwrap(),
    )
    .await
    .unwrap();
    let selected = plan
        .groups
        .iter()
        .flat_map(|group| group.conversations.iter().map(|thread| thread.id))
        .collect::<HashSet<_>>();
    assert_eq!(
        selected,
        [0, 1, 4, 9]
            .map(|index| fixtures[index].0)
            .into_iter()
            .collect()
    );
    assert_eq!(
        plan.groups
            .iter()
            .flat_map(|group| &group.conversations)
            .map(|thread| thread.bytes)
            .sum::<u64>(),
        [0, 1, 4, 9]
            .iter()
            .map(|&index| removable_file_bytes(
                &fixtures[index].1,
                &std::fs::metadata(&fixtures[index].1).unwrap()
            )
            .unwrap())
            .sum::<u64>()
    );
    assert!(matches!(
        plan.skipped.get(&fixtures[5].0),
        Some(RetentionSkip::Active)
    ));
    assert!(matches!(
        plan.skipped.get(&fixtures[6].0),
        Some(RetentionSkip::Referenced)
    ));
    assert_eq!(plan.skipped.len(), 7);
    #[cfg(any(unix, windows))]
    {
        let original_bytes = plan
            .groups
            .iter()
            .flat_map(|group| &group.conversations)
            .find(|thread| thread.id == fixtures[4].0)
            .unwrap()
            .bytes;
        let alias = fixtures[0]
            .1
            .parent()
            .unwrap()
            .join(fixtures[4].1.file_name().unwrap());
        std::fs::hard_link(&fixtures[4].1, &alias).unwrap();
        let plan = RetentionPlan::scan(
            &config,
            &pool,
            DateTime::from_timestamp_millis(/*millis*/ 1000).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            plan.groups
                .iter()
                .flat_map(|group| &group.conversations)
                .find(|thread| thread.id == fixtures[4].0)
                .unwrap()
                .bytes,
            original_bytes
        );
        std::fs::hard_link(&alias, home.path().join("retained-user-artifact")).unwrap();
        let plan = RetentionPlan::scan(
            &config,
            &pool,
            DateTime::from_timestamp_millis(/*millis*/ 1000).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            plan.groups
                .iter()
                .flat_map(|group| &group.conversations)
                .find(|thread| thread.id == fixtures[4].0)
                .unwrap()
                .bytes,
            0
        );
    }
    pool.close().await;
    state.close().await;
}

#[tokio::test]
async fn execution_rechecks_age_and_never_expands_the_planned_subtree() {
    let home = TempDir::new().unwrap();
    let config = test_config(home.path());
    let state = StateRuntime::init(config.sqlite.clone(), "test-provider".into())
        .await
        .unwrap();
    let pool = config
        .sqlite
        .open_existing_pool(&config.sqlite.state_db_path())
        .await
        .unwrap();
    let parent = seed(
        &pool,
        home.path(),
        /*number*/ 1,
        /*updated*/ 999,
        /*archived*/ false,
    )
    .await;
    let child = seed(
        &pool,
        home.path(),
        /*number*/ 2,
        /*updated*/ 999,
        /*archived*/ true,
    )
    .await;
    edge(&pool, parent.0, child.0).await;
    let cutoff = DateTime::from_timestamp_millis(/*millis*/ 1000).unwrap();
    let group = RetentionPlan::scan(&config, &pool, cutoff)
        .await
        .unwrap()
        .groups
        .remove(/*index*/ 0);
    pool.close().await;
    state.close().await;
    let state =
        StateRuntime::open_existing_for_retention(config.sqlite.clone(), "test-provider".into())
            .await
            .unwrap();
    let inspection_sqlite = config.sqlite.clone();
    let store = LocalThreadStore::new(config.clone(), Some(state.clone()))
        .with_thread_data_cleanup(move |_| {
            let sqlite = inspection_sqlite.clone();
            Box::pin(async move {
                let result = async {
                    let pool = sqlite.open_existing_pool(&sqlite.state_db_path()).await?;
                    let result = sqlx::query("UPDATE threads SET updated_at_ms=2000")
                        .execute(&pool)
                        .await;
                    pool.close().await;
                    result
                }
                .await;
                let error = result.expect_err("eligibility stays locked until deletion finishes");
                assert!(codex_state::sqlite_error_detail_is_lock(&error.to_string()));
                Ok(())
            })
        });
    let maintenance = codex_rollout::try_acquire_rollout_maintenance_lock(home.path())
        .unwrap()
        .unwrap();
    assert!(
        delete_group(&store, &group)
            .await
            .unwrap_err()
            .to_string()
            .contains("rollout maintenance")
    );
    drop(maintenance);
    let locks = Arc::new(WriterLockCoordinator::new(home.path()));
    let active = locks.acquire(parent.0).unwrap();
    assert!(matches!(
        delete_group(&store, &group).await,
        Err(ThreadStoreError::Conflict { .. })
    ));
    drop(active);
    let lifecycle = store.live_writer_locks.reserve_lifecycle(parent.0).await;
    assert!(matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(/*secs*/ 2),
            delete_group(&store, &group)
        )
        .await
        .unwrap(),
        Err(ThreadStoreError::Conflict { .. })
    ));
    drop(lifecycle);
    let history = codex_state::open_thread_history_db(&config.sqlite)
        .await
        .unwrap();
    let history_lock = history.begin_with("BEGIN IMMEDIATE").await.unwrap();
    assert!(matches!(
        delete_group(&store, &group).await,
        Err(ThreadStoreError::Conflict { .. })
    ));
    history_lock.rollback().await.unwrap();
    history.close().await;
    let mut connection = state.retention_connection().await.unwrap();
    sqlx::query("UPDATE threads SET updated_at_ms=1000 WHERE id=?")
        .bind(child.0.to_string())
        .execute(&mut *connection)
        .await
        .unwrap();
    drop(connection);
    assert!(matches!(
        delete_group(&store, &group).await,
        Err(ThreadStoreError::Conflict { .. })
    ));
    assert!(parent.1.exists() && child.1.exists());
    let mut connection = state.retention_connection().await.unwrap();
    sqlx::query("UPDATE threads SET updated_at_ms=999 WHERE id=?")
        .bind(child.0.to_string())
        .execute(&mut *connection)
        .await
        .unwrap();
    let added = seed(
        &mut *connection,
        home.path(),
        /*number*/ 3,
        /*updated*/ 999,
        /*archived*/ false,
    )
    .await;
    edge(&mut *connection, parent.0, added.0).await;
    drop(connection);
    assert!(matches!(
        delete_group(&store, &group).await,
        Err(ThreadStoreError::Conflict { .. })
    ));
    assert!(parent.1.exists() && child.1.exists() && added.1.exists());
    let mut connection = state.retention_connection().await.unwrap();
    let group = discover(&config, &mut connection, cutoff)
        .await
        .unwrap()
        .0
        .groups
        .remove(/*index*/ 0);
    drop(connection);
    assert_eq!(group.conversations.len(), 3);
    delete_group(&store, &group).await.unwrap();
    let mut connection = state.retention_connection().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM threads")
            .fetch_one(&mut *connection)
            .await
            .unwrap(),
        0
    );
    assert!(!parent.1.exists() && !child.1.exists() && !added.1.exists());
    drop(connection);
    state.close().await;
}

#[tokio::test]
async fn incomplete_reference_metadata_prevents_automatic_deletion() {
    let home = TempDir::new().unwrap();
    let config = test_config(home.path());
    let state = StateRuntime::init(config.sqlite.clone(), "test-provider".into())
        .await
        .unwrap();
    let pool = config
        .sqlite
        .open_existing_pool(&config.sqlite.state_db_path())
        .await
        .unwrap();
    let old = seed(
        &pool,
        home.path(),
        /*number*/ 1,
        /*updated*/ 999,
        /*archived*/ false,
    )
    .await;
    let unknown = seed(
        &pool,
        home.path(),
        /*number*/ 2,
        /*updated*/ 999,
        /*archived*/ true,
    )
    .await;
    let contents = std::fs::read_to_string(&unknown.1).unwrap();
    let mut metadata: serde_json::Value =
        serde_json::from_str(contents.lines().next().unwrap()).unwrap();
    metadata["payload"]["id"] = serde_json::json!(old.0);
    std::fs::write(&unknown.1, format!("{metadata}\n")).unwrap();
    assert!(
        RetentionPlan::scan(
            &config,
            &pool,
            DateTime::from_timestamp_millis(/*millis*/ 1000).unwrap()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("inconsistent ownership")
    );
    std::fs::write(&unknown.1, "incomplete metadata\n").unwrap();
    let error = RetentionPlan::scan(
        &config,
        &pool,
        DateTime::from_timestamp_millis(/*millis*/ 1000).unwrap(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("incomplete metadata"));
    std::fs::remove_file(&unknown.1).unwrap();
    assert!(
        RetentionPlan::scan(
            &config,
            &pool,
            DateTime::from_timestamp_millis(/*millis*/ 1000).unwrap()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("retained history references cannot be resolved")
    );
    assert!(old.1.exists());
    pool.close().await;
    state.close().await;
}
