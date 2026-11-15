//! Threads: listing, discovery, membership, archiving and the per-repository cap.

use super::super::api::{
    Actor, CommsError, Cursor, InboxBody, MAX_ACTIVE_THREADS_PER_REPO, MAX_RECIPIENTS, MAX_THREAD_MEMBERS, Principal,
    ReplayKey, ThreadAction, ThreadFilter, ThreadId, ThreadQuery, ThreadSummary, cap_archive_notice,
};
use super::{
    ConformanceHarness, agent, agents, bodies, by_id, change, describe, elsewhere, find, gone_thread, history, into,
    list, new_thread, only, principals, read, read_as, say, start, thread_ids, tick, who,
};
use crate::path::RelPath;

fn by_query(query: &str) -> ThreadFilter {
    ThreadFilter {
        query: Some(query.to_string()),
        ..ThreadFilter::default()
    }
}

fn by_path(path: &str) -> ThreadFilter {
    ThreadFilter {
        path: Some(RelPath::from(path)),
        ..ThreadFilter::default()
    }
}

fn with_archived() -> ThreadFilter {
    ThreadFilter {
        archived: true,
        ..ThreadFilter::default()
    }
}

fn no_threads() -> Vec<ThreadId> {
    Vec::new()
}

/// The tool with no arguments: the caller's threads, each with who else is in it.
pub fn no_arguments_lists_the_callers_threads_and_their_peers(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    start(h, "c", "elsewhere", &[]);

    let listed = list(h, "a");

    assert_eq!(listed.threads.len(), 1);
    assert_eq!(listed.pending, Vec::<Actor>::new());
    assert_eq!(listed.next_cursor, None);
    assert_eq!(listed.notices, Vec::<String>::new());
    let summary = &listed.threads[0];
    assert_eq!(summary.id, created.thread);
    assert_eq!(summary.subject, "topic");
    assert_eq!(summary.repo, who(h, "a").repo);
    assert_eq!(summary.paths, Vec::<RelPath>::new());
    assert!(!summary.open);
    assert!(!summary.archived);
    assert_eq!(summary.creator, agent(h, "a"));
    assert!(summary.member);
    assert_eq!(summary.unread, 0);
    assert_eq!(summary.head, 1);
    assert_eq!(summary.peers, principals(h, &["b"]));

    let inbox = read(h, "b");
    assert_eq!(summary.last_activity_micros, inbox.messages[0].ts_micros);
}

/// `unread` counts the messages of others past the read position.
pub fn summary_counts_unread_messages_from_others(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    say(h, "b", &created.thread, "mine").expect("post");
    say(h, "a", &created.thread, "third").expect("post");

    assert_eq!(list(h, "b").threads[0].unread, 2);
    read(h, "b");
    assert_eq!(list(h, "b").threads[0].unread, 0);
}

/// A query reaches open threads of the caller's repository: not invitation-only ones, not
/// another repository's, and not with fewer than three characters.
pub fn query_discovers_open_threads_of_the_callers_repository_only(h: &impl ConformanceHarness) {
    start(h, "a", "private migration", &["b"]);
    let open = start(h, "c", "Store Migration", &[]);
    h.post(
        &elsewhere(h, "c"),
        new_thread(h, "foreign migration", &[], &[], "first"),
    )
    .expect("thread created");

    let found = find(h, &who(h, "d"), by_query("MIGR")).expect("threads");

    assert_eq!(thread_ids(&found), [open.thread]);
    assert!(!found.threads[0].member);
    assert!(found.threads[0].open);
    assert_eq!(found.threads[0].unread, 0);
    assert_eq!(found.threads[0].peers, principals(h, &["c"]));
    assert!(matches!(
        find(h, &who(h, "d"), by_query("mi")),
        Err(CommsError::QueryTooShort { .. })
    ));
}

