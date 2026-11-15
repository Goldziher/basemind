//! `agent_threads` against the in-memory reference store.

use serde_json::{Value, json};

use super::super::tests::{Agent, HOME, actor, parse_error, refused, thread};
use super::super::{AgentThread, AgentThreadsParams, AgentThreadsResponse};
use crate::comms::v2::api::{AgentOps, MAX_RECIPIENTS, Principal, RepoKey};
use crate::comms::v2::fake::{FAKE_EPOCH_MICROS, FakeAgents};

/// A repository other than [`HOME`].
const ELSEWHERE: &str = "path:/work/elsewhere";

/// Alice's invitation-only `t-aaaaaa` with bob, then carol's open `t-aaaaab`, both in [`HOME`].
fn two_threads() -> (FakeAgents, Agent, Agent, Agent) {
    let fake = FakeAgents::new();
    let (alice, bob, carol) = (Agent::new("alice"), Agent::new("bob"), Agent::new("carol"));
    alice
        .post(
            &fake,
            json!({ "subject": "auth refactor", "body": "first", "to": ["bob"], "paths": ["src/auth/"] }),
        )
        .expect("alice's thread");
    carol
        .post(
            &fake,
            json!({ "subject": "Deploy pipeline", "body": "first", "paths": ["ci"] }),
        )
        .expect("carol's thread");
    (fake, alice, bob, carol)
}

fn ids(response: &AgentThreadsResponse) -> Vec<String> {
    response.threads.iter().map(|thread| thread.id.to_string()).collect()
}

fn peers(response: &AgentThreadsResponse) -> Vec<String> {
    response.threads[0].peers.iter().map(ToString::to_string).collect()
}

fn listed(fake: &FakeAgents, agent: &Agent, arguments: Value) -> Vec<String> {
    ids(&agent.threads(fake, arguments).expect("a listing"))
}

fn not_applicable(field: &str, with: &str) -> String {
    format!("`agent_threads`: `{field}` does not apply together with `{with}`: drop `{field}`, or make two calls")
}

fn closed(thread: &str) -> String {
    format!(
        "`agent_threads`: thread {thread} is closed to this repository (invitation-only, or owned by another \
         repository): ask a member to invite you"
    )
}

fn not_member(thread: &str) -> String {
    format!(
        "`agent_threads`: not a member of thread {thread}: join it with agent_threads {{thread, join: true}}, or \
         ask a member to invite you"
    )
}

#[test]
fn no_arguments_lists_the_callers_threads_with_unread_counts_and_peers() {
    let (fake, alice, bob, _carol) = two_threads();

    let mine = alice.threads(&fake, json!({})).expect("alice's threads");

    assert_eq!(
        mine,
        AgentThreadsResponse {
            threads: vec![AgentThread {
                id: thread("t-aaaaaa"),
                subject: "auth refactor".to_string(),
                repo: RepoKey::parse(HOME).expect("a repository key"),
                paths: vec!["src/auth".to_string()],
                open: false,
                archived: false,
                creator: actor("alice"),
                member: true,
                unread: 0,
                head: 1,
                last_activity_micros: FAKE_EPOCH_MICROS,
                peers: vec![Principal::parse("bob").expect("a principal")],
            }],
            pending: Vec::new(),
            next_cursor: None,
            notices: Vec::new(),
        }
    );
    assert_eq!(
        serde_json::to_value(&mine).expect("serialize"),
        json!({
            "threads": [{
                "id": "t-aaaaaa",
                "subject": "auth refactor",
                "repo": HOME,
                "paths": ["src/auth"],
                "open": false,
                "creator": "alice",
                "member": true,
                "unread": 0,
                "head": 1,
                "last_activity_micros": FAKE_EPOCH_MICROS,
                "peers": ["bob"],
            }],
            "notices": [],
        })
    );

    let bobs = bob.threads(&fake, json!({})).expect("bob's threads");
    assert_eq!(ids(&bobs), ["t-aaaaaa"]);
    assert_eq!(bobs.threads[0].unread, 1);
    assert_eq!(peers(&bobs), ["alice"]);

    let nobody = Agent::new("dave").threads(&fake, json!({})).expect("dave's threads");
    assert_eq!(
        serde_json::to_value(&nobody).expect("serialize"),
        json!({ "threads": [], "notices": [] })
    );
}

