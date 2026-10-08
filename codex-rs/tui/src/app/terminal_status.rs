//! Semantic evidence lives beside the existing per-thread request/replay state.
use super::App;
use crate::terminal_status::Snapshot;
use crate::terminal_status::State;
use crate::tui::Tui;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadGoalStatus;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnStatus;
use std::io::IsTerminal;

#[derive(Clone, Debug, Default)]
pub(super) struct ThreadStatusReport {
    state: State,
    turn_id: Option<String>,
    status_attention: bool,
    goal_paused: bool,
}

impl ThreadStatusReport {
    pub(super) fn set_goal_paused(&mut self, paused: bool) {
        self.goal_paused = paused;
    }

    pub(super) fn attach(&mut self, turn: Option<&Turn>) {
        // Historical completion is not a new completion. Keep already-observed IDs on refresh.
        match turn {
            Some(turn)
                if self.state == State::Done
                    && turn.status == TurnStatus::Completed
                    && self.turn_id.as_deref() == Some(turn.id.as_str()) => {}
            Some(turn) => {
                self.turn_id = Some(turn.id.clone());
                self.state = match turn.status {
                    TurnStatus::InProgress => State::Working,
                    TurnStatus::Failed => State::Error,
                    TurnStatus::Completed | TurnStatus::Interrupted => State::Idle,
                };
                self.status_attention = false;
            }
            None => {
                self.turn_id = None;
                self.state = State::Idle;
                self.status_attention = false;
            }
        }
    }
    pub(super) fn uncertain(&mut self) {
        self.state = State::Unknown;
        self.status_attention = false;
    }
    pub(super) fn observe(&mut self, notification: &ServerNotification) {
        match notification {
            ServerNotification::ThreadStarted(n) => self.thread_status(&n.thread.status),
            ServerNotification::ThreadStatusChanged(n) => self.thread_status(&n.status),
            ServerNotification::TurnStarted(n) => {
                self.state = State::Working;
                self.turn_id = Some(n.turn.id.clone());
            }
            ServerNotification::TurnCompleted(n) => {
                if self.state == State::Working
                    && self.turn_id.as_deref().is_some_and(|id| id != n.turn.id)
                {
                    return;
                }
                self.turn_id = Some(n.turn.id.clone());
                self.state = match n.turn.status {
                    TurnStatus::Completed => State::Done,
                    TurnStatus::Interrupted => State::Idle,
                    TurnStatus::Failed => State::Error,
                    TurnStatus::InProgress => State::Working,
                };
            }
            ServerNotification::Error(n)
                if !n.will_retry && self.turn_id.as_deref().is_none_or(|id| id == n.turn_id) =>
            {
                self.state = State::Error
            }
            ServerNotification::ThreadClosed(_) => self.uncertain(),
            ServerNotification::ThreadGoalUpdated(n) => {
                self.goal_paused = n.goal.status == ThreadGoalStatus::Paused
            }
            ServerNotification::ThreadGoalCleared(_) => self.goal_paused = false,
            _ => {}
        }
    }
    pub(super) fn thread_status(&mut self, status: &ThreadStatus) {
        match status {
            ThreadStatus::NotLoaded => self.uncertain(),
            ThreadStatus::SystemError => {
                self.state = State::Error;
                self.status_attention = false;
            }
            ThreadStatus::Idle => {
                if matches!(self.state, State::Working | State::Unknown) {
                    self.state = State::Idle;
                }
                self.status_attention = false;
            }
            ThreadStatus::Active { active_flags } => {
                self.status_attention = !active_flags.is_empty();
                if self.state != State::Error {
                    self.state = State::Working;
                }
            }
        }
    }
    fn state(&self, pending_request: bool) -> State {
        if self.state == State::Unknown {
            State::Unknown
        } else if pending_request || self.status_attention {
            // Both blocking and optional requests need attention; neither means all work stopped.
            State::NeedsInput
        } else if self.goal_paused && matches!(self.state, State::Idle | State::Done) {
            State::Paused
        } else {
            self.state
        }
    }
}

impl App {
    pub(super) async fn set_terminal_thread_status(
        &self,
        thread_id: codex_protocol::ThreadId,
        status: &ThreadStatus,
    ) {
        if let Some(channel) = self.thread_event_channels.get(&thread_id) {
            channel
                .store
                .lock()
                .await
                .terminal_status
                .thread_status(status);
        }
    }
    pub(super) async fn sync_terminal_status(&self, tui: &mut Tui) {
        if self.local_settings.tui.terminal_status.is_none() || !std::io::stdout().is_terminal() {
            tui.end_terminal_status();
            return;
        }
        tui.report_terminal_status(Some(self.terminal_status_snapshot().await));
    }