/// A filter narrows the caller's own threads too, not only what it discovers.
pub fn query_also_filters_the_callers_own_threads(h: &impl ConformanceHarness) {
    let migration = start(h, "a", "private migration", &["b"]);
    start(h, "a", "release notes", &["b"]);

    let found = find(h, &who(h, "b"), by_query("migration")).expect("threads");

    assert_eq!(thread_ids(&found), [migration.thread]);
}

/// A path selects a thread about that path, about something above it, or about something under
/// it, by whole components.
pub fn path_discovers_threads_about_a_prefix_or_something_under_it(h: &impl ConformanceHarness) {
    let c = who(h, "c");
    let comms = h
        .post(&c, new_thread(h, "comms", &[], &["src/comms/"], "first"))
        .expect("thread created");
    let docs = h
        .post(&c, new_thread(h, "docs", &[], &["docs"], "first"))
        .expect("thread created");
    let found = |path: &str| thread_ids(&find(h, &who(h, "d"), by_path(path)).expect("threads"));

    assert_eq!(found("src/comms/store.rs"), std::slice::from_ref(&comms.thread));
    assert_eq!(found("src"), [comms.thread]);
    assert_eq!(found("src/comms2"), no_threads());
    assert_eq!(found("docs/adr"), [docs.thread]);
}

/// A path filter narrows the caller's own threads as well.
pub fn path_also_filters_the_callers_own_threads(h: &impl ConformanceHarness) {
    let a = who(h, "a");
    let comms = h
        .post(&a, new_thread(h, "comms", &["b"], &["src/comms"], "first"))
        .expect("thread created");
    h.post(&a, new_thread(h, "docs", &["b"], &["docs"], "first"))
        .expect("thread created");
    h.post(&a, new_thread(h, "anything", &["b"], &[], "first"))
        .expect("thread created");

    let found = find(h, &who(h, "b"), by_path("src/comms/api.rs")).expect("threads");

    assert_eq!(thread_ids(&found), [comms.thread]);
}

/// `query` and `path` together narrow: a thread must match both.
pub fn query_and_path_together_narrow(h: &impl ConformanceHarness) {
    let c = who(h, "c");
    let both = h
        .post(&c, new_thread(h, "comms migration", &[], &["src/comms"], "first"))
        .expect("thread created");
    h.post(&c, new_thread(h, "docs migration", &[], &["docs"], "first"))
        .expect("thread created");
    h.post(&c, new_thread(h, "comms cleanup", &[], &["src/comms"], "first"))
        .expect("thread created");

    let found = find(
        h,
        &who(h, "d"),
        ThreadFilter {
            path: Some(RelPath::from("src/comms")),
            ..by_query("migration")
        },
    )
    .expect("threads");

    assert_eq!(thread_ids(&found), [both.thread]);
}

/// Paths are stored without trailing slashes or duplicates, and one that leaves the repository
/// is refused.
pub fn thread_paths_are_stored_normalised_and_bad_ones_are_refused(h: &impl ConformanceHarness) {
    let a = who(h, "a");
    h.post(
        &a,
        new_thread(h, "comms", &[], &["src/comms/", "src/comms", "docs"], "first"),
    )
    .expect("thread created");

    assert_eq!(
        list(h, "a").threads[0].paths,
        [RelPath::from("docs"), RelPath::from("src/comms")]
    );

    let refused = h
        .post(&a, new_thread(h, "escape", &[], &["../other"], "first"))
        .expect_err("a path outside the repository");
    assert!(matches!(refused, CommsError::InvalidPath { .. }), "{refused:?}");
    assert!(refused.to_string().contains("../other"), "{refused}");
    assert_eq!(h.thread_count(), 1);
}