#[test]
fn query_and_path_also_find_the_open_threads_of_the_callers_repository() {
    let (fake, alice, _bob, carol) = two_threads();

    let found = alice
        .threads(&fake, json!({ "query": "DEPLOY" }))
        .expect("a discovery by subject");
    assert_eq!(ids(&found), ["t-aaaaab"]);
    assert!(found.threads[0].open);
    assert!(!found.threads[0].member);
    assert_eq!(found.threads[0].unread, 0);
    assert_eq!(peers(&found), ["carol"]);

    assert_eq!(listed(&fake, &alice, json!({ "path": "ci/deploy.yml" })), ["t-aaaaab"]);
    assert_eq!(listed(&fake, &alice, json!({ "path": "src" })), ["t-aaaaaa"]);
    assert_eq!(listed(&fake, &alice, json!({ "query": "auth" })), ["t-aaaaaa"]);
    assert!(listed(&fake, &alice, json!({ "query": "deploy", "path": "src" })).is_empty());
    assert!(listed(&fake, &alice, json!({ "query": "nothing like it" })).is_empty());

    assert!(
        listed(&fake, &carol, json!({ "query": "auth" })).is_empty(),
        "an invitation-only thread is not discovered"
    );
    let outsider = Agent::in_repo("erin", ELSEWHERE);
    assert!(
        listed(&fake, &outsider, json!({ "query": "deploy" })).is_empty(),
        "another repository's open thread is not discovered"
    );
}

#[test]
fn a_query_under_three_characters_or_a_path_outside_the_repository_is_refused() {
    let (fake, alice, _bob, _carol) = two_threads();

    assert_eq!(
        refused(alice.threads(&fake, json!({ "query": "de" }))),
        "`agent_threads`: `query` needs at least 3 characters: lengthen it, or search by `path`"
    );
    let outside = refused(alice.threads(&fake, json!({ "path": "../elsewhere" })));
    assert!(
        outside.starts_with("`agent_threads`: path \"../elsewhere\" is not a repository-relative path prefix"),
        "{outside}"
    );
}

#[test]
fn thread_alone_describes_one_thread_to_its_members_and_its_repository() {
    let (fake, alice, _bob, carol) = two_threads();

    let described = alice
        .threads(&fake, json!({ "thread": "t-aaaaab" }))
        .expect("an open thread described");
    assert_eq!(ids(&described), ["t-aaaaab"]);
    assert_eq!(described.threads[0].subject, "Deploy pipeline");
    assert!(!described.threads[0].member);

    assert_eq!(
        refused(carol.threads(&fake, json!({ "thread": "t-aaaaaa" }))),
        closed("t-aaaaaa")
    );
    assert_eq!(
        refused(Agent::in_repo("erin", ELSEWHERE).threads(&fake, json!({ "thread": "t-aaaaab" }))),
        closed("t-aaaaab")
    );
    assert_eq!(
        refused(alice.threads(&fake, json!({ "thread": "t-zzzzzz" }))),
        "`agent_threads`: thread t-zzzzzz not found: list your threads with agent_threads, or search with `query` \
         or `path`"
    );
    let malformed = refused(alice.threads(&fake, json!({ "thread": "aaaaaa" })));
    assert!(
        malformed.starts_with("`agent_threads`: invalid thread id \"aaaaaa\""),
        "{malformed}"
    );
}

