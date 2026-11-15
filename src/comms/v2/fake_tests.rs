use std::sync::Arc;

use super::*;
use crate::comms::v2::api::MAX_INLINE_BODY_BYTES;

pub(super) const REPO: &str = "github.com/acme/app";
pub(super) const OTHER_REPO: &str = "path:/work/elsewhere";

pub(super) fn actor(s: &str) -> Actor {
    Actor::parse(s).expect("actor")
}

pub(super) fn actors(names: &[&str]) -> Vec<Actor> {
    names.iter().map(|name| actor(name)).collect()
}

pub(super) fn repo(s: &str) -> RepoKey {
    RepoKey::parse(s).expect("repo key")
}

pub(super) fn key(s: &str) -> ReplayKey {
    ReplayKey::parse(s).expect("replay key")
}

pub(super) fn thread(s: &str) -> ThreadId {
    ThreadId::parse(s).expect("thread id")
}

pub(super) fn message(s: &str) -> MessageId {
    MessageId::parse(s).expect("message id")
}

pub(super) fn new_thread(
    who: &str,
    in_repo: &str,
    subject: &str,
    to: &[&str],
    paths: &[&str],
    body: &str,
) -> PostRequest {
    PostRequest {
        actor: actor(who),
        repo: repo(in_repo),
        target: PostTarget::NewThread {
            subject: subject.to_string(),
            to: actors(to),
            paths: paths.iter().map(|path| RelPath::from(*path)).collect(),
        },
        body: body.to_string(),
        replay_key: None,
    }
}

/// Start a thread in [`REPO`] with the body `first`.
pub(super) fn start(fake: &FakeAgents, who: &str, subject: &str, to: &[&str]) -> PostReply {
    fake.post(new_thread(who, REPO, subject, to, &[], "first"))
        .expect("thread created")
}

pub(super) fn post_request(who: &str, target: PostTarget, body: &str) -> PostRequest {
    PostRequest {
        actor: actor(who),
        repo: repo(REPO),
        target,
        body: body.to_string(),
        replay_key: None,
    }
}

pub(super) fn post(fake: &FakeAgents, who: &str, to_thread: &ThreadId, body: &str) -> Result<PostReply, CommsError> {
    fake.post(post_request(who, PostTarget::Thread(to_thread.clone()), body))
}

pub(super) fn inbox_request(who: &str) -> InboxRequest {
    InboxRequest::new(actor(who), repo(REPO))
}

pub(super) fn read(fake: &FakeAgents, who: &str) -> InboxReply {
    fake.inbox(inbox_request(who)).expect("inbox")
}

pub(super) fn bodies(reply: &InboxReply) -> Vec<&str> {
    reply.messages.iter().map(|message| message.body.as_str()).collect()
}

pub(super) fn ids(reply: &InboxReply) -> Vec<String> {
    reply.messages.iter().map(|message| message.id.to_string()).collect()
}

/// The typed empty list a `read_to` that advanced nothing equals.
pub(super) fn no_messages() -> Vec<MessageId> {
    Vec::new()
}

pub(super) fn update(
    fake: &FakeAgents,
    who: &str,
    in_repo: &str,
    id: &ThreadId,
    action: ThreadAction,
) -> Result<ThreadsReply, CommsError> {
    fake.threads_update(ThreadUpdate {
        actor: actor(who),
        repo: repo(in_repo),
        thread: id.clone(),
        action,
    })
}

pub(super) fn query(who: &str, in_repo: &str) -> ThreadQuery {
    ThreadQuery::new(actor(who), repo(in_repo))
}

pub(super) fn thread_ids(reply: &ThreadsReply) -> Vec<&str> {
    reply.threads.iter().map(|summary| summary.id.as_str()).collect()
}

#[test]
fn creating_post_puts_creator_and_recipients_at_position_zero() {
    let fake = FakeAgents::new();
    let reply = start(&fake, "a", "topic", &["b"]);

    assert_eq!(
        reply,
        PostReply {
            id: message("t-aaaaaa.1"),
            thread: thread("t-aaaaaa"),
            created: true,
            pending: actors(&["b"]),
            notice: None,
        }
    );
    assert_eq!(fake.position(&reply.thread, &actor("a")), Some((0, 0)));
    assert_eq!(fake.position(&reply.thread, &actor("b")), Some((0, 0)));
    assert_eq!(fake.position(&reply.thread, &actor("c")), None);
}