    async fn terminal_status_snapshot(&self) -> Snapshot {
        let Some(thread_id) = self.current_displayed_thread_id() else {
            return Snapshot::unknown();
        };
        let Some(channel) = self.thread_event_channels.get(&thread_id) else {
            return Snapshot::unknown();
        };
        let store = channel.store.lock().await;
        let pending = store
            .pending_interactive_replay
            .has_pending_thread_approvals()
            || store
                .pending_interactive_replay
                .has_pending_thread_user_input()
            || (self.chat_widget.thread_id() == Some(thread_id)
                && self.chat_widget.has_unanswered_async_questions());
        let state = if self.reconnect.offline {
            State::Unknown
        } else {
            store.terminal_status.state(pending)
        };
        let widget_name = (self.chat_widget.thread_id() == Some(thread_id))
            .then(|| self.chat_widget.thread_name())
            .flatten();
        let label = widget_name.as_deref().or_else(|| {
            store
                .session
                .as_ref()
                .and_then(|session| session.thread_name.as_deref())
        });
        let snapshot = Snapshot::new(
            state,
            label,
            thread_id.to_string(),
            store.terminal_status.turn_id.as_deref(),
        );
        snapshot.unwrap_or_else(Snapshot::unknown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ThreadEventStore;
    use crate::app::test_support::make_test_app;
    use codex_app_server_protocol::RequestId;
    use codex_app_server_protocol::ServerRequest;
    use codex_app_server_protocol::ServerRequestResolvedNotification;
    use codex_app_server_protocol::ThreadActiveFlag;
    use codex_app_server_protocol::ToolRequestUserInputParams;
    use codex_app_server_protocol::TurnCompletedNotification;
    use codex_app_server_protocol::TurnStartedNotification;
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;

    fn turn(id: &str, status: TurnStatus) -> Turn {
        Turn {
            id: id.into(),
            items: Vec::new(),
            items_view: Default::default(),
            status,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }
    }
    fn started(thread: &str, id: &str) -> ServerNotification {
        ServerNotification::TurnStarted(TurnStartedNotification {
            thread_id: thread.into(),
            turn: turn(id, TurnStatus::InProgress),
        })
    }
    fn completed(thread: &str, id: &str, status: TurnStatus) -> ServerNotification {
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: thread.into(),
            turn: turn(id, status),
        })
    }
    fn pending(store: &ThreadEventStore) -> bool {
        store
            .pending_interactive_replay
            .has_pending_thread_approvals()
            || store
                .pending_interactive_replay
                .has_pending_thread_user_input()
    }
    fn request(blocking: bool) -> ServerRequest {
        ServerRequest::ToolRequestUserInput {
            request_id: RequestId::Integer(3),
            params: ToolRequestUserInputParams {
                thread_id: "a".into(),
                turn_id: "t1".into(),
                item_id: "q1".into(),
                questions: Vec::new(),
                is_blocking: blocking,
                auto_resolution_ms: None,
            },
        }
    }

    #[test]
    fn initial_idle_is_not_done_and_only_observed_completion_survives_refresh() {
        let mut report = ThreadStatusReport::default();
        assert_eq!(report.state(false), State::Idle);
        report.attach(Some(&turn("t1", TurnStatus::Completed)));
        assert_eq!(report.state(false), State::Idle);
        report.observe(&completed("a", "t1", TurnStatus::Completed));
        assert_eq!(report.state(false), State::Done);
        report.thread_status(&ThreadStatus::Idle);
        report.attach(Some(&turn("t1", TurnStatus::Completed)));
        assert_eq!(report.state(false), State::Done);
        assert_eq!(report.turn_id.as_deref(), Some("t1"));
        report.observe(&started("a", "t2"));
        report.observe(&completed("a", "t1", TurnStatus::Completed));
        assert_eq!(report.state(false), State::Working);
        report.observe(&completed("a", "t2", TurnStatus::Completed));
        assert_eq!(report.turn_id.as_deref(), Some("t2"));
    }

    #[test]
    fn input_is_attention_even_when_nonblocking_and_resolution_or_cancellation_clears_it() {
        for blocking in [false, true] {
            let mut store = ThreadEventStore::new(8);
            store.push_notification(started("a", "t1"));
            store
                .pending_interactive_replay
                .note_server_request(&request(blocking));
            assert_eq!(
                store.terminal_status.state(pending(&store)),
                State::NeedsInput
            );
            store.push_notification(ServerNotification::ServerRequestResolved(
                ServerRequestResolvedNotification {
                    thread_id: "a".into(),
                    request_id: RequestId::Integer(3),
                },
            ));
            assert_eq!(store.terminal_status.state(pending(&store)), State::Working);
            store
                .pending_interactive_replay
                .note_server_request(&request(blocking));
            store.push_notification(completed("a", "t1", TurnStatus::Interrupted));
            assert_eq!(store.terminal_status.state(pending(&store)), State::Idle);
        }
    }

