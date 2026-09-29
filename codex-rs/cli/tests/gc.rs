#![expect(
    clippy::unwrap_used,
    reason = "fixture setup failures should fail the test immediately"
)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_cargo_bin::cargo_bin;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut result = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else {
                result.insert(
                    entry
                        .path()
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    std::fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    result
}

fn modified_times(root: &Path) -> BTreeMap<std::path::PathBuf, std::time::SystemTime> {
    let mut result = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let metadata = std::fs::metadata(&path).unwrap();
        result.insert(path.clone(), metadata.modified().unwrap());
        if metadata.is_dir() {
            stack.extend(
                std::fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        }
    }
    result
}

fn gc(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(cargo_bin("codex").unwrap())
        .current_dir(home)
        .env("CODEX_HOME", home)
        .env_remove("CODEX_SQLITE_HOME")
        .args(["-c", &format!("sqlite_home={:?}", home.join("database"))])
        .arg("gc")
        .args(args)
        .output()
        .unwrap()
}

async fn fixture() -> TempDir {
    let home = TempDir::new().unwrap();
    let sqlite = SqliteConfig::from_sqlite_home(
        AbsolutePathBuf::from_absolute_path(home.path().join("database")).unwrap(),
    );
    let state = StateRuntime::init(sqlite.clone(), "openai".into())
        .await
        .unwrap();
    let pool = sqlite
        .open_existing_pool(&sqlite.state_db_path())
        .await
        .unwrap();
    for number in 1..=7 {
        let id = format!("00000000-0000-0000-0000-{number:012}");
        let directory = home.path().join(if number % 2 == 0 {
            "archived_sessions"
        } else {
            "sessions/2020/01/01"
        });
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("rollout-2020-01-01T00-00-00-{id}.jsonl"));
        let meta = serde_json::json!({"type":"session_meta", "timestamp":"2020-01-01T00:00:00Z", "payload": {
            "id":id, "timestamp":"2020-01-01T00:00:00Z", "cwd":"/recorded/project", "source":"exec", "originator":"test", "cli_version":"0.0.0", "model_provider":"openai"
        }});
        let event = serde_json::json!({"type":"event_msg", "timestamp":"2020-01-01T00:00:00Z", "payload":{"type":"user_message", "message":"x".repeat(number * 8192)}});
        std::fs::write(&path, format!("{meta}\n{event}\n")).unwrap();
        sqlx::query("INSERT INTO threads (id, rollout_path, created_at, updated_at, updated_at_ms, source, model_provider, cwd, title, sandbox_policy, approval_mode, archived) VALUES (?, ?, 1, 1, ?, 'exec', 'openai', ?, ?, 'read-only', 'on-request', ?)")
            .bind(&id).bind(path.to_str().unwrap()).bind(if number == 7 { chrono::Utc::now().timestamp_millis() } else { 1 })
            .bind(if number == 6 { "" } else if number % 2 == 0 { "/project/two" } else { "/project/one" })
            .bind(format!("title {number}\n\u{1b}[31m{}", "long".repeat(30))).bind(number % 2 == 0).execute(&pool).await.unwrap();
    }
    pool.close().await;
    let logs = sqlite
        .open_existing_pool(&sqlite.logs_db_path())
        .await
        .unwrap();
    sqlx::query("INSERT INTO logs (ts, ts_nanos, level, target, feedback_log_body) VALUES (1, 0, 'INFO', 'fixture', 'saved diagnostic')").execute(&logs).await.unwrap();
    logs.close().await;
    let history = codex_state::open_thread_history_db(&sqlite).await.unwrap();
    sqlx::query("INSERT INTO thread_turns (thread_id, turn_id, rollout_ordinal, status) VALUES ('00000000-0000-0000-0000-000000000001', 'turn', 0, 'completed')").execute(&history).await.unwrap();
    history.close().await;
    state.close().await;
    for name in [
        "auth.json",
        "config.toml.bak",
        "version.json",
        "skills/skill/SKILL.md",
        "rules/default.rules",
        "automations/job/config.toml",
        "plugins/cache/installed/source",
        "cache/user-artifact",
        ".tmp/marketplaces/referenced/source",
        ".tmp/save.tmp",
        "packages/standalone/releases/.staging.unknown/executable",
    ] {
        let path = home.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "saved user data").unwrap();
    }
    std::fs::write(home.path().join("config.toml"), "model = 'gpt-5'\n").unwrap();
    std::fs::write(
        home.path().join("models_cache.json"),
        r#"{"fetched_at":"2020-01-01T00:00:00Z","models":[]}"#,
    )
    .unwrap();
    home
}

