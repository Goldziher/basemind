use super::tests::{
    OTHER_REPO, REPO, actor, actors, ids, inbox_request, message, new_thread, post, query, read, repo, start, thread,
    thread_ids, update,
};
use super::*;

#[test]
fn no_arguments_lists_the_callers_threads_and_their_peers() {
    let fake = FakeAgents::new();
    start(&fake, "a", "topic", &["b"]);
    start(&fake, "c", "elsewhere", &[]);

    let listed = fake.threads_query(query("a", REPO)).expect("threads");

    assert_eq!(
        listed,
        ThreadsReply {
            threads: vec![ThreadSummary {
                id: thread("t-aaaaaa"),
                subject: "topic".to_string(),
                repo: repo(REPO),
                paths: Vec::new(),
                open: false,
                archived: false,
                creator: actor("a"),
                member: true,
                unread: 0,
                head: 1,
                last_activity_micros: FAKE_EPOCH_MICROS + 1,
                peers: actors(&["b"]),
            }],
            pending: Vec::new(),
            next_cursor: None,
            notice: None,
        }
    );
}

#[test]
fn summary_counts_unread_messages_from_others() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);
    post(&fake, "b", &created.thread, "mine").expect("post");
    post(&fake, "a", &created.thread, "third").expect("post");

    assert_eq!(
        fake.threads_query(query("b", REPO)).expect("threads").threads[0].unread,
        2
    );
    read(&fake, "b");
    assert_eq!(
        fake.threads_query(query("b", REPO)).expect("threads").threads[0].unread,
        0
    );
}

#[test]
fn query_discovers_open_threads_of_the_callers_repository_only() {
    let fake = FakeAgents::new();
    start(&fake, "a", "private migration", &["b"]);
    start(&fake, "c", "Store Migration", &[]);
    fake.post(new_thread("c", OTHER_REPO, "foreign migration", &[], &[], "first"))
        .expect("thread created");
    let mut request = query("d", REPO);
    request.query = Some("MIGR".to_string());

    let found = fake.threads_query(request.clone()).expect("threads");

    assert_eq!(thread_ids(&found), ["t-aaaaab"]);
    assert!(!found.threads[0].member);
    assert!(found.threads[0].open);
    assert_eq!(found.threads[0].peers, actors(&["c"]));

    request.query = Some("mi".to_string());
    assert_eq!(fake.threads_query(request), Err(CommsError::QueryTooShort { min: 3 }));
}

#[test]
fn query_also_filters_the_callers_own_threads() {
    let fake = FakeAgents::new();
    start(&fake, "a", "private migration", &["b"]);
    start(&fake, "a", "release notes", &["b"]);
    let mut request = query("b", REPO);
    request.query = Some("migration".to_string());

    assert_eq!(thread_ids(&fake.threads_query(request).expect("threads")), ["t-aaaaaa"]);
}

#[test]
fn path_discovers_threads_about_a_prefix_or_something_under_it() {
    let fake = FakeAgents::new();
    fake.post(new_thread("c", REPO, "comms", &[], &["src/comms/"], "first"))
        .expect("thread created");
    fake.post(new_thread("c", REPO, "docs", &[], &["docs"], "first"))
        .expect("thread created");
    let found = |path: &str| {
        let mut request = query("d", REPO);
        request.path = Some(RelPath::from(path));
        fake.threads_query(request)
            .expect("threads")
            .threads
            .into_iter()
            .map(|summary| summary.id.to_string())
            .collect::<Vec<_>>()
    };

    assert_eq!(found("src/comms/store.rs"), ["t-aaaaaa"]);
    assert_eq!(found("src"), ["t-aaaaaa"]);
    assert_eq!(found("src/comms2"), Vec::<String>::new());
    assert_eq!(found("docs/adr"), ["t-aaaaab"]);
}

#[test]
fn thread_paths_are_stored_normalised_and_bad_ones_are_refused() {
    let fake = FakeAgents::new();
    fake.post(new_thread(
        "a",
        REPO,
        "comms",
        &[],
        &["src/comms/", "src/comms", "docs"],
        "first",
    ))
    .expect("thread created");

    let listed = fake.threads_query(query("a", REPO)).expect("threads");
    assert_eq!(
        listed.threads[0].paths,
        [RelPath::from("docs"), RelPath::from("src/comms")]
    );

    assert_eq!(
        fake.post(new_thread("a", REPO, "escape", &[], &["../other"], "first")),
        Err(CommsError::InvalidPath {
            path: "../other".to_string(),
        })
    );
    assert_eq!(fake.thread_count(), 1);
}

#[test]
fn describing_one_thread_needs_membership_or_an_open_thread_here() {
    let fake = FakeAgents::new();
    let private = start(&fake, "a", "private", &["b"]);
    let open = start(&fake, "a", "public", &[]);
    let describe = |who: &str, in_repo: &str, id: &ThreadId| {
        let mut request = query(who, in_repo);
        request.thread = Some(id.clone());
        fake.threads_query(request).map(|reply| reply.threads[0].id.clone())
    };

    assert_eq!(describe("b", OTHER_REPO, &private.thread), Ok(private.thread.clone()));
    assert_eq!(describe("d", REPO, &open.thread), Ok(open.thread.clone()));
    assert_eq!(
        describe("d", REPO, &private.thread),
        Err(CommsError::ThreadClosed { thread: private.thread })
    );
    assert_eq!(
        describe("d", OTHER_REPO, &open.thread),
        Err(CommsError::ThreadClosed { thread: open.thread })
    );
    assert_eq!(
        describe("d", REPO, &thread("t-zzzzzz")),
        Err(CommsError::ThreadNotFound {
            thread: thread("t-zzzzzz"),
        })
    );
}