#[test]
fn subject_is_not_a_key_so_two_posts_make_two_threads() {
    let fake = FakeAgents::new();
    let first = start(&fake, "a", "topic", &[]);
    let second = start(&fake, "a", "topic", &[]);

    assert_eq!(first.thread, thread("t-aaaaaa"));
    assert_eq!(second.thread, thread("t-aaaaab"));
    assert_eq!(fake.thread_count(), 2);
}

#[test]
fn colliding_thread_id_is_drawn_again() {
    let fake = FakeAgents::new();
    fake.queue_thread_bits([5, 5, 6]);

    assert_eq!(start(&fake, "a", "one", &[]).thread, thread("t-aaaaaf"));
    assert_eq!(start(&fake, "a", "two", &[]).thread, thread("t-aaaaag"));
}

#[test]
fn pending_lists_only_recipients_the_store_has_never_seen() {
    let fake = FakeAgents::new();
    read(&fake, "b");

    let reply = start(&fake, "a", "topic", &["b", "typo", "typo", "a"]);

    assert_eq!(reply.pending, actors(&["typo"]));
}

#[test]
fn recipient_that_never_connected_finds_the_thread_on_its_first_inbox() {
    let fake = FakeAgents::new();
    start(&fake, "a", "topic", &["b"]);

    assert_eq!(
        read(&fake, "b"),
        InboxReply {
            messages: vec![InboxMessage {
                id: message("t-aaaaaa.1"),
                subject: "topic".to_string(),
                from: actor("a"),
                ts_micros: FAKE_EPOCH_MICROS + 1,
                reply_to: None,
                body: "first".to_string(),
                truncated: false,
            }],
            read_to: vec![message("t-aaaaaa.1")],
            next_cursor: None,
            notice: None,
            wait: WaitOutcome::NotWaited,
        }
    );
}

#[test]
fn reading_advances_read_seq_to_the_last_message_returned() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);
    post(&fake, "a", &created.thread, "second").expect("post");

    let reply = read(&fake, "b");
    assert_eq!(bodies(&reply), ["first", "second"]);
    assert_eq!(reply.read_to, [message("t-aaaaaa.2")]);
    assert_eq!(fake.position(&created.thread, &actor("b")), Some((2, 2)));

    let again = read(&fake, "b");
    assert_eq!(again.messages, Vec::<InboxMessage>::new());
    assert_eq!(again.read_to, no_messages());
}

#[test]
fn peek_returns_messages_without_moving_any_position() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);
    let mut request = inbox_request("b");
    request.mark = Mark::Peek;

    let first = fake.inbox(request.clone()).expect("inbox");
    let second = fake.inbox(request).expect("inbox");

    assert_eq!(ids(&first), ["t-aaaaaa.1"]);
    assert_eq!(first.read_to, no_messages());
    assert_eq!(second, first);
    assert_eq!(fake.position(&created.thread, &actor("b")), Some((0, 0)));
}

#[test]
fn notify_announces_once_and_leaves_messages_unread() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);
    let mut request = inbox_request("b");
    request.mark = Mark::Notify;

    let announced = fake.inbox(request.clone()).expect("inbox");
    assert_eq!(ids(&announced), ["t-aaaaaa.1"]);
    assert_eq!(announced.read_to, no_messages());
    assert_eq!(fake.position(&created.thread, &actor("b")), Some((0, 1)));

    assert_eq!(fake.inbox(request).expect("inbox").messages, Vec::<InboxMessage>::new());
    assert_eq!(ids(&read(&fake, "b")), ["t-aaaaaa.1"]);
    assert_eq!(fake.position(&created.thread, &actor("b")), Some((1, 1)));
}

#[test]
fn own_posts_are_never_unread() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);

    assert_eq!(read(&fake, "a").messages, Vec::<InboxMessage>::new());
    assert_eq!(fake.position(&created.thread, &actor("a")), Some((0, 0)));
    let listed = fake.threads_query(query("a", REPO)).expect("threads");
    assert_eq!(listed.threads[0].unread, 0);
}

#[test]
fn history_read_returns_read_and_own_messages_oldest_first() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);
    post(&fake, "b", &created.thread, "second").expect("post");
    read(&fake, "a");
    let mut request = inbox_request("a");
    request.unread = false;

    let reply = fake.inbox(request).expect("inbox");

    assert_eq!(bodies(&reply), ["first", "second"]);
    assert_eq!(reply.read_to, no_messages());
    assert_eq!(fake.position(&created.thread, &actor("a")), Some((2, 2)));
}