/// One thread is described to its members, wherever they are, and to anyone in its repository
/// while it is open: subject and peers, no messages.
pub fn describing_one_thread_needs_membership_or_an_open_thread_here(h: &impl ConformanceHarness) {
    let gone = gone_thread(h);
    let private = start(h, "a", "private", &["b"]);
    let open = start(h, "a", "public", &[]);
    let id_of = |described: Result<ThreadSummary, CommsError>| described.map(|summary| summary.id);

    assert_eq!(
        id_of(describe(h, &elsewhere(h, "b"), &private.thread)),
        Ok(private.thread.clone())
    );
    let seen_from_outside = describe(h, &who(h, "d"), &open.thread).expect("an open thread of this repository");
    assert_eq!(seen_from_outside.subject, "public");
    assert_eq!(seen_from_outside.peers, principals(h, &["a"]));
    assert!(!seen_from_outside.member);
    assert_eq!(seen_from_outside.unread, 0);
    assert_eq!(
        id_of(describe(h, &who(h, "d"), &private.thread)),
        Err(CommsError::ThreadClosed { thread: private.thread })
    );
    assert_eq!(
        id_of(describe(h, &elsewhere(h, "d"), &open.thread)),
        Err(CommsError::ThreadClosed { thread: open.thread })
    );
    assert_eq!(
        id_of(describe(h, &who(h, "d"), &gone.thread)),
        Err(CommsError::ThreadNotFound { thread: gone.thread })
    );
}

/// An archived thread leaves the default listing and comes back with `archived`.
pub fn archived_threads_are_listed_only_on_request(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    change(h, &who(h, "a"), &created.thread, ThreadAction::Archive).expect("archive");

    assert_eq!(thread_ids(&list(h, "b")), no_threads());
    assert_eq!(
        thread_ids(&find(h, &who(h, "b"), with_archived()).expect("threads")),
        [created.thread]
    );
}

/// A listing is ordered by the store-wide position of each thread's newest post, newest first,
/// whatever the clock read: a post moves its thread to the front.
pub fn listing_is_ordered_by_the_newest_post_newest_first(h: &impl ConformanceHarness) {
    let one = start(h, "a", "one", &[]);
    tick(h);
    let two = start(h, "a", "two", &[]);
    h.rewind(std::time::Duration::from_secs(60));
    let three = start(h, "a", "three", &[]);

    let listed = list(h, "a");
    assert_eq!(
        thread_ids(&listed),
        [three.thread.clone(), two.thread.clone(), one.thread.clone()]
    );
    assert!(listed.threads[0].last_activity_micros < listed.threads[1].last_activity_micros);

    say(h, "a", &one.thread, "again").expect("post");
    assert_eq!(thread_ids(&list(h, "a")), [one.thread, three.thread, two.thread]);
}

/// A listing pages by an opaque cursor that only continues the listing that issued it.
pub fn thread_listing_pages_by_cursor(h: &impl ConformanceHarness) {
    let mut created = Vec::new();
    for subject in ["one", "two", "three"] {
        created.push(start(h, "a", subject, &["b"]).thread);
        tick(h);
    }
    let page = |name: &str, cursor: Option<Cursor>, archived: bool| {
        find(
            h,
            &who(h, name),
            ThreadFilter {
                limit: Some(2),
                cursor,
                archived,
                ..ThreadFilter::default()
            },
        )
    };

    let first = page("a", None, false).expect("threads");
    assert_eq!(thread_ids(&first), [created[2].clone(), created[1].clone()]);
    let cursor = first.next_cursor.expect("a cursor when threads were left out");
    let second = page("a", Some(cursor.clone()), false).expect("threads");
    assert_eq!(thread_ids(&second), [created[0].clone()]);
    assert_eq!(second.next_cursor, None);

    for refused in [
        page("b", Some(cursor.clone()), false),
        page("a", Some(cursor), true),
        page("a", Some(Cursor::from_wire("7")), false),
    ] {
        let error = refused.expect_err("a cursor of another listing");
        assert!(matches!(error, CommsError::InvalidCursor { .. }), "{error:?}");
    }
}