#[test]
fn archived_threads_are_listed_only_on_request() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);
    update(&fake, "a", REPO, &created.thread, ThreadAction::Archive).expect("archive");
    let mut request = query("b", REPO);

    assert_eq!(
        fake.threads_query(request.clone()).expect("threads").threads,
        Vec::<ThreadSummary>::new()
    );
    request.archived = true;
    assert_eq!(thread_ids(&fake.threads_query(request).expect("threads")), ["t-aaaaaa"]);
}

#[test]
fn thread_listing_pages_by_cursor() {
    let fake = FakeAgents::new();
    for subject in ["one", "two", "three"] {
        start(&fake, "a", subject, &[]);
    }
    let mut request = query("a", REPO);
    request.limit = Some(2);

    let first = fake.threads_query(request.clone()).expect("threads");
    request.cursor = first.next_cursor.clone();
    let second = fake.threads_query(request.clone()).expect("threads");

    assert_eq!(thread_ids(&first), ["t-aaaaaa", "t-aaaaab"]);
    assert_eq!(first.next_cursor, Some("t-aaaaab".to_string()));
    assert_eq!(thread_ids(&second), ["t-aaaaac"]);
    assert_eq!(second.next_cursor, None);

    request.cursor = Some("7".to_string());
    assert_eq!(
        fake.threads_query(request),
        Err(CommsError::InvalidCursor {
            cursor: "7".to_string(),
        })
    );
}

#[test]
fn active_threads_are_capped_per_repository() {
    let fake = FakeAgents::new();
    for _ in 0..MAX_ACTIVE_THREADS_PER_REPO {
        start(&fake, "a", "topic", &[]);
    }

    assert_eq!(
        fake.post(new_thread("a", REPO, "one too many", &[], &[], "first")),
        Err(CommsError::ThreadCapReached {
            repo: repo(REPO),
            cap: 256,
        })
    );
    let elsewhere = fake
        .post(new_thread("a", OTHER_REPO, "topic", &[], &[], "first"))
        .expect("thread created");
    assert_eq!(elsewhere.thread, ThreadId::from_bits(256));

    update(&fake, "a", REPO, &thread("t-aaaaaa"), ThreadAction::Archive).expect("archive");
    assert_eq!(start(&fake, "a", "room again", &[]).thread, ThreadId::from_bits(257));
}

#[test]
fn oldest_messages_are_dropped_past_the_per_thread_cap() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);
    for _ in 0..MAX_MESSAGES_PER_THREAD {
        post(&fake, "a", &created.thread, "more").expect("post");
    }
    let lookup = |id: &str| {
        let mut request = inbox_request("b");
        request.select = InboxSelect::Message(message(id));
        fake.inbox(request).map(|reply| reply.messages[0].id.to_string())
    };

    assert_eq!(
        lookup("t-aaaaaa.1"),
        Err(CommsError::MessageNotFound {
            id: message("t-aaaaaa.1"),
        })
    );
    assert_eq!(lookup("t-aaaaaa.2"), Ok("t-aaaaaa.2".to_string()));
    assert_eq!(lookup("t-aaaaaa.1001"), Ok("t-aaaaaa.1001".to_string()));
}

#[test]
fn thread_is_removed_when_its_last_message_expires() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);

    fake.advance(MESSAGE_TTL - Duration::from_secs(1));
    assert_eq!(
        thread_ids(&fake.threads_query(query("a", REPO)).expect("threads")),
        ["t-aaaaaa"]
    );

    fake.advance(Duration::from_secs(2));
    assert_eq!(
        fake.threads_query(query("a", REPO)).expect("threads").threads,
        Vec::<ThreadSummary>::new()
    );
    assert_eq!(fake.thread_count(), 0);
    assert_eq!(read(&fake, "b").messages, Vec::<InboxMessage>::new());
    assert_eq!(
        post(&fake, "a", &created.thread, "late"),
        Err(CommsError::ThreadNotFound { thread: created.thread })
    );
}

#[test]
fn store_newer_than_the_binary_is_refused_for_writing_only() {
    let fake = FakeAgents::new();
    let created = start(&fake, "a", "topic", &["b"]);
    fake.set_store_version(COMMS_STORE_VER + 1);
    let newer = CommsError::StoreNewer {
        store: 1001,
        binary: 1000,
    };

    assert_eq!(post(&fake, "a", &created.thread, "hi"), Err(newer.clone()));
    assert_eq!(
        update(&fake, "c", REPO, &created.thread, ThreadAction::Join),
        Err(newer.clone())
    );
    assert_eq!(fake.inbox(inbox_request("b")).map(|reply| reply.messages), Err(newer));

    let mut peek = inbox_request("b");
    peek.mark = Mark::Peek;
    assert_eq!(ids(&fake.inbox(peek).expect("inbox")), ["t-aaaaaa.1"]);
    assert_eq!(
        thread_ids(&fake.threads_query(query("b", REPO)).expect("threads")),
        ["t-aaaaaa"]
    );
}
