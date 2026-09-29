//! Open the existing coordinated deletion machinery without startup migrations or log pruning.

use super::*;

impl StateRuntime {
    /// Retain exclusive catalog ownership across a batch of coordinated deletions, including
    /// between commits. Holding the connection also prevents pool recycling from releasing it.
    pub async fn retention_connection(&self) -> anyhow::Result<sqlx::pool::PoolConnection<Sqlite>> {
        let mut connection = self.pool.acquire().await?;
        let mode: String = sqlx::query_scalar("PRAGMA locking_mode")
            .fetch_one(&mut *connection)
            .await?;
        anyhow::ensure!(
            mode == "exclusive",
            "retention requires exclusive state database access"
        );
        // Acquire the actual lock before the caller builds an inventory.
        sqlx::query("SELECT id FROM threads LIMIT 1")
            .execute(&mut *connection)
            .await?;
        Ok(connection)
    }

    pub async fn open_existing_for_retention(
        sqlite: SqliteConfig,
        default_provider: String,
    ) -> anyhow::Result<Arc<Self>> {
        // A different client's prepared fork may not have published its history reference yet.
        // Exclusive access excludes those clients as well as ordinary metadata writers.
        let pool = Arc::new(
            sqlite
                .open_existing_pool_with_mode(
                    &sqlite.state_db_path(),
                    sqlx::sqlite::SqliteLockingMode::Exclusive,
                )
                .await?,
        );
        let logs_pool = Arc::new(sqlite.open_existing_pool(&sqlite.logs_db_path()).await?);
        let goals = Arc::new(sqlite.open_existing_pool(&sqlite.goals_db_path()).await?);
        let memories = Arc::new(
            sqlite
                .open_existing_pool(&sqlite.memories_db_path())
                .await?,
        );
        let queue = Arc::new(sqlite.open_existing_pool(&sqlite.queue_db_path()).await?);
        let memories_v2 = if tokio::fs::try_exists(sqlite.memories_v2_db_path()).await? {
            Some(MemoryStore::new(
                Arc::new(
                    sqlite
                        .open_existing_pool(&sqlite.memories_v2_db_path())
                        .await?,
                ),
                pool.clone(),
            ))
        } else {
            None
        };
        Ok(Arc::new(Self {
            sqlite,
            default_provider,
            thread_goals: GoalStore::new(goals),
            memories: MemoryStore::new(memories, pool.clone()),
            memories_v2: Arc::new(tokio::sync::OnceCell::new_with(memories_v2)),
            thread_queue: SqliteQueueStore::new(queue),
            pool,
            logs_pool,
            thread_updated_at_millis: Arc::new(AtomicI64::new(0)),
            thread_recency_at_millis: Arc::new(AtomicI64::new(0)),
            reclamation: None,
        }))
    }
}