/// Joining needs no invitation in an open thread of the caller's repository, and starts at the
/// head: what was said before is history, not unread.
pub fn joining_an_open_thread_of_the_callers_repository_starts_at_the_head(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &[]);
    say(h, "a", &created.thread, "second").expect("post");

    let joined = change(h, &who(h, "c"), &created.thread, ThreadAction::Join).expect("join");

    assert!(joined.threads[0].member);
    assert_eq!(joined.threads[0].unread, 0);
    assert_eq!(h.position(&created.thread, &agent(h, "c")), Some((2, 2)));
    assert_eq!(h.position(&created.thread, &agent(h, "c.helper")), Some((2, 2)));
    say(h, "a", &created.thread, "third").expect("post");
    assert_eq!(bodies(&read(h, "c")), ["third"]);
}

/// Joining a thread one is already in changes nothing.
pub fn joining_again_keeps_the_read_position(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);

    change(h, &who(h, "b"), &created.thread, ThreadAction::Join).expect("join");
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((0, 0)));

    change(h, &who(h, "a"), &created.thread, ThreadAction::Archive).expect("archive");
    let still_in = change(h, &who(h, "b"), &created.thread, ThreadAction::Join).expect("a member joining again");
    assert!(still_in.threads[0].member);
}

/// An invitation-only thread, and an open thread of another repository, cannot be joined.
pub fn joining_is_refused_for_invitation_only_and_foreign_threads(h: &impl ConformanceHarness) {
    let gone = gone_thread(h);
    let invitation_only = start(h, "a", "private", &["b"]);
    let open = start(h, "a", "public", &[]);

    assert_eq!(
        change(h, &who(h, "c"), &invitation_only.thread, ThreadAction::Join),
        Err(CommsError::ThreadClosed {
            thread: invitation_only.thread,
        })
    );
    assert_eq!(
        change(h, &elsewhere(h, "c"), &open.thread, ThreadAction::Join),
        Err(CommsError::ThreadClosed { thread: open.thread })
    );
    assert_eq!(
        change(h, &who(h, "c"), &gone.thread, ThreadAction::Join),
        Err(CommsError::ThreadNotFound { thread: gone.thread })
    );
}

/// Any member invites. The invited read from the head as of the invitation, and a member that
/// is invited again keeps its position.
pub fn any_member_may_invite_and_the_invited_start_at_the_head(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    say(h, "a", &created.thread, "second").expect("post");

    let invited = change(
        h,
        &who(h, "b"),
        &created.thread,
        ThreadAction::Invite(agents(h, &["c", "b"])),
    )
    .expect("invite");

    assert_eq!(invited.pending, agents(h, &["c"]));
    assert_eq!(invited.threads[0].peers, principals(h, &["a", "c"]));
    assert_eq!(h.position(&created.thread, &agent(h, "c")), Some((2, 2)));
    assert_eq!(h.position(&created.thread, &agent(h, "b")), Some((0, 0)));
    say(h, "a", &created.thread, "third").expect("post");
    assert_eq!(bodies(&read(h, "c")), ["third"]);
}

/// An outsider cannot invite, and an invitation must name someone.
pub fn inviting_requires_membership_and_someone_to_invite(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);

    assert_eq!(
        change(
            h,
            &who(h, "d"),
            &created.thread,
            ThreadAction::Invite(agents(h, &["c"]))
        ),
        Err(CommsError::NotMember {
            thread: created.thread.clone(),
        })
    );
    assert_eq!(
        change(h, &who(h, "a"), &created.thread, ThreadAction::Invite(Vec::new())),
        Err(CommsError::EmptyInvite)
    );
}

