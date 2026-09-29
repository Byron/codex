//! Local, data-preserving maintenance and explicitly authorized conversation retention.

use std::collections::BTreeSet;
use std::fmt::Write;

use anyhow::Context;
use chrono::DateTime;
use chrono::TimeDelta;
use chrono::Utc;
use clap::Parser;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_core::config::LoaderOverrides;
use codex_features::Feature;
use codex_state::ReclamationOutcome;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::LocalThreadStoreConfig;
use codex_thread_store::RetentionPlan;
use codex_thread_store::RetentionSkip;
use codex_thread_store::ThreadStoreError;
use codex_thread_store::removable_file_bytes;
use codex_utils_cli::CliConfigOverrides;

#[derive(Debug, Parser)]
pub(crate) struct GcCommand {
    /// Preview maintenance and conversations last updated before this age (e.g. 30d or 4w).
    #[arg(long, value_name = "AGE", value_parser = parse_age)]
    older_than: Option<TimeDelta>,
    /// Perform the selected deletions without another confirmation.
    #[arg(long, short = 'e', conflicts_with = "dry_run")]
    execute: bool,
    /// Preview only; never modify saved storage.
    #[arg(long)]
    dry_run: bool,
}

fn parse_age(value: &str) -> Result<TimeDelta, String> {
    let invalid =
        || "expected a positive integer followed by d or w (for example, 30d or 4w)".to_string();
    let (number, weeks) = if let Some(number) = value.strip_suffix('d') {
        (number, false)
    } else if let Some(number) = value.strip_suffix('w') {
        (number, true)
    } else {
        return Err(invalid());
    };
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let days = number
        .parse::<i64>()
        .ok()
        .filter(|days| *days > 0)
        .and_then(|days| days.checked_mul(if weeks { 7 } else { 1 }))
        .ok_or_else(invalid)?;
    TimeDelta::try_days(days).ok_or_else(invalid)
}