#[test]
fn inbox_spans_threads_in_post_order() {
    let fake = FakeAgents::new();
    let one = start(&fake, "a", "one", &["b"]);
    let two = start(&fake, "a", "two", &["b"]);
    post(&fake, "a", &one.thread, "third").expect("post");

    let reply = read(&fake, "b");

    assert_eq!(ids(&reply), ["t-aaaaaa.1", "t-aaaaab.1", "t-aaaaaa.2"]);
    assert_eq!(reply.read_to, [message("t-aaaaaa.2"), message("t-aaaaab.1")]);
    assert_eq!(fake.position(&two.thread, &actor("b")), Some((1, 1)));
}

#[test]
fn thread_selection_reads_one_thread_only() {
    let fake = FakeAgents::new();
    let one = start(&fake, "a", "one", &["b"]);
    let two = start(&fake, "a", "two", &["b"]);
    let mut request = inbox_request("b");
    request.select = InboxSelect::Thread(two.thread.clone());

    assert_eq!(ids(&fake.inbox(request).expect("inbox")), ["t-aaaaab.1"]);
    assert_eq!(fake.position(&one.thread, &actor("b")), Some((0, 0)));
}

#[test]
fn reading_and_posting_require_membership() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &[]);
    let not_member = CommsError::NotMember {
        thread: created.thread.clone(),
    };

    assert_eq!(post(&fake, "c", &created.thread, "hi"), Err(not_member.clone()));

    let mut by_thread = inbox_request("c");
    by_thread.select = InboxSelect::Thread(created.thread.clone());
    assert_eq!(
        fake.inbox(by_thread).map(|reply| reply.messages),
        Err(not_member.clone())
    );

    let mut by_id = inbox_request("c");
    by_id.select = InboxSelect::Message(created.id.clone());
    assert_eq!(fake.inbox(by_id).map(|reply| reply.messages), Err(not_member.clone()));

    let reply_to = post_request("c", PostTarget::ReplyTo(created.id), "hi");
    assert_eq!(fake.post(reply_to), Err(not_member));
}

#[test]
fn sub_identity_is_a_different_member_than_its_principal() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);

    assert_eq!(
        post(&fake, "a.helper", &created.thread, "hi"),
        Err(CommsError::NotMember { thread: created.thread })
    );
}

#[test]
fn joining_an_open_thread_of_the_callers_repository_starts_at_the_head() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &[]);
    post(&fake, "a", &created.thread, "second").expect("post");

    let joined = update(&fake, "c", REPO, &created.thread, ThreadAction::Join).expect("join");

    assert!(joined.threads[0].member);
    assert_eq!(joined.threads[0].unread, 0);
    assert_eq!(fake.position(&created.thread, &actor("c")), Some((2, 2)));
    post(&fake, "a", &created.thread, "third").expect("post");
    assert_eq!(bodies(&read(&fake, "c")), ["third"]);
}

#[test]
fn joining_again_keeps_the_read_position() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);

    update(&fake, "b", REPO, &created.thread, ThreadAction::Join).expect("join");

    assert_eq!(fake.position(&created.thread, &actor("b")), Some((0, 0)));
}

#[test]
fn joining_is_refused_for_invitation_only_and_foreign_threads() {
    let fake = FakeAgents::new();
    let invitation_only = start(&fake, "a", "private", &["b"]);
    let open = start(&fake, "a", "public", &[]);

    assert_eq!(
        update(&fake, "c", REPO, &invitation_only.thread, ThreadAction::Join),
        Err(CommsError::ThreadClosed {
            thread: invitation_only.thread,
        })
    );
    assert_eq!(
        update(&fake, "c", OTHER_REPO, &open.thread, ThreadAction::Join),
        Err(CommsError::ThreadClosed { thread: open.thread })
    );
    assert_eq!(
        update(&fake, "c", REPO, &thread("t-zzzzzz"), ThreadAction::Join),
        Err(CommsError::ThreadNotFound {
            thread: thread("t-zzzzzz"),
        })
    );
}

#[test]
fn any_member_may_invite_and_the_invited_start_at_the_head_as_of_the_invitation() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);
    post(&fake, "a", &created.thread, "second").expect("post");

    let invited = update(
        &fake,
        "b",
        REPO,
        &created.thread,
        ThreadAction::Invite(actors(&["c", "b"])),
    )
    .expect("invite");

    assert_eq!(invited.pending, actors(&["c"]));
    assert_eq!(invited.threads[0].peers, actors(&["a", "c"]));
    assert_eq!(fake.position(&created.thread, &actor("c")), Some((2, 2)));
    assert_eq!(fake.position(&created.thread, &actor("b")), Some((0, 0)));
    post(&fake, "a", &created.thread, "third").expect("post");
    assert_eq!(bodies(&read(&fake, "c")), ["third"]);
}