#[test]
fn join_makes_the_caller_a_member_at_the_head_and_leave_ends_it() {
    let (fake, alice, _bob, carol) = two_threads();

    let joined = alice
        .threads(&fake, json!({ "thread": "t-aaaaab", "join": true }))
        .expect("a join");
    assert_eq!(ids(&joined), ["t-aaaaab"]);
    assert!(joined.threads[0].member);
    assert_eq!(joined.threads[0].unread, 0, "what was said before is history");
    assert_eq!(peers(&joined), ["carol"]);
    assert_eq!(listed(&fake, &alice, json!({})), ["t-aaaaab", "t-aaaaaa"]);

    alice
        .threads(&fake, json!({ "thread": "t-aaaaab", "join": true }))
        .expect("joining again changes nothing");
    carol
        .post(&fake, json!({ "thread": "t-aaaaab", "body": "welcome" }))
        .expect("carol's post");
    let read = alice.inbox(&fake, json!({})).expect("alice's inbox");
    assert_eq!(read.messages.len(), 1);
    assert_eq!(read.messages[0].body, "welcome");

    let left = alice
        .threads(&fake, json!({ "thread": "t-aaaaab", "leave": true }))
        .expect("a leave");
    assert!(!left.threads[0].member);
    assert!(!left.threads[0].archived);
    assert_eq!(listed(&fake, &alice, json!({})), ["t-aaaaaa"]);
    assert_eq!(
        refused(alice.threads(&fake, json!({ "thread": "t-aaaaab", "leave": true }))),
        not_member("t-aaaaab")
    );
}

#[test]
fn joining_needs_an_open_thread_of_the_callers_repository() {
    let (fake, alice, _bob, carol) = two_threads();

    assert_eq!(
        refused(carol.threads(&fake, json!({ "thread": "t-aaaaaa", "join": true }))),
        closed("t-aaaaaa")
    );
    assert_eq!(
        refused(Agent::in_repo("erin", ELSEWHERE).threads(&fake, json!({ "thread": "t-aaaaab", "join": true }))),
        closed("t-aaaaab")
    );
    assert_eq!(
        refused(alice.threads(&fake, json!({ "thread": "t-zzzzzz", "join": true }))),
        "`agent_threads`: thread t-zzzzzz not found: list your threads with agent_threads, or search with `query` \
         or `path`"
    );
    assert_eq!(listed(&fake, &carol, json!({})), ["t-aaaaab"]);
}

#[test]
fn the_last_member_to_leave_archives_the_thread() {
    let (fake, _alice, _bob, carol) = two_threads();

    let left = carol
        .threads(&fake, json!({ "thread": "t-aaaaab", "leave": true }))
        .expect("carol leaves her own thread");

    assert!(left.threads[0].archived);
    assert!(!left.threads[0].member);
    assert_eq!(
        serde_json::to_value(&left).expect("serialize")["threads"][0]["archived"],
        json!(true)
    );
}

#[test]
fn any_member_may_invite_and_pending_lists_the_invited_that_made_no_call() {
    let (fake, alice, bob, _carol) = two_threads();
    let (dave, erin) = (Agent::new("dave"), Agent::in_repo("erin", ELSEWHERE));
    dave.threads(&fake, json!({})).expect("dave makes a call");

    let invited = bob
        .threads(&fake, json!({ "thread": "t-aaaaaa", "invite": ["dave", "erin"] }))
        .expect("an invitation by a member");

    assert_eq!(invited.pending, [actor("erin")]);
    assert_eq!(peers(&invited), ["alice", "dave", "erin"]);
    assert_eq!(
        serde_json::to_value(&invited).expect("serialize")["pending"],
        json!(["erin"])
    );

    assert!(
        dave.inbox(&fake, json!({})).expect("dave's inbox").messages.is_empty(),
        "an invited agent reads from the head"
    );
    alice
        .post(&fake, json!({ "thread": "t-aaaaaa", "body": "welcome" }))
        .expect("alice's post");
    let theirs = erin
        .threads(&fake, json!({}))
        .expect("the threads of an agent of another repository");
    assert_eq!(ids(&theirs), ["t-aaaaaa"]);
    assert_eq!(theirs.threads[0].repo.as_str(), HOME);
    assert_eq!(theirs.threads[0].unread, 1);
    dave.post(&fake, json!({ "thread": "t-aaaaaa", "body": "thanks" }))
        .expect("an invited agent posts");
}