/// Membership crosses repositories by invitation only: the invited agent reads, posts and lists
/// the thread from its own repository, while its neighbours there can neither join nor see it.
pub fn invitation_crosses_repositories(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &[]);
    let (x, y) = (elsewhere(h, "x"), elsewhere(h, "y"));
    let closed = CommsError::ThreadClosed {
        thread: created.thread.clone(),
    };

    assert_eq!(change(h, &y, &created.thread, ThreadAction::Join), Err(closed.clone()));
    assert_eq!(describe(h, &y, &created.thread).map(|summary| summary.id), Err(closed));

    change(
        h,
        &who(h, "a"),
        &created.thread,
        ThreadAction::Invite(vec![x.actor.clone()]),
    )
    .expect("invite");
    say(h, "a", &created.thread, "second").expect("post");

    let inbox = h.inbox(&x, InboxBody::default()).expect("inbox");
    assert_eq!(bodies(&inbox), ["second"]);
    h.post(&x, into(&created.thread, "from afar")).expect("post");
    assert_eq!(bodies(&read(h, "a")), ["from afar"]);

    let listed = h.threads_query(&x, ThreadQuery::default()).expect("threads");
    assert_eq!(thread_ids(&listed), [created.thread]);
    assert_eq!(listed.threads[0].repo, who(h, "a").repo);
    assert_ne!(listed.threads[0].repo, x.repo);
    let unseen = h.threads_query(&y, ThreadQuery::default()).expect("threads");
    assert_eq!(thread_ids(&unseen), no_threads());
}

/// Leaving ends the principal's membership, for all its actors: no posting, no second leave,
/// no position.
pub fn leaving_ends_membership(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    let not_member = CommsError::NotMember {
        thread: created.thread.clone(),
    };
    read(h, "b.helper");

    let left = change(h, &who(h, "b"), &created.thread, ThreadAction::Leave).expect("leave");
    assert_eq!(h.position(&created.thread, &agent(h, "b.helper")), None);

    assert!(!left.threads[0].member);
    assert!(!left.threads[0].archived);
    assert_eq!(h.position(&created.thread, &agent(h, "b")), None);
    assert_eq!(say(h, "b", &created.thread, "hi"), Err(not_member.clone()));
    assert_eq!(
        change(h, &who(h, "b"), &created.thread, ThreadAction::Leave),
        Err(not_member)
    );
}

/// Archiving is the creator's alone, and archiving twice is not an error.
pub fn only_the_creator_may_archive(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);

    assert_eq!(
        change(h, &who(h, "b"), &created.thread, ThreadAction::Archive),
        Err(CommsError::NotCreator {
            thread: created.thread.clone(),
        })
    );
    let archived = change(h, &who(h, "a"), &created.thread, ThreadAction::Archive).expect("archive");
    assert!(archived.threads[0].archived);
    let again = change(h, &who(h, "a"), &created.thread, ThreadAction::Archive).expect("archive");
    assert!(again.threads[0].archived);
}

/// The right to archive follows the creator, not its membership.
pub fn creator_who_left_may_still_archive(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);
    change(h, &who(h, "a"), &created.thread, ThreadAction::Leave).expect("leave");

    let archived = change(h, &who(h, "a"), &created.thread, ThreadAction::Archive).expect("archive");

    assert!(archived.threads[0].archived);
    assert!(!archived.threads[0].member);
    assert_eq!(
        say(h, "b", &created.thread, "late"),
        Err(CommsError::ThreadArchived { thread: created.thread })
    );
}

/// A thread nobody is in is archived the moment its last member leaves.
pub fn last_member_leaving_archives_the_thread(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &["b"]);

    let first = change(h, &who(h, "a"), &created.thread, ThreadAction::Leave).expect("leave");
    assert!(!first.threads[0].archived);
    assert_eq!(thread_ids(&list(h, "b")), std::slice::from_ref(&created.thread));

    let last = change(h, &who(h, "b"), &created.thread, ThreadAction::Leave).expect("leave");
    assert!(last.threads[0].archived);
    assert_eq!(last.threads[0].peers, Vec::<Principal>::new());

    let open = start(h, "c", "open", &[]);
    change(h, &who(h, "c"), &open.thread, ThreadAction::Leave).expect("leave");
    assert_eq!(
        change(h, &who(h, "d"), &open.thread, ThreadAction::Join),
        Err(CommsError::ThreadArchived { thread: open.thread })
    );
    assert_eq!(h.thread_count(), 2);
}

