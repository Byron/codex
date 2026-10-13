# Commit workflow

- Use the `$tix` skill for all history mutations; use Git for inspection and staging.
- Automatically create a descriptive commit for each completed, semantically distinct change after focused validation, without asking for confirmation. Keep related code, tests, fixtures, and documentation in the same commit.
- For post-commit fixes to a recent change, amend the relevant commit with `tix amend --index` instead of creating a separate commit. If it is an ancestor, use Tix travel and restore the starting checkout afterward. For commits marked `✨`, follow the skill's fixup workflow to preserve human review approval.
- Stage only task-owned changes and use `--index` for Tix new and amend. Author new commits as `Codex <codex@openai.com>` and include the reason for the change and validation in the commit message.
- Use Markdown in both commit titles and bodies: backtick code identifiers, commands, flags, and paths; use paragraphs and lists where helpful.
- Verify the resulting commits and worktree status. Push only when the user requests it.