#[test]
fn an_invitation_is_refused_from_a_non_member_and_when_empty_malformed_or_too_long() {
    let (fake, alice, _bob, carol) = two_threads();
    let invite = |agent: &Agent, invited: Value| {
        refused(agent.threads(&fake, json!({ "thread": "t-aaaaaa", "invite": invited })))
    };

    assert_eq!(invite(&carol, json!(["carol"])), not_member("t-aaaaaa"));
    assert_eq!(
        invite(&alice, json!([])),
        "`agent_threads`: `invite` is empty: name at least one agent id"
    );
    let malformed = invite(&alice, json!(["not an id"]));
    assert!(
        malformed.starts_with("`agent_threads`: invalid agent id \"not an id\""),
        "{malformed}"
    );
    let crowd: Vec<String> = (0..=MAX_RECIPIENTS).map(|n| format!("agent-{n}")).collect();
    assert_eq!(
        invite(&alice, json!(crowd)),
        "`agent_threads`: `invite` names 33 agents, above the maximum of 32: name fewer, and invite the rest in \
         further calls"
    );
    assert_eq!(
        peers(
            &alice
                .threads(&fake, json!({ "thread": "t-aaaaaa" }))
                .expect("described")
        ),
        ["bob"]
    );
}

fn archived(tool: &str, thread: &str) -> String {
    format!(
        "`{tool}`: thread {thread} is archived: read it with agent_inbox `thread`, or start a new thread by posting \
         with `subject`"
    )
}

#[test]
fn only_the_creator_archives_and_an_archived_thread_is_closed_to_posts_and_invitations() {
    let (fake, alice, bob, _carol) = two_threads();

    assert_eq!(
        refused(bob.threads(&fake, json!({ "thread": "t-aaaaaa", "archive": true }))),
        "`agent_threads`: only the creator of thread t-aaaaaa may archive it: leave it with agent_threads {thread, \
         leave: true} instead"
    );

    let archived_now = alice
        .threads(&fake, json!({ "thread": "t-aaaaaa", "archive": true }))
        .expect("the creator archives");
    assert!(archived_now.threads[0].archived);
    assert!(archived_now.threads[0].member);

    assert_eq!(
        refused(bob.post(&fake, json!({ "thread": "t-aaaaaa", "body": "too late" }))),
        archived("agent_post", "t-aaaaaa")
    );
    assert_eq!(
        refused(bob.post(&fake, json!({ "reply_to": "t-aaaaaa.1", "body": "too late" }))),
        archived("agent_post", "t-aaaaaa")
    );
    assert_eq!(
        refused(alice.threads(&fake, json!({ "thread": "t-aaaaaa", "invite": ["dave"] }))),
        archived("agent_threads", "t-aaaaaa")
    );
    assert_eq!(fake.arrivals(), 2, "nothing was posted into the archived thread");

    assert!(listed(&fake, &alice, json!({})).is_empty());
    assert_eq!(listed(&fake, &alice, json!({ "archived": true })), ["t-aaaaaa"]);
}

#[test]
fn an_archived_thread_is_read_by_naming_it_and_can_be_left_but_not_joined() {
    let (fake, alice, bob, carol) = two_threads();
    alice
        .threads(&fake, json!({ "thread": "t-aaaaaa", "archive": true }))
        .expect("alice archives her thread");
    carol
        .threads(&fake, json!({ "thread": "t-aaaaab", "archive": true }))
        .expect("carol archives her open thread");

    assert!(
        bob.inbox(&fake, json!({}))
            .expect("a read over all threads")
            .messages
            .is_empty(),
        "a read over all threads covers active threads only"
    );
    let by_id = bob
        .inbox(&fake, json!({ "id": "t-aaaaaa.1" }))
        .expect("an archived message by id");
    assert_eq!(by_id.messages[0].body, "first");
    let named = bob
        .inbox(&fake, json!({ "thread": "t-aaaaaa" }))
        .expect("the archived thread, named");
    assert_eq!(named.messages.len(), 1);
    assert_eq!(named.read_to.len(), 1);

    assert_eq!(
        refused(alice.threads(&fake, json!({ "thread": "t-aaaaab", "join": true }))),
        archived("agent_threads", "t-aaaaab")
    );
    for unchanged in [
        json!({ "thread": "t-aaaaaa", "join": true }),
        json!({ "thread": "t-aaaaaa", "invite": ["bob"] }),
        json!({ "thread": "t-aaaaaa", "archive": true }),
    ] {
        let reply = alice.threads(&fake, unchanged).expect("a change already made succeeds");
        assert!(reply.threads[0].archived);
        assert_eq!(peers(&reply), ["bob"]);
    }

    let left = bob
        .threads(&fake, json!({ "thread": "t-aaaaaa", "leave": true }))
        .expect("an archived thread can be left");
    assert!(!left.threads[0].member);
    assert!(left.threads[0].archived, "nothing reopens it");
}

