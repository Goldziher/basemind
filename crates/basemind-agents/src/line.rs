// Adapted from https://github.com/hardbeat920/monocode (crates/monocode-core/src/{provider.rs,provider/parked.rs,transcript.rs}) at 09d04436babb39ad72198cdf22f735067027fc04, MIT.
//! The loop shared by the two harnesses that speak newline-delimited JSON on stdio. The
//! per-harness state machines implement [`Protocol`] and never touch a process, so they are
//! tested on recorded frames alone.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::mpsc::{self, RecvTimeoutError, TrySendError};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::LaunchError;
use crate::event::{ApprovalSource, Control, Event};
use crate::frames::FrameReader;
use crate::pipe;
use crate::session::{CONTROL_QUEUE, ChildIo, ControlTx, Driver, EventSink, Undelivered};

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
    /// Cancels the turn in progress through the harness's own protocol, denying and reporting
    /// withdrawn every request still pending.
    fn interrupt(&mut self, out: &mut dyn Write, sink: &EventSink) -> Result<(), LaunchError>;
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

/// How many messages a [`TextStream`] remembers; older ones are complete by then.
const TEXT_MESSAGES: usize = 8;
/// How much of one message's text a [`TextStream`] keeps to compare snapshots against.
const TEXT_KEPT_BYTES: usize = 1024 * 1024;

/// What was emitted for one message: its first bytes, and how long it is in all.
#[derive(Default)]
struct Emitted {
    kept: String,
    len: usize,
}

impl Emitted {
    /// The part of `snapshot` not yet emitted.
    fn remainder<'a>(&self, snapshot: &'a str) -> &'a str {
        if snapshot.starts_with(&self.kept) {
            if snapshot.len() >= self.len && snapshot.is_char_boundary(self.len) {
                &snapshot[self.len..]
            } else {
                // A snapshot no longer than what was streamed brings nothing new.
                ""
            }
        } else if self.kept.starts_with(snapshot) {
            ""
        } else {
            snapshot
        }
    }

    fn record(&mut self, text: &str) {
        if self.kept.len() == self.len {
            let mut room = TEXT_KEPT_BYTES.saturating_sub(self.kept.len()).min(text.len());
            while !text.is_char_boundary(room) {
                room -= 1;
            }
            self.kept.push_str(&text[..room]);
        }
        self.len += text.len();
    }
}

/// Assistant text arrives twice from both harnesses: as deltas, then as a completed snapshot.
/// This emits each piece once, keyed by the message it belongs to. It remembers the last
/// [`TEXT_MESSAGES`] messages and the first [`TEXT_KEPT_BYTES`] of each, so its memory is
/// bounded however long the session runs.
#[derive(Default)]
pub(crate) struct TextStream {
    emitted: VecDeque<(String, Emitted)>,
}

impl TextStream {
    /// Emits the new part of `text` for message `key`; returns whether anything was emitted.
    pub fn push(&mut self, key: &str, text: &str, snapshot: bool, sink: &EventSink) -> bool {
        let at = match self.emitted.iter().position(|(known, _)| known == key) {
            Some(at) => at,
            None => {
                if self.emitted.len() >= TEXT_MESSAGES {
                    self.emitted.pop_front();
                }
                self.emitted.push_back((key.to_owned(), Emitted::default()));
                self.emitted.len() - 1
            }
        };
        let already = &mut self.emitted[at].1;
        let text = if snapshot { already.remainder(text) } else { text };
        if text.is_empty() {
            return false;
        }
        already.record(text);
        sink.emit(Event::Text(text.to_owned()));
        true
    }
}

/// How many frames read from the harness may wait for the driver. Each may be 16 MiB.
const FRAME_QUEUE: usize = 2;
/// How many frames may wait for the harness to read them.
const OUTPUT_QUEUE: usize = 64;

/// The protocol's side of the harness's stdin: each flushed frame goes to a writer thread
/// through a bounded queue, so the driver never blocks on a harness that stopped reading.
struct QueuedWriter {
    frame: Vec<u8>,
    tx: mpsc::SyncSender<Vec<u8>>,
}

impl QueuedWriter {
    fn start(mut stdin: impl Write + Send + 'static) -> Self {
        let (tx, frames) = mpsc::sync_channel::<Vec<u8>>(OUTPUT_QUEUE);
        std::thread::spawn(move || {
            for frame in frames {
                if stdin.write_all(&frame).and_then(|()| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        Self { frame: Vec::new(), tx }
    }
}

impl Write for QueuedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.frame.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.frame.is_empty() {
            return Ok(());
        }
        match self.tx.try_send(std::mem::take(&mut self.frame)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(std::io::Error::other(format!(
                "the harness left {OUTPUT_QUEUE} frames unread"
            ))),
            Err(TrySendError::Disconnected(_)) => Err(std::io::ErrorKind::BrokenPipe.into()),
        }
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
        let (tx, controls) = mpsc::sync_channel(CONTROL_QUEUE);
        (
            Box::new(Self { protocol, controls }),
            Box::new(move |control| {
                tx.try_send(control).map_err(|error| match error {
                    TrySendError::Full(_) => Undelivered::Full,
                    TrySendError::Disconnected(_) => Undelivered::Closed,
                })
            }),
        )
    }
}

impl<P: Protocol> Driver for LineDriver<P> {
    fn run(self: Box<Self>, io: ChildIo, sink: EventSink) -> Result<(), LaunchError> {
        let Self { mut protocol, controls } = *self;
        let mut out = QueuedWriter::start(pipe::Writer::new(io.stdin, io.abandon.clone())?.for_stdin(io.stdin_state));
        let stdout = pipe::Reader::new(io.stdout, io.abandon);
        let (tx, lines) = mpsc::sync_channel(FRAME_QUEUE);
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
                        let _ = protocol.interrupt(&mut out, &sink);
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
    fn the_text_stream_remembers_a_bounded_amount() {
        let (sink, events) = EventSink::for_test(None, 1);
        let mut text = TextStream::default();
        for n in 0..100 {
            text.push(&n.to_string(), "hello", false, &sink);
        }
        assert_eq!(text.emitted.len(), TEXT_MESSAGES);
        let long = "y".repeat(3 * TEXT_KEPT_BYTES);
        for chunk in long.as_bytes().chunks(4096) {
            text.push("long", std::str::from_utf8(chunk).unwrap(), false, &sink);
        }
        let kept = &text.emitted.back().unwrap().1;
        assert_eq!(kept.kept.len(), TEXT_KEPT_BYTES);
        assert_eq!(kept.len, long.len());
        let _ = events.try_iter().count();
        // The completed snapshot still yields only what was not streamed.
        assert!(text.push("long", &format!("{long}!"), true, &sink));
        assert!(!text.push("long", &long, true, &sink));
        assert_eq!(events.try_iter().collect::<Vec<_>>(), [Event::Text("!".to_owned())]);
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
