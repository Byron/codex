# Rustty pane status (private OSC extension)

This companion TUI change reports the conversation currently displayed by each
Codex TUI to its own terminal output. Rustty can assign one Stream Deck tile per
reporting pane, including two ordinary TUIs in the same directory. Switching
threads changes that pane's existing tile. Hidden subagents have no separate tile.

Build from this checkout without installing or replacing anything:

```sh
cd codex-rs
CARGO_TARGET_DIR=/tmp/codex-rustty-build cargo build -p codex-cli
/tmp/codex-rustty-build/debug/codex -c 'tui.terminal_status="rustty"'
```

The command-line override changes no configuration file. With a build containing
this change, the persistent equivalent is the following **opt-in** setting:

```toml
[tui]
terminal_status = "rustty"
```

Enable Rustty's `stream-deck = true` separately. The producer does not use
`TERM_PROGRAM`; Rustty may still advertise `ghostty`. Ordinary launching and
shared app-server/daemon operation are preserved: each TUI uses its own existing
event subscription and stdout, not a proxy, hook, observer server, or guessed
`/dev/tty`. Redirected stdout does not receive status frames. Building and testing
this feature does not change installed applications or global Codex settings.

The dashboard requires no additional daemon. Rustty owns pane status, assignments,
and the device. The emitter works both with Codex's embedded server (`--no-daemon`)
and its existing optional shared app-server; choosing the shared server is a
Codex launch preference, not a dashboard requirement.

## Wire format and ownership

The opt-in extension is `ESC ] 777;rustty-agent;1;BASE64(JSON) BEL`, with compact
UTF-8 JSON. It is a private Rustty subcommand, not a standardized numeric OSC.
The normal notification backend remains independent. The TUI emits a `begin`
with the current snapshot, coalesced `update` packets, and `end` on foreground-loop
shutdown. A packet contains `op`, `state`, optional `label`, `thread_id` when
attached, and `turn_id` for `done`. Between thread attachments an `unknown`
snapshot can omit thread metadata. No packet names a target pane or requests an
action. The terminal attributes the message to the receiving PTY.

The whole base64 frame is written under one stdout lock and flushed from the
serialized TUI loop. Job-control polling runs on that same foreground path:
Ctrl-Z and external-editor handoffs emit `end` before yielding the terminal;
return begins a fresh lifecycle with the current snapshot. There is no background
teardown writer. Failed output
disables reporting for that TUI, without stopping its terminal or retrying partial
frames. Normal redraw, compaction, title/progress expiry and thread switches do
not end a lifecycle. The nested resume picker reports unknown while its separate
event pump temporarily stops observing the displayed conversation.

Decoded JSON is at most 1024 bytes. Labels omit control characters and are
truncated at grapheme boundaries to at most 128 UTF-8 bytes. IDs must be nonempty
ASCII without controls and at most 128 bytes. Invalid metadata cannot enter OSC
syntax: all metadata is serialized into JSON, then base64 encoded. Rustty still
validates received packets independently. Other processes sharing the PTY can
spoof that pane's metadata; this is not authentication.

## State evidence

| State | Source and precedence |
| --- | --- |
| Idle | A newly attached ready thread or an interrupted/cancelled turn. Historical completed turns at first attachment do not invent a new completion. |
| Working | `turn/started`, a resumed in-progress turn, or `thread/status/changed` active. A generic active update does not erase an explicit error. |
| Needs input | An unresolved approval, MCP elicitation or user-input request in the existing per-thread request store, unanswered asynchronous agent questions in the displayed widget, or waiting-on-approval/user-input status flags. Optional `isBlocking = false` requests also need attention; this does not claim all work stopped. |
| Done | An observed `turn/completed` with `Completed` status and its actual turn ID. Duplicate snapshots and ordinary cached thread switches preserve the ID. This does not prove success. |
| Error | An explicit nonretrying relevant turn error, failed turn or thread system-error status. Transient retries and individual handled tool errors do not mark the whole thread failed. |
| Paused | An explicitly paused goal update or resume-time goal snapshot while the thread is idle. A pending question keeps its attention mark. Job-control suspension unregisters while the shell owns the terminal. |
| Unknown | Lost connection, unloaded/closed thread, lagged event history, or a thread transition with insufficient evidence. Timeout and disconnect never imply completion. |