#[test]
fn a_call_makes_at_most_one_change_and_a_change_needs_a_thread() {
    let (fake, alice, _bob, _carol) = two_threads();

    assert_eq!(
        refused(alice.threads(&fake, json!({ "thread": "t-aaaaaa", "join": true, "leave": true }))),
        "`agent_threads`: pass at most one of join, leave, invite, archive; this call set [join, leave]: make one \
         call per change"
    );
    assert_eq!(
        refused(alice.threads(
            &fake,
            json!({ "thread": "t-aaaaaa", "invite": ["dave"], "archive": true })
        )),
        "`agent_threads`: pass at most one of join, leave, invite, archive; this call set [invite, archive]: make \
         one call per change"
    );
    for change in ["join", "leave", "archive"] {
        assert_eq!(
            refused(alice.threads(&fake, json!({ change: true }))),
            format!("`agent_threads`: `{change}` needs `thread`: pass `thread` as well")
        );
    }
    assert_eq!(
        refused(alice.threads(&fake, json!({ "invite": ["dave"] }))),
        "`agent_threads`: `invite` needs `thread`: pass `thread` as well"
    );
}

#[test]
fn a_listing_field_beside_a_thread_is_refused_not_ignored() {
    let (fake, alice, _bob, _carol) = two_threads();
    let on_thread = |extra: Value| {
        let mut arguments = json!({ "thread": "t-aaaaaa" });
        for (field, value) in extra.as_object().expect("an object") {
            arguments[field.as_str()] = value.clone();
        }
        refused(alice.threads(&fake, arguments))
    };

    assert_eq!(on_thread(json!({ "query": "auth" })), not_applicable("query", "thread"));
    assert_eq!(on_thread(json!({ "path": "src" })), not_applicable("path", "thread"));
    assert_eq!(on_thread(json!({ "limit": 5 })), not_applicable("limit", "thread"));
    assert_eq!(
        on_thread(json!({ "archived": true })),
        not_applicable("archived", "thread")
    );
    assert_eq!(
        on_thread(json!({ "cursor": "abc", "leave": true })),
        not_applicable("cursor", "leave")
    );
    assert_eq!(
        on_thread(json!({ "archived": true, "join": true })),
        not_applicable("archived", "join")
    );
    assert_eq!(
        on_thread(json!({ "query": "auth", "invite": ["dave"] })),
        not_applicable("query", "invite")
    );
}

#[test]
fn a_field_at_its_default_or_empty_counts_as_left_out() {
    let (fake, alice, _bob, _carol) = two_threads();

    let described = alice
        .threads(
            &fake,
            json!({
                "thread": "t-aaaaaa",
                "join": false,
                "leave": false,
                "archive": false,
                "archived": false,
                "limit": 20,
                "query": "",
                "path": "",
                "cursor": "",
                "as": "",
            }),
        )
        .expect("a description");

    assert_eq!(ids(&described), ["t-aaaaaa"]);
    assert!(described.threads[0].member);
}

