//! When a call writes the caller's seen record.

use super::*;

#[test]
fn first_call_records_the_caller_and_later_calls_rewrite_the_record_once_a_day() {
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);
    let store = harness.store();
    assert_eq!(store.recorded_seen("a"), Some(START_MICROS));
    assert_eq!(store.recorded_seen("b"), None);

    harness.clock.advance(Duration::from_secs(60));
    harness.inbox(&caller("b"), peek()).expect("b's first call");
    let first_call = START_MICROS + 60_000_000;
    assert_eq!(store.recorded_seen("b"), Some(first_call));

    harness.clock.advance(SEEN_REFRESH_INTERVAL - Duration::from_secs(1));
    harness.inbox(&caller("b"), peek()).expect("inbox");
    harness.inbox(&caller("b"), InboxBody::default()).expect("inbox");
    say(&harness, "b", &created.thread, "second");
    assert_eq!(store.recorded_seen("b"), Some(first_call), "reads and changes alike");

    harness.clock.advance(Duration::from_secs(1));
    harness.inbox(&caller("b"), peek()).expect("inbox");
    assert_eq!(
        store.recorded_seen("b"),
        Some(first_call + rules::ttl_micros(SEEN_REFRESH_INTERVAL))
    );
    let rows = store.stored_rows();
    assert_eq!(rows[10], 2, "one seen row each");
    assert_eq!(rows[11], 2 + 1, "one expiry entry each, and the thread's");
}

#[test]
fn failed_call_and_a_call_on_a_newer_store_record_nothing() {
    let harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);
    let before = harness.store().stored_rows();

    harness
        .post(&caller("c"), into(&created.thread, "refused"))
        .expect_err("not a member");
    harness.store().force_store_version(COMMS_STORE_VER + 1);
    harness.inbox(&caller("b"), peek()).expect("inbox");

    assert_eq!(harness.store().stored_rows(), before);
}
