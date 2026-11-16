//! What the relay does for the host while no backend can be reached: how long it holds a request,
//! and what it answers from this binary once the wait has gone on too long.

use super::*;

/// An `initialize` as a host sends it.
fn host_initialize(id: u64) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"2025-03-26\",\
         \"capabilities\":{{}},\"clientInfo\":{{\"name\":\"host\",\"version\":\"1\"}}}}}}\n"
    )
}

fn method_line(id: u64, method: &str) -> String {
    format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"{method}\"}}\n")
}

fn notification_line(method: &str) -> String {
    format!("{{\"jsonrpc\":\"2.0\",\"method\":\"{method}\"}}\n")
}

async fn wait_for_attempts(session: &Session, more_than: usize) {
    while session.attempt_count() <= more_than {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// A first attempt that takes longer than the hold window and then succeeds: a cold daemon spawn,
/// a takeover, a handshake queued behind other sessions after a restart. Nothing has failed, so
/// `initialize` must reach the daemon and the host must see no error.
#[tokio::test(start_paused = true)]
async fn a_slow_first_attempt_that_succeeds_forwards_initialize_and_fails_nothing() {
    const SLOW: Duration = Duration::from_secs(25);
    let (reachable, mut backend) = backend();
    let steps = vec![Step::After(SLOW, Box::new(reachable))];
    let mut session = Session::start(steps, (ReconnectFailure::Fatal, "unreachable step"));
    session.send(host_initialize(1)).await;

    assert_eq!(backend_reads(&mut backend).await, host_initialize(1));
    backend
        .write_all(response_line(1).as_bytes())
        .await
        .expect("initialize response");
    let answer = session.read().await;
    assert_eq!(answer["id"], serde_json::json!(1));
    assert!(answer.get("result").is_some(), "no error may reach the host: {answer}");
    assert!(session.elapsed() >= SLOW, "{:?}", session.elapsed());
    assert_eq!(session.attempt_count(), 1);
}

/// The same for a request in flight when the daemon was replaced: the reconnect that takes longer
/// than the hold window and succeeds replays it, as ADR-0013 says.
#[tokio::test(start_paused = true)]
async fn a_request_in_flight_is_replayed_by_a_slow_reconnect_that_succeeds() {
    const SLOW: Duration = Duration::from_secs(25);
    let (first, mut backend_one) = backend();
    let (second, mut backend_two) = backend();
    let steps = vec![first, Step::After(SLOW, Box::new(second))];
    let mut session = Session::start(steps, (ReconnectFailure::Fatal, "unreachable step"));
    session.send(request_line(4)).await;
    let in_flight = backend_reads(&mut backend_one).await;
    drop(backend_one);

    assert_eq!(backend_reads(&mut backend_two).await, in_flight);
    backend_two.write_all(response_line(4).as_bytes()).await.expect("reply");
    let answer = session.read().await;
    assert_eq!(answer["id"], serde_json::json!(4));
    assert!(answer.get("result").is_some(), "{answer}");
}

/// An attempt that never returns must not hold a request for good. It has a bound of its own,
/// longer than the hold window, and what was waiting is then told the daemon is not there yet.
#[tokio::test(start_paused = true)]
async fn a_request_waits_out_a_hung_attempt_for_the_attempts_own_bound() {
    let mut session = Session::start(vec![Step::Hang], (ReconnectFailure::Fatal, "unreachable step"));
    session.send(request_line(1)).await;

    let refused = session.read().await;
    assert_eq!(refused["id"], serde_json::json!(1));
    assert_eq!(refused["error"]["code"], BACKEND_UNAVAILABLE_CODE, "{refused}");
    assert_eq!(refused["error"]["data"]["retryable"], true);
    assert!(
        refused["error"]["message"]
            .as_str()
            .expect("message")
            .contains(NOT_REACHED_YET),
        "{refused}"
    );
    let waited = session.elapsed();
    assert!(
        waited >= ATTEMPT_HOLD_LIMIT && waited < ATTEMPT_HOLD_LIMIT + Duration::from_secs(1),
        "{waited:?}"
    );
    assert_eq!(session.attempt_count(), 1, "the first attempt is still the only one");

    session.send(request_line(2)).await;
    let next = session.read().await;
    assert_eq!(next["id"], serde_json::json!(2));
    assert!(
        session.elapsed() < ATTEMPT_HOLD_LIMIT + Duration::from_secs(1),
        "once the bound has passed a request is answered as it arrives"
    );
}

/// A request whose hold window ran out while an attempt was in flight is answered as soon as that
/// attempt fails, with the reason it failed for.
#[tokio::test(start_paused = true)]
async fn a_window_that_ran_out_during_an_attempt_ends_when_the_attempt_fails() {
    const SLOW: Duration = Duration::from_secs(25);
    let steps = vec![Step::After(
        SLOW,
        Box::new(Step::Fail(ReconnectFailure::Blocked, WONT_STOP)),
    )];
    let mut session = Session::start(steps, (ReconnectFailure::Blocked, WONT_STOP));
    session.send(request_line(1)).await;

    let refused = session.read().await;
    assert_eq!(refused["id"], serde_json::json!(1));
    assert!(
        refused["error"]["message"]
            .as_str()
            .expect("message")
            .contains(WONT_STOP),
        "{refused}"
    );
    let waited = session.elapsed();
    assert!(waited >= SLOW && waited < SLOW + Duration::from_secs(1), "{waited:?}");
}

/// The back-off before the next attempt can end on the very tick the hold window does. Whichever
/// timer the relay sees first, the window is over: what waits is answered then, not held for the
/// whole bound of an attempt started on that tick.
#[tokio::test(start_paused = true)]
async fn a_window_ending_with_the_back_off_is_not_stretched_by_the_next_attempt() {
    // `select!` breaks a tie at random, so the tie is set up often enough to meet both orders. ~keep
    const TIES: usize = 16;
    for _ in 0..TIES {
        let first = Step::After(
            HOLD_WINDOW - INITIAL_RECONNECT_DELAY,
            Box::new(Step::Fail(ReconnectFailure::Transient, REFUSED)),
        );
        let mut session = Session::start(vec![first, Step::Hang], (ReconnectFailure::Fatal, "unreachable step"));
        session.send(request_line(1)).await;

        let refused = session.read().await;
        assert_eq!(refused["id"], serde_json::json!(1));
        assert_eq!(refused["error"]["code"], BACKEND_UNAVAILABLE_CODE, "{refused}");
        let waited = session.elapsed();
        assert!(
            waited >= HOLD_WINDOW && waited < HOLD_WINDOW + Duration::from_secs(1),
            "{waited:?}"
        );
        assert_eq!(session.attempt_count(), 2);
    }
}

/// The upgrade case. Another version's daemon holds the workspace when the session starts and
/// stays past the hold window. The host sends `initialize` once, as hosts do. It must get a
/// result, so the server is up and its tools are listed; a tool call says who is in the way; and
/// when the daemon is reachable the session is carried over to it without the host seeing a second
/// answer to anything.
#[tokio::test(start_paused = true)]
async fn a_session_started_while_blocked_is_usable_says_why_and_moves_to_the_daemon_when_it_can() {
    const BLOCKED_ATTEMPTS: usize = 8;
    let (reachable, mut backend) = backend();
    let mut steps = blocked(BLOCKED_ATTEMPTS, WONT_STOP);
    steps.push(reachable);
    let mut session = Session::start(steps, (ReconnectFailure::Fatal, "unreachable step"));
    session.send(host_initialize(1)).await;

    let initialized = session.read().await;
    assert_eq!(initialized["id"], serde_json::json!(1));
    assert_eq!(
        initialized["result"]["protocolVersion"], "2025-03-26",
        "initialize is answered, not refused: {initialized}"
    );
    assert_eq!(
        initialized["result"]["capabilities"]["tools"]["listChanged"], true,
        "{initialized}"
    );
    assert!(session.elapsed() >= HOLD_WINDOW, "{:?}", session.elapsed());
    session.send(notification_line("notifications/initialized")).await;

    session.send(method_line(2, "tools/list")).await;
    let listed = session.read().await;
    assert_eq!(listed["id"], serde_json::json!(2));
    assert!(
        listed["result"]["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "code_outline")),
        "the tools are listed from this binary: {listed}"
    );

    session.send(call_line(3, "code_outline", "")).await;
    let refused = session.read().await;
    assert_eq!(refused["id"], serde_json::json!(3));
    assert_eq!(refused["error"]["code"], BACKEND_UNAVAILABLE_CODE, "{refused}");
    assert_eq!(refused["error"]["data"]["retryable"], true);
    assert!(
        refused["error"]["message"]
            .as_str()
            .expect("message")
            .contains(WONT_STOP),
        "a tool call says what is in the way: {refused}"
    );

    session.send(method_line(4, "ping")).await;
    let pong = session.read().await;
    assert_eq!(pong["id"], serde_json::json!(4));
    assert!(pong.get("result").is_some(), "the server answers a ping itself: {pong}");

    wait_for_attempts(&session, BLOCKED_ATTEMPTS).await;
    assert_eq!(
        backend_reads(&mut backend).await,
        host_initialize(1),
        "the daemon is initialized with what the host sent"
    );
    backend
        .write_all(response_line(1).as_bytes())
        .await
        .expect("initialize response");
    assert_eq!(
        backend_reads(&mut backend).await,
        notification_line("notifications/initialized")
    );
    let changed = session.read().await;
    assert_eq!(
        changed,
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }),
        "the host is told to list the tools again, and initialize is not answered twice"
    );

    session.send(call_line(5, "code_outline", "")).await;
    assert_eq!(backend_reads(&mut backend).await, call_line(5, "code_outline", ""));
    backend.write_all(response_line(5).as_bytes()).await.expect("reply");
    let answer = session.read().await;
    assert_eq!(answer["id"], serde_json::json!(5));
    assert!(answer.get("result").is_some(), "{answer}");
}

/// `initialize` reached a daemon that went before answering, and no daemon came back in time.
/// The host gets one answer, from this binary; the daemon that arrives later is initialized with
/// the same request and its answer goes nowhere.
#[tokio::test(start_paused = true)]
async fn an_initialize_in_flight_when_the_daemon_went_is_answered_once() {
    const BLOCKED_ATTEMPTS: usize = 8;
    let (first, mut backend_one) = backend();
    let (second, mut backend_two) = backend();
    let mut steps = vec![first];
    steps.extend(blocked(BLOCKED_ATTEMPTS, WONT_STOP));
    steps.push(second);
    let mut session = Session::start(steps, (ReconnectFailure::Fatal, "unreachable step"));
    session.send(host_initialize(1)).await;
    backend_reads(&mut backend_one).await;
    drop(backend_one);

    let initialized = session.read().await;
    assert_eq!(initialized["id"], serde_json::json!(1));
    assert!(initialized.get("result").is_some(), "{initialized}");

    wait_for_attempts(&session, BLOCKED_ATTEMPTS + 1).await;
    assert_eq!(backend_reads(&mut backend_two).await, host_initialize(1));
    backend_two
        .write_all(response_line(1).as_bytes())
        .await
        .expect("initialize response");
    session.send(notification_line("notifications/initialized")).await;
    assert_eq!(
        backend_reads(&mut backend_two).await,
        notification_line("notifications/initialized")
    );
    let next = session.read().await;
    assert_eq!(
        next["method"], "notifications/tools/list_changed",
        "the daemon's answer to initialize is not passed on: {next}"
    );
}

/// A session a daemon initialized did not promise the host that its tool list can change, so the
/// relay does not answer `tools/list` for it: the request is refused like any other.
#[tokio::test(start_paused = true)]
async fn the_tool_list_is_answered_from_this_binary_only_for_a_session_it_initialized() {
    let mut session = Session::start(Vec::new(), (ReconnectFailure::Blocked, WONT_STOP));
    session.send(method_line(1, "tools/list")).await;
    let refused = session.read().await;
    assert_eq!(refused["error"]["code"], BACKEND_UNAVAILABLE_CODE, "{refused}");
}

/// A notification that arrives with no backend is kept for the hold window like a request, not
/// for good: one that waited longer is not delivered to a daemon that arrives later.
#[tokio::test(start_paused = true)]
async fn a_notification_is_not_kept_past_the_hold_window() {
    const BLOCKED_ATTEMPTS: usize = 8;
    let (reachable, mut backend) = backend();
    let mut steps = blocked(BLOCKED_ATTEMPTS, WONT_STOP);
    steps.push(reachable);
    let mut session = Session::start(steps, (ReconnectFailure::Fatal, "unreachable step"));
    session
        .send(notification_line("notifications/roots/list_changed"))
        .await;
    tokio::time::sleep(HOLD_WINDOW + Duration::from_secs(1)).await;
    wait_for_attempts(&session, BLOCKED_ATTEMPTS).await;
    session.send(call_line(7, "code_outline", "")).await;
    assert_eq!(
        backend_reads(&mut backend).await,
        call_line(7, "code_outline", ""),
        "the stale notification was dropped"
    );
}

/// A second `initialize` starts the session over. The `initialized` that followed the first one
/// says nothing about the second, and must not be replayed after it.
#[test]
fn a_new_initialize_forgets_the_initialized_that_followed_the_old_one() {
    let mut state = SessionState::default();
    state.observe_client(&host_initialize(1));
    state.observe_backend(&response_line(1));
    state.observe_client(&notification_line("notifications/initialized"));
    assert!(state.initialized.is_some() && state.initialize_complete);

    state.observe_client(&host_initialize(2));
    assert_eq!(state.initialized, None);
    assert!(!state.initialize_complete, "the new initialize is not answered yet");
}