#[tokio::test]
async fn previews_are_read_only_and_default_maintenance_preserves_saved_data() {
    let home = fixture().await;
    let before = files(home.path());
    let before_times = modified_times(home.path());
    for args in [
        &["--dry-run"][..],
        &["--older-than", "30d"],
        &["--older-than", "4w"],
    ] {
        let output = gc(home.path(), args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Preview (read-only)"));
        assert_eq!(files(home.path()), before);
        assert_eq!(modified_times(home.path()), before_times);
    }
    for args in [
        &["--help"][..],
        &["--older-than", "0d"],
        &["--older-than", "-1w"],
        &["--older-than", "1000000000d"],
        &["--dry-run", "--execute"],
    ] {
        let output = gc(home.path(), args);
        assert_eq!(output.status.success(), args == ["--help"]);
        assert_eq!(files(home.path()), before);
        assert_eq!(modified_times(home.path()), before_times);
    }
    let busy = std::fs::File::open(home.path().join("models_cache.json")).unwrap();
    busy.try_lock().unwrap();
    let output = gc(home.path(), &[]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("models_cache.json is busy"));
    assert_eq!(files(home.path()), before);
    drop(busy);
    for args in [&[][..], &["--execute"]] {
        let output = gc(home.path(), args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("deleted 0 conversations"));
    }
    let mut expected = before;
    expected.remove("models_cache.json");
    assert_eq!(files(home.path()), expected);
}

#[tokio::test]
async fn execution_reports_unique_candidates_largest_first_and_cleans_history() {
    let home = fixture().await;
    let preview = gc(home.path(), &["--older-than", "30d"]);
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let report = String::from_utf8(preview.stdout).unwrap();
    assert!(
        report
            .contains("Would delete 6 conversations across 2 working directories (1 missing cwd)."),
        "{report}"
    );
    let rows = report
        .lines()
        .filter(|line| line.contains(" MiB  "))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 5);
    for (row, number) in rows.iter().zip((2..=6).rev()) {
        assert!(
            row.contains(&format!("00000000-0000-0000-0000-{number:012}")),
            "{row}"
        );
        assert!(
            row.contains("\\n\\u{1b}[31m") && row.ends_with('…'),
            "{row}"
        );
    }
    let mut normalized = report.replace("logical-size fallback", "allocated bytes");
    for (pattern, replacement) in [
        (
            r"(?m)^UTC cutoff.*$",
            "UTC cutoff (last update strictly before): [cutoff]",
        ),
        (
            r"(?m)^Estimated reclaimable conversation storage:.*$",
            "Estimated reclaimable conversation storage: [size].",
        ),
        (
            r"(?m)^Additional data-preserving maintenance:.*$",
            "Additional data-preserving maintenance: [size].",
        ),
        (r"(?m)^[0-9.]+ MiB  ", "[size]  "),
    ] {
        normalized = regex_lite::Regex::new(pattern)
            .unwrap()
            .replace_all(&normalized, replacement)
            .into_owned();
    }
    insta::assert_snapshot!("gc_retention_preview", normalized);
    let sqlite = SqliteConfig::from_sqlite_home(
        AbsolutePathBuf::from_absolute_path(home.path().join("database")).unwrap(),
    );
    let client = sqlite
        .open_read_only_pool(&sqlite.state_db_path(), /*busy_timeout*/ None)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM threads")
            .fetch_one(&client)
            .await
            .unwrap(),
        7
    );
    let busy = gc(home.path(), &["--older-than", "30d", "--execute"]);
    let busy_report = String::from_utf8(busy.stdout).unwrap();
    assert!(
        busy.status.success(),
        "{busy_report}\n{}",
        String::from_utf8_lossy(&busy.stderr)
    );
    assert!(
        busy_report.contains("deleted 0 conversations; skipped 6 at execution; 0 failures."),
        "{busy_report}"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM threads")
            .fetch_one(&client)
            .await
            .unwrap(),
        7
    );
    client.close().await;
    let output = gc(home.path(), &["--older-than", "30d", "-e"]);
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("deleted 6 conversations; skipped 0 at execution; 0 failures.")
    );
    let report = String::from_utf8(output.stdout).unwrap();
    let progress = report
        .lines()
        .rev()
        .find(|line| line.starts_with("Deletion progress:"))
        .unwrap();
    insta::assert_snapshot!(progress, @"Deletion progress: 6/6 processed; 6 deleted, 0 skipped, 0 failures.");
    let pool = sqlite
        .open_read_only_pool(&sqlite.state_db_path(), /*busy_timeout*/ None)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT id FROM threads")
            .fetch_all(&pool)
            .await
            .unwrap(),
        vec!["00000000-0000-0000-0000-000000000007"]
    );
    pool.close().await;
    let history = sqlite
        .open_read_only_pool(&sqlite.thread_history_db_path(), /*busy_timeout*/ None)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM thread_turns")
            .fetch_one(&history)
            .await
            .unwrap(),
        0
    );
    history.close().await;
    let logs = sqlite
        .open_read_only_pool(&sqlite.logs_db_path(), /*busy_timeout*/ None)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT feedback_log_body FROM logs")
            .fetch_all(&logs)
            .await
            .unwrap(),
        vec!["saved diagnostic"]
    );
    logs.close().await;
}

#[tokio::test]
async fn partial_cleanup_failure_is_reported_and_returns_failure_status() {
    let home = fixture().await;
    let sqlite = SqliteConfig::from_sqlite_home(
        AbsolutePathBuf::from_absolute_path(home.path().join("database")).unwrap(),
    );
    let logs = sqlite
        .open_existing_pool(&sqlite.logs_db_path())
        .await
        .unwrap();
    sqlx::query("INSERT INTO logs (ts, ts_nanos, level, target, feedback_log_body, thread_id) VALUES (1, 0, 'INFO', 'fixture', 'cannot delete', '00000000-0000-0000-0000-000000000006')").execute(&logs).await.unwrap();
    sqlx::query("CREATE TRIGGER refuse_deletion BEFORE DELETE ON logs WHEN OLD.thread_id IS NOT NULL BEGIN SELECT RAISE(FAIL, 'injected cleanup failure'); END").execute(&logs).await.unwrap();
    logs.close().await;
    let output = gc(home.path(), &["--older-than", "4w", "--execute"]);
    assert!(!output.status.success());
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(
        report.contains("cleanup may be partial") && report.contains("1 failures."),
        "{report}"
    );
    assert!(
        report.contains(
            "Execution results: deleted 5 conversations; skipped 0 at execution; 1 failures."
        ),
        "{report}"
    );
}
