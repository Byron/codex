use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex_exec::test_codex_exec;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

const OSC_PREFIX: &str = "\x1b]777;rustty-agent;1;";

async fn review_output(cmd: &assert_cmd::Command, terminal_stderr: bool) -> anyhow::Result<Output> {
    let mut command = Command::new(cmd.get_program());
    command
        .args(cmd.get_args())
        .current_dir(cmd.get_current_dir().context("review working directory")?)
        .env("TERM_PROGRAM", "kitty")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("STY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in cmd.get_envs() {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }

    let terminal_output = if terminal_stderr {
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: both output descriptors are valid; optional name/settings pointers are null.
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == -1
        {
            return Err(std::io::Error::last_os_error()).context("open review stderr PTY");
        }
        // SAFETY: successful openpty transfers ownership of two unique file descriptors.
        let master = unsafe { File::from_raw_fd(master) };
        // SAFETY: slave is the other unique descriptor returned by openpty.
        command.stderr(unsafe { File::from_raw_fd(slave) });
        Some(master)
    } else {
        None
    };
    let mut command = tokio::process::Command::from(command);
    let child = command.kill_on_drop(true).spawn()?;
    drop(command);
    let stderr_reader = terminal_output.map(|mut terminal| {
        tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            let mut output = Vec::new();
            let mut buffer = [0; 8192];
            let deadline = Instant::now() + Duration::from_secs(35);
            loop {
                if Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "review stderr PTY did not close",
                    ));
                }
                let mut descriptor = libc::pollfd {
                    fd: terminal.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: descriptor points to one initialized poll descriptor.
                let ready = unsafe { libc::poll(&mut descriptor, 1, 100) };
                if ready == -1 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                if ready == 0 {
                    continue;
                }
                match terminal.read(&mut buffer) {
                    Ok(0) => return Ok(output),
                    Ok(count) => output.extend_from_slice(&buffer[..count]),
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(output),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
        })
    });
    let mut output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .context("review must finish within 30 seconds")??;
    if let Some(stderr_reader) = stderr_reader {
        output.stderr = tokio::time::timeout(Duration::from_secs(5), stderr_reader)
            .await
            .context("review stderr PTY must close after exit")???;
    }
    Ok(output)
}

fn packets(output: &[u8]) -> anyhow::Result<Vec<Value>> {
    String::from_utf8_lossy(output)
        .split(OSC_PREFIX)
        .skip(1)
        .map(|frame| {
            let (payload, _) = frame.split_once('\x07').context("OSC must end with BEL")?;
            Ok(serde_json::from_slice(&STANDARD.decode(payload)?)?)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_terminal_status_obeys_opt_in_and_stderr_tty() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    for (enabled, terminal_stderr, json_mode) in [
        (true, true, false),
        (true, true, true),
        (false, true, false),
        (true, false, false),
    ] {
        let test = test_codex_exec();
        let server = responses::start_mock_server().await;
        let response_mock = responses::mount_sse_once(
            &server,
            responses::sse(vec![
                responses::ev_assistant_message(
                    "review-message",
                    &json!({
                        "findings": [],
                        "overall_correctness": "patch is correct",
                        "overall_explanation": "No issues found.",
                        "overall_confidence_score": 0.9,
                    })
                    .to_string(),
                ),
                responses::ev_completed("review-response"),
            ]),
        )
        .await;
        let mut cmd = test.cmd_with_server(&server);
        cmd.args(["--skip-git-repo-check", "--color", "never"]);
        if enabled {
            cmd.args(["-c", "tui.terminal_status=\"rustty\""]);
        }
        if json_mode {
            cmd.arg("--json");
        }
        cmd.args(["review", "Check the current change."]);
        let output = review_output(&cmd, terminal_stderr).await?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "review failed: {stderr}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains(OSC_PREFIX));
        let emitted = packets(&output.stderr)?;
        if !(enabled && terminal_stderr) {
            assert!(emitted.is_empty(), "unexpected status frames: {stderr}");
            continue;
        }

        assert_eq!(emitted.len(), 3, "status frames: {stderr}");
        assert_eq!(emitted[0]["op"], "begin");
        assert_eq!(emitted[0]["state"], "working");
        assert_eq!(emitted[1]["op"], "update");
        assert_eq!(emitted[1]["state"], "done");
        assert_eq!(emitted[2], json!({ "op": "end" }));
        let metadata: Value = serde_json::from_str(
            &response_mock
                .single_request()
                .header("x-codex-turn-metadata")
                .context("review model request metadata")?,
        )?;
        assert!(metadata["parent_thread_id"].is_string());
        assert!(metadata["parent_turn_id"].is_string());
        assert_eq!(emitted[0]["thread_id"], metadata["parent_thread_id"]);
        assert_eq!(emitted[1]["thread_id"], metadata["parent_thread_id"]);
        assert_eq!(emitted[1]["turn_id"], metadata["parent_turn_id"]);
        if json_mode {
            for line in String::from_utf8(output.stdout)?.lines() {
                let _: Value = serde_json::from_str(line)?;
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_terminal_status_reports_error_and_ends_before_failure_exit() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = responses::start_mock_server().await;
    responses::mount_sse_once(
        &server,
        responses::sse_failed(
            "review-failed",
            "rate_limit_exceeded",
            "synthetic review error",
        ),
    )
    .await;
    let mut cmd = test.cmd_with_server(&server);
    cmd.args([
        "--skip-git-repo-check",
        "-c",
        "tui.terminal_status=\"rustty\"",
        "review",
        "Check the current change.",
    ]);
    let output = review_output(&cmd, /*terminal_stderr*/ true).await?;
    assert_eq!(output.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(OSC_PREFIX));
    let emitted = packets(&output.stderr)?;
    assert_eq!(emitted.first().context("begin packet")?["state"], "working");
    assert!(
        emitted.iter().any(|packet| packet["state"] == "error"),
        "review error was not reported: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(emitted.last(), Some(&json!({ "op": "end" })));
    Ok(())
}