pub(crate) async fn run(
    command: GcCommand,
    overrides: CliConfigOverrides,
    loader: LoaderOverrides,
    harness: ConfigOverrides,
    strict_config: bool,
) -> anyhow::Result<()> {
    let started = Utc::now();
    let cutoff = command
        .older_than
        .map(|age| {
            started
                .checked_sub_signed(age)
                .context("age cutoff overflows the supported UTC date range")
        })
        .transpose()?;
    let execute = !command.dry_run && (command.older_than.is_none() || command.execute);
    let mut config = ConfigBuilder::default()
        .cli_overrides(overrides.parse_overrides().map_err(anyhow::Error::msg)?)
        .loader_overrides(loader)
        .harness_overrides(harness)
        .strict_config(strict_config)
        .build()
        .await?;
    let mut skipped_resources = Vec::new();
    let plan = if let Some(cutoff) = cutoff {
        let scratch = tempfile::tempdir()?;
        match config
            .sqlite
            .open_read_only_snapshot(&config.sqlite.state_db_path(), scratch.path())
            .await
        {
            Ok(pool) => {
                let plan = RetentionPlan::scan(
                    &LocalThreadStoreConfig::from_config(&config),
                    &pool,
                    cutoff,
                )
                .await;
                pool.close().await;
                match plan {
                    Ok(plan) => plan,
                    Err(error) => {
                        skipped_resources.push(error.to_string());
                        RetentionPlan::default()
                    }
                }
            }
            Err(error) => {
                skipped_resources.push(format!(
                    "conversation inventory unavailable; no conversations selected: {error}"
                ));
                RetentionPlan::default()
            }
        }
    } else {
        RetentionPlan::default()
    };

    let mut databases = Vec::new();
    for db in config.sqlite.runtime_db_paths() {
        if !db.path.try_exists()? {
            continue;
        }
        let scratch = tempfile::tempdir()?;
        match config
            .sqlite
            .open_read_only_snapshot(&db.path, scratch.path())
            .await
        {
            Ok(pool) => {
                let estimate = codex_state::estimate_reclamation(&pool).await;
                pool.close().await;
                match estimate {
                    Ok(bytes) => databases.push((db, bytes)),
                    Err(error) => skipped_resources.push(format!("{}: {error}", db.label)),
                }
            }
            Err(error) => skipped_resources.push(format!("{}: {error}", db.label)),
        }
    }
    // models-manager/cache.rs treats absence as a cache miss and refetches the catalog.
    // version.json also stores the user's dismissed-version choice, so it is NOT disposable.
    let cache_path = config.codex_home.join("models_cache.json");
    let cache = match std::fs::symlink_metadata(&cache_path) {
        Ok(metadata)
            if metadata.is_file()
                && std::fs::read(&cache_path).is_ok_and(|contents| {
                    serde_json::from_slice::<codex_models_manager::cache::ModelsCacheEntry>(
                        &contents,
                    )
                    .is_ok()
                }) =>
        {
            Some((cache_path, metadata))
        }
        Ok(_) => {
            skipped_resources.push("models_cache.json: uncertain file type or contents".into());
            None
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            skipped_resources.push(format!("models_cache.json: {error}"));
            None
        }
    };
    let cache = cache.and_then(|(path, metadata)| {
        match std::fs::File::open(&path).and_then(|file| {
            file.try_lock()?;
            let bytes = removable_file_bytes(&path, &metadata)?;
            Ok((file, bytes))
        }) {
            Ok((lock, bytes)) => Some((path, metadata, lock, bytes)),
            Err(error) => {
                skipped_resources
                    .push(format!("models_cache.json is busy or unavailable: {error}"));
                None
            }
        }
    });
    let maintenance_bytes = databases.iter().map(|(_, bytes)| bytes).sum::<u64>()
        + cache.as_ref().map_or(0, |(_, _, _, bytes)| *bytes);
    print!(
        "{}",
        render_report(&plan, cutoff, maintenance_bytes, execute)
    );
    for message in skipped_resources {
        println!("Skipped resource: {}", escape(&message));
    }
    if !execute {
        return Ok(());
    }

    let mut deleted = 0;
    let mut skipped = 0;
    let mut failed = 0;
    if !plan.groups.is_empty() {
        // Reuse host-owned cleanup without launching unrelated compression/migration workers.
        let setup: anyhow::Result<_> = async {
            config
                .features
                .disable(Feature::LocalThreadStoreCompression)?;
            config
                .features
                .disable(Feature::BackgroundPaginatedRolloutMigration)?;
            codex_state::StateRuntime::open_existing_for_retention(
                config.sqlite.clone(),
                config.model_provider_id.clone(),
            )
            .await
        }
        .await;
        match setup {
            Ok(state) => {
                let store = codex_core::thread_store_from_config(&config, Some(state.clone()));
                let selected = plan
                    .groups
                    .iter()
                    .map(|group| group.conversations.len())
                    .sum::<usize>();
                let mut last_progress = std::time::Instant::now();
                let mut on_group = |count, result| {
                    match result {
                        Ok(()) => deleted += count,
                        Err(
                            ThreadStoreError::Conflict { message }
                            | ThreadStoreError::InvalidRequest { message },
                        ) => {
                            skipped += count;
                            println!("Skipped {count} conversations: {}", escape(&message));
                        }
                        Err(error) => {
                            failed += count;
                            println!(
                                "Failed deletion of {count} conversations (cleanup may be partial): {}",
                                escape(&error.to_string())
                            );
                        }
                    }
                    let processed = deleted + skipped + failed;
                    if processed == selected
                        || last_progress.elapsed()
                            >= std::time::Duration::from_secs(/*secs*/ 1)
                    {
                        println!(
                            "Deletion progress: {processed}/{selected} processed; {deleted} deleted, {skipped} skipped, {failed} failures."
                        );
                        last_progress = std::time::Instant::now();
                    }
                };
                let result = if let Some(local) = store.as_any().downcast_ref::<LocalThreadStore>()
                {
                    local
                        .delete_retention_groups(&plan.groups, &mut on_group)
                        .await
                } else {
                    Err(ThreadStoreError::Unsupported {
                        operation: "conversation_retention",
                    })
                };
                if let Err(error) = result {
                    on_group(selected, Err(error));
                }
                drop(store);
                state.close().await;
            }
            Err(error) if codex_state::sqlite_error_detail_is_lock(&error.to_string()) => {
                skipped += plan
                    .groups
                    .iter()
                    .map(|group| group.conversations.len())
                    .sum::<usize>();
                println!(
                    "Skipped retention: storage is busy: {}",
                    escape(&error.to_string())
                );
            }
            Err(error) => {
                failed += plan
                    .groups
                    .iter()
                    .map(|group| group.conversations.len())
                    .sum::<usize>();
                println!(
                    "Retention failed before deletion: {}",
                    escape(&error.to_string())
                );
            }
        }
    }
    if let Some((path, before, _lock, _)) = cache {
        match std::fs::symlink_metadata(&path) {
            Ok(after)
                if after.is_file()
                    && before.len() == after.len()
                    && before.modified()? == after.modified()? =>
            {
                if let Err(error) = std::fs::remove_file(&path) {
                    failed += 1;
                    println!("Cache cleanup failed: {error}");
                }
            }
            Ok(_) | Err(_) => println!("Skipped cache: changed during inspection"),
        }
    }
    for (db, bytes) in databases {
        if bytes == 0 && deleted == 0 {
            continue;
        }
        match codex_state::reclaim_database(&config.sqlite, &db).await {
            Ok(pass) => {
                let status = match pass.outcome {
                    ReclamationOutcome::Idle | ReclamationOutcome::Active => "finished",
                    ReclamationOutcome::Contended => "busy; remaining work skipped",
                    ReclamationOutcome::Interrupted | ReclamationOutcome::Shutdown => {
                        "remaining work deferred"
                    }
                };
                println!(
                    "{}: reclaimed {} unused SQLite pages; {status}.",
                    db.label, pass.pages
                );
            }
            Err(error) => {
                failed += 1;
                println!("{} maintenance failed: {error}", db.label);
            }
        }
    }
    println!(
        "Execution results: deleted {deleted} conversations; skipped {skipped} at execution; {failed} failures."
    );
    anyhow::ensure!(
        failed == 0,
        "gc encountered failures; cleanup may be partial"
    );
    Ok(())
}

