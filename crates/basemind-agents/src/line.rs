// Adapted from https://github.com/hardbeat920/monocode (crates/monocode-core/src/{provider.rs,provider/parked.rs,transcript.rs}) at 09d04436babb39ad72198cdf22f735067027fc04, MIT.
//! The loop shared by the two harnesses that speak newline-delimited JSON on stdio. The
//! per-harness state machines implement [`Protocol`] and never touch a process, so they are
//! tested on recorded frames alone.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::LaunchError;
use crate::event::{ApprovalSource, Control, Event};
use crate::frames::FrameReader;
use crate::session::{ChildIo, ControlTx, Driver, EventSink};

const TICK: Duration = Duration::from_millis(16);

/// What a frame did to the turn in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Continue,
    TurnDone,
}

/// One harness's wire protocol as a state machine over parsed frames.
pub(crate) trait Protocol: Send + 'static {
    /// Opens the conversation. The brief is sent once the harness is ready for it.
    fn start(&mut self, out: &mut dyn Write) -> Result<(), LaunchError>;
    /// Handles one frame from the harness.
    fn frame(&mut self, frame: Value, out: &mut dyn Write, sink: &EventSink) -> Result<Step, LaunchError>;
    /// Answers a pending approval request; `false` when `id` is not pending.
    fn answer(&mut self, id: &str, allow: bool, out: &mut dyn Write) -> Result<bool, LaunchError>;
    /// Starts the next turn with `text`.
    fn prompt(&mut self, text: &str, out: &mut dyn Write, sink: &EventSink) -> Result<(), LaunchError>;
    /// Cancels the turn in progress through the harness's own protocol.
    fn interrupt(&mut self, out: &mut dyn Write) -> Result<(), LaunchError>;
    /// Runs on every loop pass, for work that depends on elapsed time.
    fn tick(&mut self, _elapsed: Duration, _out: &mut dyn Write, _sink: &EventSink) -> Result<(), LaunchError> {
        Ok(())
    }
}

pub(crate) fn write_json(out: &mut dyn Write, value: &Value) -> Result<(), LaunchError> {
    let mut frame = serde_json::to_vec(value).map_err(|error| LaunchError::Protocol(error.to_string()))?;
    frame.push(b'\n');
    out.write_all(&frame)?;
    out.flush()?;
    Ok(())
}

/// The part of `snapshot` not yet emitted, given what was `already` streamed.
fn snapshot_remainder<'a>(already: &str, snapshot: &'a str) -> &'a str {
    if let Some(suffix) = snapshot.strip_prefix(already) {
        suffix
    } else if already.starts_with(snapshot) {
        ""
    } else {
        snapshot
    }
}

/// Assistant text arrives twice from both harnesses: as deltas, then as a completed snapshot.
/// This emits each piece once, keyed by the message it belongs to.
#[derive(Default)]
pub(crate) struct TextStream {
    emitted: HashMap<String, String>,
}

impl TextStream {
    /// Emits the new part of `text` for message `key`; returns whether anything was emitted.
    pub fn push(&mut self, key: &str, text: &str, snapshot: bool, sink: &EventSink) -> bool {
        let already = self.emitted.entry(key.to_owned()).or_default();
        let text = if snapshot {
            snapshot_remainder(already, text)
        } else {
            text
        };
        if text.is_empty() {
            return false;
        }
        already.push_str(text);
        sink.emit(Event::Text(text.to_owned()));
        true
    }
}

/// Runs a [`Protocol`] over the pipes of a harness.
pub(crate) struct LineDriver<P> {
    protocol: P,
    controls: mpsc::Receiver<Control>,
}

impl<P: Protocol> LineDriver<P> {
    /// The driver for `protocol` and the sender its controls arrive through.
    pub fn boxed(protocol: P) -> (Box<dyn Driver>, ControlTx) {
        let (tx, controls) = mpsc::channel();
        (
            Box::new(Self { protocol, controls }),
            Box::new(move |control| tx.send(control).is_ok()),
        )
    }
}

