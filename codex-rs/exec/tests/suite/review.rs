use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex_exec::test_codex_exec;
use std::path::Path;
use std::process::Command;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(cwd)
        .args([
            "-c",
            "user.name=Review Test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn base_review_preserves_merge_base_with_additional_instructions() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let repo = test.cwd_path().join("repo");
    std::fs::create_dir(&repo)?;
    git(&repo, &["init", "-b", "main"]);
    git(
        &repo,
        &["commit", "--allow-empty", "--no-gpg-sign", "-m", "base"],
    );
    let merge_base = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-b", "feature"]);
    git(
        &repo,
        &["commit", "--allow-empty", "--no-gpg-sign", "-m", "change"],
    );

    let server = responses::start_mock_server().await;
    let prompt = "also investigate the CI adjustment and ensure CI is running this new backend";
    for prompt_arg in [Some(prompt), Some("-"), None] {
        let response_mock = responses::mount_sse_once(
            &server,
            responses::sse(vec![responses::ev_completed("resp-review")]),
        )
        .await;
        let mut cmd = test.cmd_with_server(&server);
        cmd.arg("-C").arg(&repo).args(["review", "--base", "main"]);
        if let Some(prompt_arg) = prompt_arg {
            cmd.arg(prompt_arg);
            if prompt_arg == "-" {
                cmd.write_stdin(format!("  {prompt}\n"));
            }
        }
        cmd.assert().success();

        let target_prompt = format!(
            "Review the code changes against the base branch 'main'. The merge base commit for this comparison is {merge_base}. Run `git diff {merge_base}` to inspect the changes relative to main. Provide prioritized, actionable findings."
        );
        let expected = if prompt_arg.is_some() {
            format!("{target_prompt}\n\nAdditional review instructions:\n{prompt}")
        } else {
            target_prompt
        };
        let request = response_mock.single_request();
        assert!(
            request.has_message_with_input_texts("user", |texts| texts == [expected.clone()]),
            "model request should preserve the merge-base prompt and additional instructions: {:?}",
            request.message_input_texts("user")
        );
    }
    Ok(())
}