/// `join`, `invite` and `archive` made again find the change made and succeed; a relayed
/// `leave` returns its first reply when it is replayed, where a new `leave` is refused.
pub fn membership_changes_are_idempotent_under_replay(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &[]);
    let key = |text: &str| ReplayKey::parse(text).expect("replay key");
    let (a, c) = (who(h, "a"), who(h, "c"));

    let joined = change(
        h,
        &c.clone().with_replay_key(key("r1")),
        &created.thread,
        ThreadAction::Join,
    );
    assert_eq!(
        change(
            h,
            &c.clone().with_replay_key(key("r1")),
            &created.thread,
            ThreadAction::Join
        ),
        joined
    );
    let invite = || ThreadAction::Invite(agents(h, &["d"]));
    let invited = change(h, &a.clone().with_replay_key(key("r2")), &created.thread, invite()).expect("invite");
    let again = change(h, &a.clone().with_replay_key(key("r2")), &created.thread, invite()).expect("invite");
    assert_eq!(again, invited);

    let leaving = c.clone().with_replay_key(key("r3"));
    let left = change(h, &leaving, &created.thread, ThreadAction::Leave).expect("leave");
    assert!(!left.threads[0].member);
    assert_eq!(change(h, &leaving, &created.thread, ThreadAction::Leave), Ok(left));
    assert_eq!(
        change(h, &c.with_replay_key(key("r4")), &created.thread, ThreadAction::Leave),
        Err(CommsError::NotMember {
            thread: created.thread.clone()
        })
    );

    let archived = change(
        h,
        &a.clone().with_replay_key(key("r5")),
        &created.thread,
        ThreadAction::Archive,
    );
    assert_eq!(
        change(h, &a.with_replay_key(key("r5")), &created.thread, ThreadAction::Archive),
        archived
    );
    assert_eq!(
        describe(h, &who(h, "a"), &created.thread).expect("thread").peers,
        principals(h, &["d"])
    );
}

/// An archived thread can be left and read by naming it or one of its messages, and nothing
/// else: a read over all threads does not return its messages, and nothing reopens it.
pub fn archived_thread_is_read_by_naming_it_and_closed_to_posts_joins_and_invitations(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &[]);
    change(h, &who(h, "b"), &created.thread, ThreadAction::Join).expect("join");
    let second = say(h, "a", &created.thread, "second").expect("post");
    change(h, &who(h, "a"), &created.thread, ThreadAction::Archive).expect("archive");
    let archived = CommsError::ThreadArchived {
        thread: created.thread.clone(),
    };

    assert_eq!(say(h, "a", &created.thread, "late"), Err(archived.clone()));
    assert_eq!(
        change(h, &who(h, "c"), &created.thread, ThreadAction::Join),
        Err(archived.clone())
    );
    assert_eq!(
        change(
            h,
            &who(h, "a"),
            &created.thread,
            ThreadAction::Invite(agents(h, &["c"]))
        ),
        Err(archived)
    );

    assert_eq!(bodies(&read(h, "b")), Vec::<&str>::new());
    assert_eq!(bodies(&read_as(h, "b", history()).expect("inbox")), Vec::<&str>::new());
    assert_eq!(bodies(&read_as(h, "b", by_id(&second.id)).expect("inbox")), ["second"]);
    assert_eq!(
        bodies(&read_as(h, "b", only(&created.thread)).expect("inbox")),
        ["second"]
    );
    let whole = read_as(
        h,
        "b",
        InboxBody {
            select: only(&created.thread).select,
            ..history()
        },
    )
    .expect("inbox");
    assert_eq!(bodies(&whole), ["first", "second"]);
    assert!(
        describe(h, &who(h, "d"), &created.thread)
            .expect("an open thread stays describable")
            .archived
    );
    let left = change(h, &who(h, "b"), &created.thread, ThreadAction::Leave).expect("leave");
    assert!(!left.threads[0].member);
}