fn render_report(
    plan: &RetentionPlan,
    cutoff: Option<DateTime<Utc>>,
    maintenance: u64,
    execute: bool,
) -> String {
    let mut output = String::new();
    let mode = if execute {
        "Execution plan"
    } else {
        "Preview (read-only)"
    };
    // Formatting into a String cannot fail.
    let _ = writeln!(output, "{mode}");
    let _ = match cutoff {
        Some(cutoff) => writeln!(
            output,
            "UTC cutoff (last update strictly before): {}",
            cutoff.to_rfc3339()
        ),
        None => writeln!(output, "No age cutoff: all conversations are preserved."),
    };
    let mut conversations = plan
        .groups
        .iter()
        .flat_map(|group| &group.conversations)
        .collect::<Vec<_>>();
    let directories = conversations
        .iter()
        .filter_map(|thread| thread.cwd.as_ref())
        .collect::<BTreeSet<_>>();
    let missing_cwd = conversations
        .iter()
        .filter(|thread| thread.cwd.is_none())
        .count();
    let action = if execute {
        "Selected for deletion:"
    } else {
        "Would delete"
    };
    let _ = writeln!(
        output,
        "{action} {} conversations across {} working directories ({missing_cwd} missing cwd).",
        conversations.len(),
        directories.len()
    );
    let _ = writeln!(
        output,
        "Estimated reclaimable conversation storage: {:.6} GiB.",
        conversations.iter().map(|thread| thread.bytes).sum::<u64>() as f64 / 1073741824.0
    );
    let _ = writeln!(
        output,
        "Additional data-preserving maintenance: {:.3} MiB.",
        maintenance as f64 / 1048576.0
    );
    let active = plan
        .skipped
        .values()
        .filter(|reason| matches!(reason, RetentionSkip::Active))
        .count();
    let referenced = plan
        .skipped
        .values()
        .filter(|reason| matches!(reason, RetentionSkip::Referenced))
        .count();
    let _ = writeln!(
        output,
        "Skipped: {active} active, {referenced} referenced, {} otherwise ineligible.",
        plan.skipped.len() - active - referenced
    );
    let _ = writeln!(
        output,
        "Working directories and project files are not deletion targets."
    );
    let _ = writeln!(
        output,
        "File estimates use allocated bytes; retained hard links are excluded. SQLite estimates exclude its retained reserve and are counted once. Actual disk-space gains can differ."
    );
    conversations.sort_by_key(|thread| (std::cmp::Reverse(thread.bytes), thread.id.to_string()));
    for thread in conversations.into_iter().take(/*n*/ 5) {
        let mut title = thread.title.chars().take(/*n*/ 80).collect::<String>();
        if title.len() < thread.title.len() {
            title.push('…');
        }
        let _ = writeln!(
            output,
            "{:.3} MiB  {}  {}  {}  {}",
            thread.bytes as f64 / 1048576.0,
            thread
                .updated_at
                .map_or_else(|| "unknown".into(), |time| time.to_rfc3339()),
            thread.id,
            escape(thread.cwd.as_deref().unwrap_or("<missing cwd>")),
            escape(&title)
        );
    }
    let mut skipped = plan.skipped.iter().collect::<Vec<_>>();
    skipped.sort_by_key(|(id, _)| id.to_string());
    for (id, reason) in skipped.into_iter().take(/*n*/ 20) {
        let reason = match reason {
            RetentionSkip::Active => "active writer",
            RetentionSkip::Referenced => "history required by another conversation",
            RetentionSkip::Ineligible(reason) => reason,
        };
        let _ = writeln!(output, "Skipped {id}: {}", escape(reason));
    }
    output
}

fn escape(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(character, '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                character.escape_default().to_string()
            } else {
                character.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "gc_tests.rs"]
mod tests;