#[test]
fn inviting_requires_membership_and_someone_to_invite() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);

    assert_eq!(
        update(&fake, "d", REPO, &created.thread, ThreadAction::Invite(actors(&["c"]))),
        Err(CommsError::NotMember {
            thread: created.thread.clone(),
        })
    );
    assert_eq!(
        update(&fake, "a", REPO, &created.thread, ThreadAction::Invite(Vec::new())),
        Err(CommsError::EmptyInvite)
    );
}

#[test]
fn leaving_ends_membership() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);
    let not_member = CommsError::NotMember {
        thread: created.thread.clone(),
    };

    let left = update(&fake, "b", REPO, &created.thread, ThreadAction::Leave).expect("leave");

    assert!(!left.threads[0].member);
    assert_eq!(fake.position(&created.thread, &actor("b")), None);
    assert_eq!(post(&fake, "b", &created.thread, "hi"), Err(not_member.clone()));
    assert_eq!(
        update(&fake, "b", REPO, &created.thread, ThreadAction::Leave),
        Err(not_member)
    );
}

#[test]
fn only_the_creator_may_archive() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);

    assert_eq!(
        update(&fake, "b", REPO, &created.thread, ThreadAction::Archive),
        Err(CommsError::NotCreator {
            thread: created.thread.clone(),
        })
    );
    let archived = update(&fake, "a", REPO, &created.thread, ThreadAction::Archive).expect("archive");
    assert!(archived.threads[0].archived);
    let again = update(&fake, "a", REPO, &created.thread, ThreadAction::Archive).expect("archive");
    assert!(again.threads[0].archived);
}

#[test]
fn archived_thread_is_readable_but_closed_to_posts_joins_and_invitations() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &[]);
    update(&fake, "b", REPO, &created.thread, ThreadAction::Join).expect("join");
    post(&fake, "a", &created.thread, "second").expect("post");
    update(&fake, "a", REPO, &created.thread, ThreadAction::Archive).expect("archive");
    let archived = CommsError::ThreadArchived {
        thread: created.thread.clone(),
    };

    assert_eq!(post(&fake, "a", &created.thread, "late"), Err(archived.clone()));
    assert_eq!(
        update(&fake, "c", REPO, &created.thread, ThreadAction::Join),
        Err(archived.clone())
    );
    assert_eq!(
        update(&fake, "a", REPO, &created.thread, ThreadAction::Invite(actors(&["c"]))),
        Err(archived)
    );
    assert_eq!(bodies(&read(&fake, "b")), ["second"]);
}

#[test]
fn long_body_is_returned_cut_and_counts_as_read() {
    let fake = FakeAgents::new();
    let long = "x".repeat(5000);
    let created = fake
        .post(new_thread("a", REPO, "topic", &["b"], &[], &long))
        .expect("thread created");

    let reply = read(&fake, "b");

    assert_eq!(reply.messages[0].body, "x".repeat(MAX_INLINE_BODY_BYTES));
    assert!(reply.messages[0].truncated);
    assert_eq!(fake.position(&created.thread, &actor("b")), Some((1, 1)));
}

#[test]
fn reading_by_id_returns_the_whole_body_and_moves_no_position() {
    let fake = FakeAgents::new();
    let long = "x".repeat(5000);
    let created = fake
        .post(new_thread("a", REPO, "topic", &["b"], &[], &long))
        .expect("thread created");
    let mut request = inbox_request("b");
    request.select = InboxSelect::Message(created.id.clone());

    let reply = fake.inbox(request).expect("inbox");

    assert_eq!(reply.messages[0].body, long);
    assert!(!reply.messages[0].truncated);
    assert_eq!(reply.read_to, no_messages());
    assert_eq!(fake.position(&created.thread, &actor("b")), Some((0, 0)));
}

#[test]
fn reading_an_unknown_message_id_is_refused() {
    let fake = FakeAgents::new();
    start(&fake, "a", "topic", &["b"]);

    for missing in ["t-aaaaaa.2", "t-zzzzzz.1"] {
        let mut request = inbox_request("a");
        request.select = InboxSelect::Message(message(missing));
        assert_eq!(
            fake.inbox(request).map(|reply| reply.messages),
            Err(CommsError::MessageNotFound { id: message(missing) }),
            "{missing}"
        );
    }
}