impl<P: Protocol> Driver for LineDriver<P> {
    fn run(self: Box<Self>, io: ChildIo, sink: EventSink) -> Result<(), LaunchError> {
        let Self { mut protocol, controls } = *self;
        let mut out = io.stdin;
        let stdout = io.stdout;
        let (tx, lines) = mpsc::sync_channel(16);
        std::thread::spawn(move || {
            let mut reader = FrameReader::new(stdout);
            loop {
                match reader.next_frame() {
                    Ok(None) => break,
                    Ok(Some(frame)) => {
                        if tx.send(Ok(frame)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        break;
                    }
                }
            }
        });

        protocol.start(&mut out)?;
        let started = Instant::now();
        let mut queued = VecDeque::new();
        let mut idle = false;
        loop {
            while let Ok(control) = controls.try_recv() {
                match control {
                    Control::Answer { id, allow } => {
                        if protocol.answer(&id, allow, &mut out)? {
                            sink.resolved(id, allow, ApprovalSource::Caller);
                        } else {
                            sink.emit(Event::Error(format!("no approval request {id:?} is pending")));
                        }
                    }
                    Control::FollowUp(text) => queued.push_back(text),
                    Control::Stop => {
                        // The harness may already be gone; the supervisor ends it either way.
                        let _ = protocol.interrupt(&mut out);
                        return Ok(());
                    }
                }
            }
            if idle && let Some(text) = queued.pop_front() {
                protocol.prompt(&text, &mut out, &sink)?;
                idle = false;
            }
            protocol.tick(started.elapsed(), &mut out, &sink)?;
            let line = match lines.recv_timeout(TICK) {
                Ok(Ok(line)) => line,
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::InvalidData => {
                    return Err(LaunchError::Protocol(error.to_string()));
                }
                Ok(Err(error)) => return Err(error.into()),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) if idle => return Ok(()),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(LaunchError::Protocol(
                        "the harness closed its output before the turn ended".to_owned(),
                    ));
                }
            };
            let Some(frame) = parse_line(&line)? else {
                continue;
            };
            if protocol.frame(frame, &mut out, &sink)? == Step::TurnDone {
                let turn = sink.turns();
                sink.emit(Event::TurnCompleted { turn });
                if turn >= sink.max_turns() {
                    return Ok(());
                }
                idle = true;
            }
        }
    }
}

/// Parses one output line. Blank lines and startup banners are skipped; a line that starts
/// like a JSON object and does not parse, such as a frame cut off by a crash, is an error.
pub(crate) fn parse_line(line: &[u8]) -> Result<Option<Value>, LaunchError> {
    match serde_json::from_slice(line) {
        Ok(value) => Ok(Some(value)),
        Err(error) => {
            let first = line.iter().copied().find(|byte| !byte.is_ascii_whitespace());
            if first == Some(b'{') {
                Err(LaunchError::Protocol(format!("invalid JSON from the harness: {error}")))
            } else {
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banners_and_blank_lines_are_skipped() {
        assert_eq!(parse_line(b"\n").unwrap(), None);
        assert_eq!(parse_line(b"Loading configuration...\n").unwrap(), None);
        assert_eq!(parse_line(b"  \t\n").unwrap(), None);
    }

    #[test]
    fn a_truncated_or_malformed_object_is_an_error() {
        for line in [
            &b"{\"type\":\"assistant\",\"mess"[..],
            b"{\"type\": }\n",
            b"  {not json}\n",
        ] {
            let error = parse_line(line).unwrap_err();
            assert!(matches!(error, LaunchError::Protocol(_)), "{error}");
        }
    }

    #[test]
    fn a_complete_frame_parses() {
        let frame = parse_line(b"{\"type\":\"result\"}\n").unwrap().unwrap();
        assert_eq!(frame["type"], "result");
    }

    #[test]
    fn a_snapshot_after_deltas_emits_only_what_is_new() {
        let (sink, events) = EventSink::for_test(None, 1);
        let mut text = TextStream::default();
        assert!(text.push("1", "Hel", false, &sink));
        assert!(text.push("1", "lo", false, &sink));
        assert!(!text.push("1", "Hello", true, &sink));
        assert!(text.push("1", "Hello, world", true, &sink));
        assert!(text.push("2", "Hello", true, &sink));
        let texts: Vec<_> = events.try_iter().collect();
        assert_eq!(
            texts,
            ["Hel", "lo", ", world", "Hello"].map(|text| Event::Text(text.to_owned()))
        );
    }
}
