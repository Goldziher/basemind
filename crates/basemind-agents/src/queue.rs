//! The bounded queue between a session's adapter and its caller.
//!
//! A caller that reads slowly, or not at all, must not make the launcher buffer whatever the
//! harness prints. When the queue is full, increments are merged and progress reports of one
//! tool call replace each other; any other event waits for room, which in turn stops the
//! adapter reading the harness. Once the session is ending nothing waits: an event that does
//! not fit is dropped, and only the final `Error` and `Finished` go past the bound.

use std::collections::VecDeque;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::event::Event;

/// How many events may wait for the caller.
pub(crate) const EVENT_CAPACITY: usize = 1024;
/// How many bytes of text the waiting events may hold, roughly.
pub(crate) const EVENT_BYTES: usize = 16 * 1024 * 1024;
/// The largest increment two merged increments may make.
const MERGED_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct State {
    events: VecDeque<Event>,
    bytes: usize,
    /// The caller is gone: every event is dropped.
    closed: bool,
    /// The session is ending: nothing waits for room any more.
    draining: bool,
    /// `Finished` is queued, so nothing more is.
    finished: bool,
    /// `Finished` has been handed to the caller.
    delivered: bool,
}

impl State {
    fn full(&self) -> bool {
        !self.events.is_empty() && (self.events.len() >= EVENT_CAPACITY || self.bytes >= EVENT_BYTES)
    }

    fn push(&mut self, event: Event) {
        self.bytes += size(&event);
        self.events.push_back(event);
    }
}

/// The rough size of `event` in bytes, for the byte bound.
fn size(event: &Event) -> usize {
    const BASE: usize = 64;
    BASE + match event {
        Event::HarnessSession(text)
        | Event::Text(text)
        | Event::Reasoning(text)
        | Event::Status(text)
        | Event::Error(text)
        | Event::Diagnostics(text) => text.len(),
        Event::ToolCall(call) => call.id.len() + call.name.len() + call.input.len() + call.output.len(),
        Event::ApprovalRequest(request) => {
            request.title.len() + request.detail.len() + crate::session::json_len(&request.params)
        }
        _ => 0,
    }
}

/// The queue. The adapter side pushes, the session handle receives.
#[derive(Default)]
pub(crate) struct EventQueue {
    state: Mutex<State>,
    ready: Condvar,
    room: Condvar,
}

impl EventQueue {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queues `event`, merging, waiting or dropping as the module documentation describes.
    pub fn push(&self, event: Event) {
        let mut state = self.lock();
        let mut event = event;
        loop {
            if state.closed || state.finished {
                return;
            }
            if !state.full() {
                state.push(event);
                self.ready.notify_one();
                return;
            }
            event = match merge(&mut state, event) {
                None => return,
                Some(event) => event,
            };
            if state.draining {
                return;
            }
            state = self.room.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Queues one of the session's last events (`Diagnostics`, `Error`, `Finished` and the
    /// requests withdrawn at the end) past the bound.
    pub fn push_last(&self, event: Event) {
        let mut state = self.lock();
        if state.closed || state.finished {
            return;
        }
        state.finished = matches!(event, Event::Finished(_));
        state.push(event);
        self.ready.notify_all();
    }

    /// The session is ending: from now on nothing waits for room.
    pub fn drain(&self) {
        self.lock().draining = true;
        self.room.notify_all();
    }

    /// The caller is gone: drop what is queued and everything that follows.
    pub fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        state.draining = true;
        state.events.clear();
        state.bytes = 0;
        self.room.notify_all();
    }

    /// The next event, waiting until `deadline` (forever without one).
    pub fn recv_until(&self, deadline: Option<Instant>) -> Result<Event, RecvTimeoutError> {
        let mut state = self.lock();
        loop {
            if let Some(event) = state.events.pop_front() {
                state.bytes -= size(&event);
                state.delivered |= matches!(event, Event::Finished(_));
                self.room.notify_one();
                return Ok(event);
            }
            if state.delivered || state.closed {
                return Err(RecvTimeoutError::Disconnected);
            }
            state = match deadline {
                None => self
                    .ready
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(RecvTimeoutError::Timeout);
                    }
                    self.ready
                        .wait_timeout(state, left)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0
                }
            };
        }
    }

    /// The next event, waiting at most `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Event, RecvTimeoutError> {
        self.recv_until(Instant::now().checked_add(timeout))
    }

    /// Every event queued now, without waiting.
    #[cfg(test)]
    pub fn try_iter(&self) -> impl Iterator<Item = Event> {
        let mut state = self.lock();
        state.bytes = 0;
        let events: Vec<_> = state.events.drain(..).collect();
        state.delivered |= events.iter().any(|event| matches!(event, Event::Finished(_)));
        self.room.notify_all();
        events.into_iter()
    }
}

/// Folds `event` into the full queue when it can: an increment into the increment before
/// it, a tool call's report over an earlier report of the same call. Returns the event when
/// it could not.
fn merge(state: &mut State, event: Event) -> Option<Event> {
    let State { events, bytes, .. } = state;
    if let Event::ToolCall(call) = &event {
        let earlier = events
            .iter_mut()
            .rev()
            .find(|queued| matches!(queued, Event::ToolCall(queued) if queued.id == call.id));
        let Some(slot) = earlier else {
            return Some(event);
        };
        *bytes = *bytes + size(&event) - size(slot);
        *slot = event;
        return None;
    }
    match (events.back_mut(), &event) {
        (Some(Event::Text(queued)), Event::Text(text)) | (Some(Event::Reasoning(queued)), Event::Reasoning(text))
            if queued.len() + text.len() <= MERGED_BYTES =>
        {
            queued.push_str(text);
            *bytes += text.len();
            None
        }
        _ => Some(event),
    }
}

#[cfg(test)]
#[path = "queue_tests.rs"]
mod tests;
