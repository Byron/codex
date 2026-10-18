use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::TurnStatus;
use codex_utils_cli::terminal_status::Reporter;
use codex_utils_cli::terminal_status::Snapshot;
use codex_utils_cli::terminal_status::State;
use std::io::Write;

pub(crate) struct ReviewStatus<W: Write> {
    reporter: Option<Reporter>,
    output: W,
}

impl<W: Write> ReviewStatus<W> {
    pub(crate) fn new(enabled: bool, output: W) -> Self {
        Self {
            reporter: enabled.then(Reporter::default),
            output,
        }
    }

    /// The exec loop filters notifications to the review's thread and turn first.
    pub(crate) fn observe(&mut self, notification: &ServerNotification) {
        let (state, thread_id, turn_id) = match notification {
            ServerNotification::TurnStarted(n) => (State::Working, &n.thread_id, &n.turn.id),
            ServerNotification::TurnCompleted(n) => (
                match n.turn.status {
                    TurnStatus::Completed => State::Done,
                    TurnStatus::Failed => State::Error,
                    TurnStatus::Interrupted => State::Idle,
                    TurnStatus::InProgress => State::Working,
                },
                &n.thread_id,
                &n.turn.id,
            ),
            ServerNotification::Error(n) if !n.will_retry => {
                (State::Error, &n.thread_id, &n.turn_id)
            }
            _ => return,
        };
        self.report(Some(
            Snapshot::new(state, Some("Code review"), thread_id.clone(), Some(turn_id))
                .unwrap_or_else(Snapshot::unknown),
        ));
    }

    pub(crate) fn unknown(&mut self) {
        self.report(Some(Snapshot::unknown()));
    }

    pub(crate) fn end(&mut self) {
        self.report(None);
    }

    fn report(&mut self, snapshot: Option<Snapshot>) {
        if let Some(reporter) = &mut self.reporter
            && let Err(error) = reporter.report(snapshot, &mut self.output)
        {
            tracing::debug!(%error, "review terminal status output failed");
        }
    }
}

impl<W: Write> Drop for ReviewStatus<W> {
    fn drop(&mut self) {
        // Broken stdout pipes can unwind the output processor before normal cleanup.
        self.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::ErrorNotification;
    use codex_app_server_protocol::Turn;
    use codex_app_server_protocol::TurnCompletedNotification;
    use pretty_assertions::assert_eq;

    fn completed(status: TurnStatus) -> ServerNotification {
        ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: "thread".into(),
            turn: Turn {
                id: "turn".into(),
                root_turn_id: None,
                items: Vec::new(),
                items_view: Default::default(),
                status,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            },
        })
    }

    #[test]
    fn review_status_preserves_completion_and_interruption_semantics() {
        for (status, state) in [
            (TurnStatus::InProgress, State::Working),
            (TurnStatus::Completed, State::Done),
            (TurnStatus::Failed, State::Error),
            (TurnStatus::Interrupted, State::Idle),
        ] {
            let mut report = ReviewStatus::new(true, Vec::new());
            report.observe(&completed(status));
            assert_eq!(
                report.reporter.as_ref().unwrap().current(),
                Snapshot::new(state, Some("Code review"), "thread".into(), Some("turn"))
            );
            report.unknown();
            assert_eq!(
                report.reporter.as_ref().unwrap().current(),
                Some(Snapshot::unknown())
            );
            report.end();
            assert_eq!(report.reporter.as_ref().unwrap().current(), None);
        }
    }

    #[test]
    fn retrying_errors_and_disabled_reporting_emit_nothing() {
        let mut report = ReviewStatus::new(true, Vec::new());
        let mut error: ErrorNotification = serde_json::from_value(serde_json::json!({
            "threadId": "thread",
            "turnId": "turn",
            "willRetry": true,
            "error": {"message": "retrying"}
        }))
        .unwrap();
        report.observe(&ServerNotification::Error(error.clone()));
        assert!(report.output.is_empty());
        error.will_retry = false;
        report.observe(&ServerNotification::Error(error));
        assert_eq!(
            report.reporter.as_ref().unwrap().current().unwrap().state,
            State::Error
        );

        let mut report = ReviewStatus::new(false, Vec::new());
        report.observe(&completed(TurnStatus::Completed));
        report.unknown();
        report.end();
        assert!(report.output.is_empty());
    }

    #[test]
    fn unwinding_ends_the_report_like_normal_cleanup() {
        let mut expected = Vec::new();
        {
            let mut report = ReviewStatus::new(true, &mut expected);
            report.observe(&completed(TurnStatus::InProgress));
            report.end();
        }
        let mut output = Vec::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut report = ReviewStatus::new(true, &mut output);
            report.observe(&completed(TurnStatus::InProgress));
            panic!("output processor failed");
        }));
        assert!(result.is_err());
        assert_eq!(output, expected);
    }
}
