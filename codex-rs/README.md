# Codex CLI

[**Codex CLI Documentation**](https://developers.openai.com/codex/cli)

`codex gc` performs data-preserving SQLite maintenance and removes the regenerable model
catalog cache. `codex gc --dry-run` previews it without changing saved storage.

`codex gc --older-than 30d` (or `4w`) previews whole-conversation deletion using the last
update in UTC, including archived conversations and spawned descendants. Add `--execute`
or `-e` to perform it without another confirmation. `--execute` conflicts with `--dry-run`;
without an age cutoff it performs only safe maintenance.

Active writers, retained history references, incomplete metadata, and changed subtrees
are skipped. Working directories in the report are labels; their project files are preserved.
Retention requires exclusive state-database access and skips while another client has it open,
including clients preparing unpublished forks.
Deletion reports progress as conversation groups finish.
File estimates use allocated bytes and exclude retained hard links; files whose allocation
cannot be inspected are skipped. SQLite estimates respect the reclamation reserve. Databases not
approved for concurrent reclamation require exclusive access and are skipped while open
in another client. Actual disk gains can differ.
Previews inspect temporary database copies, including WAL data, without creating source
sidecars. Uncertain caches, installer/save temporaries, installed plugins, backups, and
the thread-history database are preserved.