#[test]
fn an_unknown_field_is_refused_naming_the_accepted_ones() {
    let refusal = parse_error::<AgentThreadsParams>(json!({ "thread": "t-aaaaaa", "remove": "bob" }));

    assert!(
        refusal.starts_with("`agent_threads`: unknown field `remove`"),
        "{refusal}"
    );
    let (_, accepted) = refusal.split_once("Accepted fields: ").expect("the accepted fields");
    for field in [
        "`query`",
        "`path`",
        "`thread`",
        "`join`",
        "`leave`",
        "`invite`",
        "`archive`",
        "`archived`",
        "`limit`",
        "`cursor`",
        "`as`",
    ] {
        assert!(accepted.contains(field), "{field} is missing from: {accepted}");
    }
    assert_eq!(accepted.matches('`').count(), 22, "eleven fields: {accepted}");

    let mistyped = parse_error::<AgentThreadsParams>(json!({ "invite": "bob", "join": "yes" }));
    assert!(mistyped.contains("`invite` must be an array"), "{mistyped}");
    assert!(mistyped.contains("`join` must be a boolean"), "{mistyped}");
}

#[test]
fn as_shares_the_principals_memberships_and_an_invalid_as_is_refused() {
    let (fake, alice, _bob, carol) = two_threads();

    assert_eq!(listed(&fake, &alice, json!({ "as": "worker" })), ["t-aaaaaa"]);

    let joined = alice
        .threads(&fake, json!({ "thread": "t-aaaaab", "join": true, "as": "worker" }))
        .expect("the sub-identity joins for its principal");
    assert!(joined.threads[0].member);
    assert_eq!(
        listed(&fake, &alice, json!({ "as": "worker" })),
        ["t-aaaaab", "t-aaaaaa"]
    );
    assert_eq!(listed(&fake, &alice, json!({})), ["t-aaaaab", "t-aaaaaa"]);
    assert_eq!(
        peers(
            &carol
                .threads(&fake, json!({ "thread": "t-aaaaab" }))
                .expect("described")
        ),
        ["alice"],
        "the member is the principal, whichever actor joined"
    );

    alice
        .threads(&fake, json!({ "thread": "t-aaaaab", "leave": true }))
        .expect("the principal leaves for all its actors");
    assert_eq!(listed(&fake, &alice, json!({ "as": "worker" })), ["t-aaaaaa"]);

    let refusal = refused(alice.threads(&fake, json!({ "as": "carol.x" })));
    assert!(
        refusal.starts_with("`agent_threads`: `as`: invalid sub-identity name \"carol.x\""),
        "{refusal}"
    );
}

#[test]
fn a_listing_is_newest_first_and_pages_with_the_cursor_return_every_thread_once() {
    let fake = FakeAgents::new();
    let alice = Agent::new("alice");
    for subject in ["one", "two", "three"] {
        alice
            .post(&fake, json!({ "subject": subject, "body": "b" }))
            .expect("a thread");
    }

    let first = alice.threads(&fake, json!({ "limit": 2 })).expect("the first page");
    assert_eq!(ids(&first), ["t-aaaaac", "t-aaaaab"]);
    let cursor = json!(first.next_cursor.expect("a cursor"));

    let second = alice
        .threads(&fake, json!({ "limit": 2, "cursor": cursor }))
        .expect("the second page");
    assert_eq!(ids(&second), ["t-aaaaaa"]);
    assert_eq!(second.next_cursor, None);

    assert_eq!(listed(&fake, &alice, json!({ "limit": 0 })), ["t-aaaaac"]);
    assert_eq!(listed(&fake, &alice, json!({})).len(), 3);

    for (agent, arguments) in [
        (&alice, json!({ "archived": true, "cursor": cursor })),
        (&alice, json!({ "query": "one", "cursor": cursor })),
        (&Agent::new("bob"), json!({ "cursor": cursor })),
        (&alice, json!({ "cursor": "not-a-cursor" })),
    ] {
        let refusal = refused(agent.threads(&fake, arguments));
        assert!(refusal.starts_with("`agent_threads`: `cursor` "), "{refusal}");
        assert!(
            refusal.ends_with("is not a `next_cursor` of this request: repeat the call without `cursor`"),
            "{refusal}"
        );
    }
}