/// A repository holds [`MAX_ACTIVE_THREADS_PER_REPO`] active threads. Creating one more archives
/// the one whose newest post is the oldest and tells the poster, and only the poster. Another
/// repository has its own count.
pub fn creating_a_thread_at_the_cap_archives_the_least_recently_active(h: &impl ConformanceHarness) {
    let first = start(h, "a", "topic", &["b"]);
    let second = start(h, "a", "topic", &["b"]);
    for _ in 2..MAX_ACTIVE_THREADS_PER_REPO {
        assert_eq!(start(h, "a", "topic", &[]).notices, Vec::<String>::new());
    }
    say(h, "a", &first.thread, "still in use").expect("post");
    h.post(&elsewhere(h, "a"), new_thread(h, "topic", &[], &[], "first"))
        .expect("another repository has room");

    let over = start(h, "a", "one more", &[]);

    assert!(over.created);
    assert_eq!(over.notices, [cap_archive_notice(&second.thread)]);
    let archived = |thread: &ThreadId| describe(h, &who(h, "a"), thread).expect("thread").archived;
    assert!(archived(&second.thread));
    assert!(!archived(&first.thread));
    assert!(!archived(&over.thread));
    assert_eq!(h.thread_count(), MAX_ACTIVE_THREADS_PER_REPO + 2);
    assert_eq!(
        bodies(&read(h, "b")),
        ["first", "still in use"],
        "no notice, and no mail of it"
    );
    assert_eq!(
        say(h, "b", &second.thread, "late"),
        Err(CommsError::ThreadArchived { thread: second.thread })
    );
}

/// A thread holds at most [`MAX_THREAD_MEMBERS`] members. A join or an invitation that would
/// pass that is refused whole, inviting members that are already in adds nobody and is not, and
/// a member that leaves frees a place.
pub fn members_are_capped_per_thread(h: &impl ConformanceHarness) {
    let created = start(h, "a", "topic", &[]);
    let names: Vec<String> = (1..MAX_THREAD_MEMBERS).map(|n| format!("m{n}")).collect();
    let (last, others) = names.split_last().expect("more than one member");
    for batch in others.chunks(MAX_RECIPIENTS) {
        let batch: Vec<&str> = batch.iter().map(String::as_str).collect();
        change(
            h,
            &who(h, "a"),
            &created.thread,
            ThreadAction::Invite(agents(h, &batch)),
        )
        .expect("invite");
    }
    let full = CommsError::MemberCapReached {
        thread: created.thread.clone(),
        cap: MAX_THREAD_MEMBERS,
    };

    assert_eq!(
        change(
            h,
            &who(h, "a"),
            &created.thread,
            ThreadAction::Invite(agents(h, &[last.as_str(), "x"]))
        ),
        Err(full.clone()),
        "one place is left and the invitation needs two"
    );
    assert_eq!(h.position(&created.thread, &agent(h, last)), None, "refused whole");
    change(h, &who(h, last), &created.thread, ThreadAction::Join).expect("the last place");
    assert_eq!(
        describe(h, &who(h, "a"), &created.thread).expect("thread").peers.len(),
        MAX_THREAD_MEMBERS - 1
    );

    assert_eq!(
        change(h, &who(h, "x"), &created.thread, ThreadAction::Join),
        Err(full.clone())
    );
    assert_eq!(
        change(
            h,
            &who(h, "a"),
            &created.thread,
            ThreadAction::Invite(agents(h, &["x"]))
        ),
        Err(full)
    );
    change(
        h,
        &who(h, "a"),
        &created.thread,
        ThreadAction::Invite(agents(h, &["m1", "m2", "m1"])),
    )
    .expect("inviting members adds nobody");
    change(h, &who(h, last), &created.thread, ThreadAction::Join).expect("joining again changes nothing");

    change(h, &who(h, "m1"), &created.thread, ThreadAction::Leave).expect("leave");
    change(h, &who(h, "x"), &created.thread, ThreadAction::Join).expect("a place was freed");
    assert_eq!(h.position(&created.thread, &agent(h, "x")), Some((1, 1)));
}
