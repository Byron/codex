//! Ordered, opt-in Rustty OSC output. No background task, hook, or terminal discovery.
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use codex_terminal_detection::Multiplexer;
use codex_terminal_detection::terminal_info;
use serde::Serialize;
use std::io;
use std::io::Write;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    #[default]
    Idle,
    Working,
    NeedsInput,
    Done,
    Error,
    Paused,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Snapshot {
    pub(crate) state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_id: Option<String>,
}

impl Snapshot {
    pub(crate) fn unknown() -> Self {
        Self {
            state: State::Unknown,
            label: None,
            thread_id: None,
            turn_id: None,
        }
    }

    pub(crate) fn new(
        state: State,
        label: Option<&str>,
        thread_id: String,
        turn_id: Option<&str>,
    ) -> Option<Self> {
        if !valid_id(&thread_id) {
            return None;
        }
        let turn_id = if state == State::Done {
            Some(turn_id.filter(|value| valid_id(value))?.to_owned())
        } else {
            None
        };
        let label = label
            .map(|label| {
                let clean: String = label.chars().filter(|c| !c.is_control()).collect();
                let mut result = String::new();
                for grapheme in clean.graphemes(true) {
                    if result.len() + grapheme.len() > 128 {
                        break;
                    }
                    result.push_str(grapheme);
                }
                result
            })
            .filter(|label| !label.is_empty());
        Some(Self {
            state,
            label,
            thread_id: Some(thread_id),
            turn_id,
        })
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.is_ascii()
        && !value.chars().any(char::is_control)
}

/// Shared only with synchronous job-control polling on the foreground TUI loop.
/// Failed output disables this reporter; there is no teardown writer or retry queue.
#[derive(Default)]
pub(crate) struct Reporter {
    last: Option<Snapshot>,
    failed: bool,
    tmux: Option<bool>,
}

impl Reporter {
    pub(crate) fn current(&self) -> Option<Snapshot> {
        self.last.clone()
    }

    pub(crate) fn end_for_handoff(&mut self, output: &mut impl Write) -> Option<Snapshot> {
        let snapshot = self.current();
        if let Err(error) = self.report(None, output) {
            tracing::debug!(%error, "terminal status handoff failed");
        }
        snapshot
    }

    pub(crate) fn report(
        &mut self,
        snapshot: Option<Snapshot>,
        output: &mut impl Write,
    ) -> io::Result<()> {
        if self.failed || self.last == snapshot {
            return Ok(());
        }
        let op = if snapshot.is_none() {
            "end"
        } else if self.last.is_none() {
            "begin"
        } else {
            "update"
        };
        #[derive(Serialize)]
        struct Packet<'a> {
            op: &'a str,
            #[serde(flatten)]
            snapshot: Option<&'a Snapshot>,
        }
        let json = serde_json::to_vec(&Packet {
            op,
            snapshot: snapshot.as_ref(),
        })?;
        if json.len() > 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "terminal status exceeds 1024 bytes",
            ));
        }
        let payload = STANDARD.encode(json);
        let tmux = *self.tmux.get_or_insert_with(|| {
            matches!(terminal_info().multiplexer, Some(Multiplexer::Tmux { .. }))
        });
        let frame = if tmux {
            format!("\x1bPtmux;\x1b\x1b]777;rustty-agent;1;{payload}\x07\x1b\\")
        } else {
            format!("\x1b]777;rustty-agent;1;{payload}\x07")
        };
        if let Err(error) = output
            .write_all(frame.as_bytes())
            .and_then(|()| output.flush())
        {
            self.failed = true;
            return Err(error);
        }
        self.last = snapshot;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    fn snapshot(state: State, thread: &str, turn: Option<&str>) -> Snapshot {
        Snapshot::new(state, Some("Review Δ"), thread.to_owned(), turn).unwrap()
    }
    fn decode(bytes: &[u8]) -> Vec<serde_json::Value> {
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .split('\x07')
            .filter(|s| !s.is_empty())
            .map(|frame| {
                serde_json::from_slice(
                    &STANDARD
                        .decode(frame.strip_prefix("\x1b]777;rustty-agent;1;").unwrap())
                        .unwrap(),
                )
                .unwrap()
            })
            .collect()
    }
    #[test]
    fn ordered_lifecycle_coalesces_switches_and_restarts_after_suspend() {
        let mut reporter = Reporter {
            tmux: Some(false),
            ..Default::default()
        };
        let mut output = Vec::new();
        let a = snapshot(State::Done, "a", Some("t1"));
        reporter.report(Some(a.clone()), &mut output).unwrap();
        reporter.report(Some(a.clone()), &mut output).unwrap();
        reporter
            .report(Some(snapshot(State::Idle, "b", None)), &mut output)
            .unwrap();
        reporter.report(Some(a.clone()), &mut output).unwrap();
        let saved = reporter.end_for_handoff(&mut output);
        assert_eq!(saved.as_ref(), Some(&a));
        reporter.report(saved, &mut output).unwrap();
        reporter.report(None, &mut output).unwrap();
        let packets = decode(&output);
        assert_eq!(
            packets
                .iter()
                .map(|p| p["op"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["begin", "update", "update", "end", "begin", "end"]
        );
        assert_eq!(packets[0]["turn_id"], packets[2]["turn_id"]);
        assert_eq!(packets[0]["label"], "Review Δ");
        assert_eq!(packets[3], serde_json::json!({"op":"end"}));
    }
    #[test]
    fn bounded_metadata_and_all_states() {
        assert!(Snapshot::new(State::Done, None, "a".into(), None).is_none());
        for id in ["\x1b", "Δ", &"x".repeat(129)] {
            assert!(Snapshot::new(State::Idle, None, id.into(), None).is_none());
        }
        let label = format!("\x1b\n{}", "👨‍👩‍👧‍👦".repeat(20));
        let mut reporter = Reporter {
            tmux: Some(false),
            ..Default::default()
        };
        let mut output = Vec::new();
        for state in [
            State::Idle,
            State::Working,
            State::NeedsInput,
            State::Done,
            State::Error,
            State::Paused,
            State::Unknown,
        ] {
            let s = Snapshot::new(state, Some(&label), "t".into(), Some("turn")).unwrap();
            assert!(s.label.as_ref().unwrap().len() <= 128);
            assert!(s.label.as_ref().unwrap().graphemes(true).all(|g| g == "👨‍👩‍👧‍👦"));
            reporter.report(Some(s), &mut output).unwrap();
        }
        for packet in decode(&output) {
            assert!(serde_json::to_vec(&packet).unwrap().len() <= 1024);
            assert_eq!(packet.get("turn_id").is_some(), packet["state"] == "done");
        }
    }
    #[test]
    fn tmux_wraps_whole_frame_and_failures_do_not_retry() {
        let mut reporter = Reporter {
            tmux: Some(true),
            ..Default::default()
        };
        let mut output = Vec::new();
        reporter
            .report(Some(snapshot(State::Idle, "a", None)), &mut output)
            .unwrap();
        assert!(output.starts_with(b"\x1bPtmux;\x1b\x1b]777;rustty-agent;1;"));
        assert!(output.ends_with(b"\x07\x1b\\"));
        let mut failed = Reporter::default();
        assert!(
            failed
                .report(
                    Some(snapshot(State::Idle, "a", None)),
                    &mut &mut [0_u8; 0][..]
                )
                .is_err()
        );
        let mut retry = Vec::new();
        failed
            .report(Some(snapshot(State::Working, "a", None)), &mut retry)
            .unwrap();
        assert!(retry.is_empty());
    }
}