#[test]
fn message_left_out_by_the_response_budget_stays_unread() {
    let fake = FakeAgents::new();
    let full = "x".repeat(MAX_INLINE_BODY_BYTES);
    let created = fake
        .post(new_thread("a", REPO, "topic", &["b"], &[], &full))
        .expect("thread created");
    for _ in 0..4 {
        post(&fake, "a", &created.thread, &full).expect("post");
    }

    let first = read(&fake, "b");
    assert_eq!(ids(&first), ["t-aaaaaa.1", "t-aaaaaa.2", "t-aaaaaa.3", "t-aaaaaa.4"]);
    assert_eq!(first.next_cursor, Some("4".to_string()));
    assert_eq!(first.read_to, [message("t-aaaaaa.4")]);
    assert_eq!(fake.position(&created.thread, &actor("b")), Some((4, 4)));

    let second = read(&fake, "b");
    assert_eq!(ids(&second), ["t-aaaaaa.5"]);
    assert_eq!(second.next_cursor, None);
}

#[test]
fn limit_cuts_the_page_and_the_rest_stays_unread() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);
    post(&fake, "a", &created.thread, "second").expect("post");
    post(&fake, "a", &created.thread, "third").expect("post");
    let mut request = inbox_request("b");
    request.limit = Some(2);

    let first = fake.inbox(request).expect("inbox");

    assert_eq!(bodies(&first), ["first", "second"]);
    assert_eq!(first.next_cursor, Some("2".to_string()));
    assert_eq!(fake.position(&created.thread, &actor("b")), Some((2, 2)));
    assert_eq!(bodies(&read(&fake, "b")), ["third"]);
}

#[test]
fn cursor_pages_a_peek_without_marking() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);
    post(&fake, "a", &created.thread, "second").expect("post");
    let mut request = inbox_request("b");
    request.mark = Mark::Peek;
    request.limit = Some(1);

    let first = fake.inbox(request.clone()).expect("inbox");
    request.cursor = first.next_cursor.clone();
    let second = fake.inbox(request.clone()).expect("inbox");

    assert_eq!(first.next_cursor, Some("1".to_string()));
    assert_eq!(bodies(&second), ["second"]);
    assert_eq!(second.next_cursor, None);
    assert_eq!(fake.position(&created.thread, &actor("b")), Some((0, 0)));

    request.cursor = Some("not-a-cursor".to_string());
    assert_eq!(
        fake.inbox(request).map(|reply| reply.messages),
        Err(CommsError::InvalidCursor {
            cursor: "not-a-cursor".to_string(),
        })
    );
}

#[test]
fn replayed_post_into_a_thread_returns_the_first_reply() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);
    let mut request = post_request("a", PostTarget::Thread(created.thread.clone()), "second");
    request.replay_key = Some(key("relay-1"));

    let first = fake.post(request.clone()).expect("post");
    let replay = fake.post(request.clone()).expect("post");
    assert_eq!(first.id, message("t-aaaaaa.2"));
    assert_eq!(replay, first);

    let mut other_agent = post_request("b", PostTarget::Thread(created.thread.clone()), "mine");
    other_agent.replay_key = Some(key("relay-1"));
    assert_eq!(fake.post(other_agent).expect("post").id, message("t-aaaaaa.3"));

    fake.advance(REPLAY_TTL);
    assert_eq!(fake.post(request).expect("post").id, message("t-aaaaaa.4"));
}

#[test]
fn post_without_a_replay_key_always_makes_a_new_message() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);

    assert_eq!(
        post(&fake, "a", &created.thread, "same").expect("post").id,
        message("t-aaaaaa.2")
    );
    assert_eq!(
        post(&fake, "a", &created.thread, "same").expect("post").id,
        message("t-aaaaaa.3")
    );
}

#[test]
fn replayed_creating_post_returns_the_thread_already_created() {
    let fake = FakeAgents::new();
    let mut request = new_thread("a", REPO, "topic", &["b"], &[], "first");
    request.replay_key = Some(key("relay-1"));

    let first = fake.post(request.clone()).expect("post");
    let replay = fake.post(request).expect("post");

    assert_eq!(first.thread, thread("t-aaaaaa"));
    assert!(first.created);
    assert_eq!(replay, first);
    assert_eq!(fake.thread_count(), 1);
}

