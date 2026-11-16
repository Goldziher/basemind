use std::sync::Arc;

use super::*;
use crate::event::{FinishStatus, ToolCall};

fn fill(queue: &EventQueue) {
    assert!(queue.try_iter().next().is_none());
    for n in 0..EVENT_CAPACITY {
        queue.push(Event::Status(format!("status {n}")));
    }
}

fn tool(id: &str, output: &str) -> Event {
    let mut call = ToolCall::new(id, "Bash");
    call.output = output.to_owned();
    Event::ToolCall(call)
}

/// Pushes `event` on another thread and reports whether the push returned within `within`.
fn push_returns(queue: &Arc<EventQueue>, event: Event, within: Duration) -> std::sync::mpsc::Receiver<()> {
    let (done, returned) = std::sync::mpsc::channel();
    let queue = queue.clone();
    std::thread::spawn(move || {
        queue.push(event);
        let _ = done.send(());
    });
    assert!(
        returned.recv_timeout(within).is_err(),
        "the push into a full queue returned at once"
    );
    returned
}

#[test]
fn a_full_queue_replaces_tool_reports_and_holds_the_rest() {
    let queue = Arc::new(EventQueue::default());
    for n in 0..EVENT_CAPACITY - 2 {
        queue.push(Event::Status(format!("status {n}")));
    }
    queue.push(Event::Text("Hel".to_owned()));
    queue.push(tool("t1", "a"));
    // Full: a later report of a queued call replaces it in place.
    queue.push(tool("t1", "ab"));
    // Text cannot merge into a tool report, so it waits for room.
    let blocked = push_returns(&queue, Event::Text("lo".to_owned()), Duration::from_millis(100));
    queue.recv_until(None).unwrap();
    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
    let events: Vec<_> = queue.try_iter().collect();
    assert_eq!(events.len(), EVENT_CAPACITY);
    assert_eq!(
        events[EVENT_CAPACITY - 3..],
        [
            Event::Text("Hel".to_owned()),
            tool("t1", "ab"),
            Event::Text("lo".to_owned())
        ]
    );
}

#[test]
fn text_merges_only_up_to_its_limit() {
    let queue = EventQueue::default();
    for n in 0..EVENT_CAPACITY - 1 {
        queue.push(Event::Status(format!("status {n}")));
    }
    queue.push(Event::Text("a".repeat(MERGED_BYTES - 1)));
    queue.push(Event::Text("b".to_owned()));
    let queue = Arc::new(queue);
    let blocked = push_returns(&queue, Event::Text("c".to_owned()), Duration::from_millis(100));
    queue.drain();
    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
    let last = queue.try_iter().last().unwrap();
    assert_eq!(last, Event::Text(format!("{}b", "a".repeat(MERGED_BYTES - 1))));
}

#[test]
fn a_waiting_push_is_released_and_dropped_once_the_session_drains() {
    let queue = Arc::new(EventQueue::default());
    fill(&queue);
    let blocked = push_returns(&queue, Event::Status("late".to_owned()), Duration::from_millis(100));
    queue.drain();
    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(!queue.try_iter().any(|event| event == Event::Status("late".to_owned())));
}

#[test]
fn the_byte_bound_holds_back_large_events() {
    let queue = Arc::new(EventQueue::default());
    let big = "s".repeat(EVENT_BYTES / 8);
    for _ in 0..8 {
        queue.push(Event::Status(big.clone()));
    }
    let blocked = push_returns(&queue, Event::Status(big), Duration::from_millis(100));
    queue.recv_until(None).unwrap();
    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
}

#[test]
fn the_last_events_pass_the_bound_and_end_the_stream() {
    let queue = EventQueue::default();
    fill(&queue);
    queue.push_last(Event::Error("boom".to_owned()));
    queue.push_last(Event::Finished(FinishStatus::Failed));
    queue.push(Event::Status("after".to_owned()));
    let mut events = Vec::new();
    while let Ok(event) = queue.recv_timeout(Duration::from_secs(1)) {
        events.push(event);
    }
    assert_eq!(events.len(), EVENT_CAPACITY + 2);
    assert_eq!(events[EVENT_CAPACITY + 1], Event::Finished(FinishStatus::Failed));
    assert_eq!(
        queue.recv_timeout(Duration::from_secs(1)),
        Err(RecvTimeoutError::Disconnected)
    );
}

#[test]
fn a_closed_queue_drops_everything() {
    let queue = EventQueue::default();
    fill(&queue);
    queue.close();
    queue.push(Event::Status("after close".to_owned()));
    queue.push_last(Event::Finished(FinishStatus::Stopped));
    assert_eq!(queue.try_iter().count(), 0);
    assert_eq!(queue.recv_timeout(Duration::ZERO), Err(RecvTimeoutError::Disconnected));
}