    #[test]
    fn approval_auto_resolution_is_not_a_focus_or_completion_action() {
        let mut store = ThreadEventStore::new(8);
        store.push_notification(started("a", "t1"));
        store.push_request(ServerRequest::FileChangeRequestApproval {
            request_id: RequestId::Integer(4),
            params: codex_app_server_protocol::FileChangeRequestApprovalParams {
                thread_id: "a".into(),
                turn_id: "t1".into(),
                item_id: "patch".into(),
                started_at_ms: 0,
                reason: None,
                grant_root: None,
            },
        });
        assert_eq!(
            store.terminal_status.state(pending(&store)),
            State::NeedsInput
        );
        // Reading/focusing the snapshot never resolves the request.
        assert_eq!(
            store.terminal_status.state(pending(&store)),
            State::NeedsInput
        );
        store.push_notification(ServerNotification::ServerRequestResolved(
            ServerRequestResolvedNotification {
                thread_id: "a".into(),
                request_id: RequestId::Integer(4),
            },
        ));
        assert_eq!(store.terminal_status.state(pending(&store)), State::Working);
    }

    #[test]
    fn aggregate_attention_survives_turn_boundaries_until_authoritative_resolution() {
        let mut report = ThreadStatusReport::default();
        report.thread_status(&ThreadStatus::Active {
            active_flags: vec![ThreadActiveFlag::WaitingOnUserInput],
        });
        report.observe(&started("a", "t1"));
        assert_eq!(report.state(false), State::NeedsInput);
        report.observe(&completed("a", "t1", TurnStatus::Completed));
        assert_eq!(report.state(false), State::NeedsInput);
        report.thread_status(&ThreadStatus::Idle);
        assert_eq!(report.state(false), State::Done);
        assert_eq!(report.turn_id.as_deref(), Some("t1"));
    }

    #[test]
    fn flags_error_pause_and_unknown_do_not_invent_completion() {
        let mut report = ThreadStatusReport::default();
        report.thread_status(&ThreadStatus::Active {
            active_flags: vec![ThreadActiveFlag::WaitingOnApproval],
        });
        assert_eq!(report.state(false), State::NeedsInput);
        report.observe(&ServerNotification::ServerRequestResolved(
            ServerRequestResolvedNotification {
                thread_id: "a".into(),
                request_id: RequestId::Integer(99),
            },
        ));
        assert_eq!(report.state(false), State::NeedsInput);
        report.thread_status(&ThreadStatus::Active {
            active_flags: Vec::new(),
        });
        assert_eq!(report.state(false), State::Working);
        report.thread_status(&ThreadStatus::Active {
            active_flags: vec![ThreadActiveFlag::WaitingOnApproval],
        });
        report.thread_status(&ThreadStatus::SystemError);
        assert_eq!(report.state(false), State::Error);
        report.thread_status(&ThreadStatus::Active {
            active_flags: Vec::new(),
        });
        assert_eq!(report.state(false), State::Error);
        report.observe(&started("a", "t1"));
        assert_eq!(report.state(false), State::Working);
        report.observe(&completed("a", "t1", TurnStatus::Failed));
        assert_eq!(report.state(false), State::Error);
        report.thread_status(&ThreadStatus::NotLoaded);
        assert_eq!(report.state(true), State::Unknown);
        report.attach(Some(&turn("t1", TurnStatus::InProgress)));
        assert_eq!(report.state(false), State::Working);
        report.observe(&completed("a", "t1", TurnStatus::Interrupted));
        report.goal_paused = true;
        assert_eq!(report.state(false), State::Paused);
        assert_eq!(report.state(true), State::NeedsInput);
    }

    #[tokio::test]
    async fn displayed_thread_only_and_same_directory_panes_are_independent() {
        let mut first = make_test_app().await;
        let mut second = make_test_app().await;
        second.config.cwd = first.config.cwd.clone();
        assert!(first.local_settings.tui.terminal_status.is_none());
        let a = ThreadId::new();
        let b = ThreadId::new();
        first
            .ensure_thread_channel(a)
            .store
            .lock()
            .await
            .push_notification(completed(&a.to_string(), "t1", TurnStatus::Completed));
        first
            .ensure_thread_channel(b)
            .store
            .lock()
            .await
            .push_notification(started(&b.to_string(), "t2"));
        second
            .ensure_thread_channel(b)
            .store
            .lock()
            .await
            .push_notification(started(&b.to_string(), "t2"));
        first.active_thread_id = Some(a);
        second.active_thread_id = Some(b);
        let before = first.terminal_status_snapshot().await;
        assert_eq!(before.state, State::Done);
        assert_eq!(
            second.terminal_status_snapshot().await.state,
            State::Working
        );
        first.active_thread_id = Some(b);
        assert_eq!(first.terminal_status_snapshot().await.state, State::Working);
        first
            .ensure_thread_channel(a)
            .store
            .lock()
            .await
            .push_notification(completed(&a.to_string(), "t1", TurnStatus::Completed));
        assert_eq!(first.terminal_status_snapshot().await.state, State::Working);
        first.active_thread_id = Some(a);
        assert_eq!(first.terminal_status_snapshot().await, before);
        first.reconnect.offline = true;
        assert_eq!(first.terminal_status_snapshot().await.state, State::Unknown);
        first.reconnect.offline = false;
        assert_eq!(first.terminal_status_snapshot().await, before);
        first.active_thread_id = None;
        assert_eq!(first.terminal_status_snapshot().await.state, State::Unknown);
    }
}