Requests resolve through existing user-response, cancellation, turn-ending and
server-request-resolution paths, including automatic resolution. Selecting or
focusing a Stream Deck tile never writes a request response. Background events
update their own thread stores; only the displayed thread becomes this TUI's
snapshot. Thread names supply labels; omission allows Rustty's pane fallback.
Aggregate waiting flags follow authoritative thread status: starting or completing
a turn alone does not establish that an optional request was resolved.

Rustty owns unread-completion acknowledgement. It can project acknowledged done
as idle without sending anything to Codex. Switching away and back to a cached
thread republishes the same completion identity; a later turn uses its new ID.
An initial attachment after discarding all local TUI replay state starts idle for
historical completed turns. A crash, SIGKILL, or external SIGSTOP can bypass
foreground cleanup; Rustty must use an observable enclosing command/PTY exit or
retain uncertainty. There is no heartbeat or completion-by-timeout heuristic.

## Existing-client fallback

Unmodified clients can still use ordinary native notifications:

```toml
[tui]
notifications = ["agent-turn-complete", "approval-requested"]
notification_method = "osc9"
notification_condition = "always"
```

These are notifications, without structured begin/working/request-resolution
snapshots. They do not discover/register every stock Codex session. Detached
command hooks and captured tool stdout do not implement this integration.

## Verification boundaries

| Category | Boundary |
| --- | --- |
| Producer | Full structured status requires this companion TUI build. No plugin or hook installation is sufficient. |
| Scope | One serialized reporter per Rustty PTY; no hidden-subagent or outside-Rustty tile, and no agent command/approval/usage action. |
| Semantics | Done is an observed completed turn; missing process cleanup remains uncertain. Initial historical completion is idle. |
| Multiplexers | Uses the OSC 9 backend's tmux DCS passthrough convention. Multiple concurrent reporters in tmux panes sharing one Rustty PTY exceed v1. End-to-end tmux/SSH behavior requires separate validation. |
| Platforms | Unit tests and source builds do not establish Windows ConPTY, native focus, installed-binary, physical Stream Deck, firmware or transport support. |
| Isolation | Unit tests use disposable fixtures and in-memory output. Separate macOS PTY runs verified both persistent configuration and `-c 'tui.terminal_status="rustty"'` with two ordinary TUIs sharing an isolated daemon and local mock responses. No external model calls were made. |
| Remaining runtime checks | Real provider responses, approval auto-resolution, resume/fork and displayed-agent switching, SSH/tmux, Windows ConPTY and physical hardware require separate runtime validation. |

The regression tests cover bounded Unicode metadata, all serialized states,
coalescing/lifecycle ordering, switching/duplicate turn identity, independent
same-directory app fixtures, request auto-resolution/cancellation, disconnect,
and tmux framing. Both macOS PTY opt-in runs verified distinct same-directory
thread IDs on one shared daemon, pane-local OSC across the full lifetime, `/clear`
within one reporting lifecycle, working/completion/cancellation, optional input
resolution, approval denial without command execution, preserved OSC 9
notifications, normal `end`, and disabled reporting. The optional-input case used
`request_user_input` in Default mode, whose exercised server handler sets
`isBlocking=false`; it did not exercise `request_user_input_async`. Both reporting
TUIs and the opt-out TUI exited successfully, and the fixture reaped its daemon.
These checks do not establish other transports or real-device support.

Run the focused tests with the repository's test-thread stack size:

```sh
RUST_MIN_STACK=8388608 cargo test -p codex-tui --lib terminal_status
RUST_MIN_STACK=8388608 cargo test -p codex-tui --lib notifications::osc9
RUST_MIN_STACK=8388608 cargo test -p codex-tui --lib daemon_startup_tests
cargo test -p codex-config terminal_status
cargo clippy -p codex-tui --lib --tests --no-deps -- -D warnings
cargo fmt --check
```

At the referenced 2026-10-08 checkout, dependency-inclusive strict Clippy stops on
the existing `clippy::let_and_return` at
`app-server/src/message_processor.rs:372`. That source is unchanged by this
integration; strict TUI library/test linting with `--no-deps` passes.