#[test]
fn replayed_inbox_read_returns_the_range_first_delivered() {
    let fake = FakeAgents::new();
    start(&fake, "a", "topic", &["b"]);
    let mut request = inbox_request("b");
    request.replay_key = Some(key("relay-1"));

    let first = fake.inbox(request.clone()).expect("inbox");
    let replay = fake.inbox(request.clone()).expect("inbox");

    assert_eq!(ids(&first), ["t-aaaaaa.1"]);
    assert_eq!(replay, first);
    assert_eq!(read(&fake, "b").messages, Vec::<InboxMessage>::new());

    fake.advance(REPLAY_TTL);
    assert_eq!(fake.inbox(request).expect("inbox").messages, Vec::<InboxMessage>::new());
}

#[test]
fn wait_times_out_when_nothing_arrives() {
    let fake = FakeAgents::new().with_wait_unit(Duration::from_millis(1));
    start(&fake, "a", "topic", &["b"]);
    read(&fake, "b");
    let mut request = inbox_request("b");

    assert_eq!(fake.inbox(request.clone()).expect("inbox").wait, WaitOutcome::NotWaited);
    request.wait_secs = 2;
    let reply = fake.inbox(request.clone()).expect("inbox");
    assert_eq!(reply.wait, WaitOutcome::TimedOut);
    assert_eq!(reply.messages, Vec::<InboxMessage>::new());

    request.wait_secs = 41;
    assert_eq!(
        fake.inbox(request).map(|reply| reply.wait),
        Err(CommsError::WaitOutOfRange { given: 41, max: 40 })
    );
}

#[test]
fn wait_returns_as_soon_as_a_message_arrives() {
    let fake = Arc::new(FakeAgents::new());
    let created = start(&fake, "a", "topic", &["b"]);
    read(&fake, "b");
    let waiter = {
        let fake = Arc::clone(&fake);
        std::thread::spawn(move || {
            let mut request = inbox_request("b");
            request.wait_secs = 40;
            fake.inbox(request)
        })
    };
    while fake.waiting() == 0 {
        std::thread::yield_now();
    }

    post(&fake, "a", &created.thread, "ping").expect("post");
    let reply = waiter.join().expect("waiter thread").expect("inbox");

    assert_eq!(reply.wait, WaitOutcome::Arrived);
    assert_eq!(bodies(&reply), ["ping"]);
    assert_eq!(fake.position(&created.thread, &actor("b")), Some((2, 2)));
}

#[test]
fn shared_identity_notice_rides_on_every_reply_of_the_principal() {
    let fake = FakeAgents::new();
    fake.set_live_sessions("a", 2);
    let notice = Some(shared_identity_notice("a", 2));

    let created = start(&fake, "a", "topic", &["a.helper", "b"]);
    assert_eq!(created.notice, notice);
    assert_eq!(read(&fake, "a.helper").notice, notice);
    assert_eq!(fake.threads_query(query("a", REPO)).expect("threads").notice, notice);
    assert_eq!(
        update(&fake, "a", REPO, &created.thread, ThreadAction::Invite(actors(&["c"])))
            .expect("invite")
            .notice,
        notice
    );
    assert_eq!(read(&fake, "b").notice, None);

    fake.set_live_sessions("a", 1);
    assert_eq!(read(&fake, "a").notice, None);
}

#[test]
fn reply_goes_into_the_thread_of_the_message_it_answers() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);

    let reply = fake
        .post(post_request("b", PostTarget::ReplyTo(created.id.clone()), "answer"))
        .expect("post");

    assert_eq!(reply.id, message("t-aaaaaa.2"));
    assert!(!reply.created);
    let inbox = read(&fake, "a");
    assert_eq!(bodies(&inbox), ["answer"]);
    assert_eq!(inbox.messages[0].reply_to, Some(created.id));
    assert_eq!(
        fake.post(post_request("b", PostTarget::ReplyTo(message("t-aaaaaa.9")), "answer")),
        Err(CommsError::MessageNotFound {
            id: message("t-aaaaaa.9"),
        })
    );
}

#[test]
fn invalid_posts_are_refused_before_anything_is_stored() {
    let fake = FakeAgents::new();

    assert_eq!(
        fake.post(new_thread("a", REPO, "topic", &[], &[], "  ")),
        Err(CommsError::EmptyBody)
    );
    assert_eq!(
        fake.post(new_thread("a", REPO, "two\nlines", &[], &[], "first")),
        Err(CommsError::InvalidSubject { max: 200 })
    );
    assert_eq!(fake.thread_count(), 0);
}
