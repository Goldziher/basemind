//! The session, the requests and the refusal wording every front end shares.

use std::cell::RefCell;
use std::time::Duration;

use super::*;
use crate::comms::v2::api::{
    IdKind, InboxSelect, InvalidId, MAX_POST_BODY_BYTES, MessageId, ReadKind, ThreadId, WaitOutcome,
};
use crate::comms::v2::fake::FakeAgents;

/// Longest principal the id grammar allows: any sub-identity of it is over the id length limit.
const LONGEST_PRINCIPAL_LEN: usize = 128;

struct Owner {
    principal: Principal,
    repo: RepoKey,
}

impl Owner {
    fn new(principal: &str) -> Self {
        Self {
            principal: Principal::parse(principal).expect("a valid principal"),
            repo: RepoKey::parse("github.com/acme/app").expect("a valid repository key"),
        }
    }

    fn session(&self) -> Session<'_> {
        Session {
            principal: &self.principal,
            repo: &self.repo,
            replay_key: None,
            notices: &[],
        }
    }
}

fn announce(fields: InboxFields) -> Result<InboxCall, CommsError> {
    inbox_call(fields, true)
}

fn not_applicable(field: &str) -> Result<InboxCall, CommsError> {
    Err(CommsError::not_applicable(field, ANNOUNCE_FIELD))
}

#[test]
fn a_call_without_as_is_made_by_the_principal() {
    let owner = Owner::new("alice");

    for as_ in [None, Some("")] {
        let caller = owner.session().caller(as_).expect("the principal");
        assert_eq!(caller.actor.as_str(), "alice");
        assert_eq!(caller.actor.sub(), None);
        assert_eq!(caller.repo, owner.repo);
        assert_eq!(caller.replay_key, None);
    }
}

#[test]
fn as_names_a_sub_identity_of_the_sessions_principal_and_keeps_the_replay_key() {
    let owner = Owner::new("alice");
    let key = ReplayKey::parse("relay-1").expect("a replay key");
    let session = Session {
        replay_key: Some(&key),
        ..owner.session()
    };

    let caller = session.caller(Some("helper")).expect("the sub-identity");

    assert_eq!(caller.actor.as_str(), "alice.helper");
    assert_eq!(caller.actor.principal_id(), owner.principal);
    assert_eq!(caller.repo, owner.repo);
    assert_eq!(caller.replay_key, Some(key));
}

#[test]
fn an_as_outside_the_grammar_or_over_the_id_length_is_refused() {
    let owner = Owner::new("alice");
    let longest = Owner::new(&"p".repeat(LONGEST_PRINCIPAL_LEN));

    assert_eq!(
        owner.session().caller(Some("bob.helper")),
        Err(CommsError::InvalidId(InvalidId::new(IdKind::Sub, "bob.helper")))
    );
    assert!(longest.session().caller(None).is_ok());
    assert!(matches!(
        longest.session().caller(Some("helper")),
        Err(CommsError::InvalidId(_))
    ));
}

#[test]
fn a_replys_notices_come_first_and_unchanged() {
    let owner = Owner::new("alice");
    let own = ["from the session".to_string()];
    let session = Session {
        notices: &own,
        ..owner.session()
    };

    assert_eq!(
        session.notices(vec!["from the store".to_string(), "from the daemon".to_string()]),
        ["from the store", "from the daemon", "from the session"]
    );
    assert_eq!(owner.session().notices(Vec::new()), Vec::<String>::new());
}

#[test]
fn a_post_checks_its_body_before_its_target() {
    let fields = |body: &str| PostFields {
        body: body.to_string(),
        ..PostFields::default()
    };

    assert_eq!(post_body(fields(" \n")), Err(CommsError::EmptyBody));
    assert_eq!(
        post_body(fields("hi")),
        Err(CommsError::InvalidPostTarget { given: Vec::new() })
    );
    assert_eq!(
        post_body(fields(&"x".repeat(MAX_POST_BODY_BYTES + 1))),
        Err(CommsError::BodyTooLarge {
            len: MAX_POST_BODY_BYTES + 1,
            max: MAX_POST_BODY_BYTES,
        })
    );
}

#[test]
fn a_post_of_exactly_the_body_limit_is_built_with_its_target() {
    let body = "x".repeat(MAX_POST_BODY_BYTES);

    let post = post_body(PostFields {
        body: body.clone(),
        thread: Some("t-aaaaaa".to_string()),
        reply_to: Some(String::new()),
        ..PostFields::default()
    })
    .expect("a post at the limit");

    assert_eq!(
        post,
        PostBody {
            target: PostTarget::Thread(ThreadId::parse("t-aaaaaa").expect("a thread id")),
            body,
        }
    );
}

#[test]
fn without_announce_the_call_is_the_shared_field_check() {
    let fields = InboxFields {
        thread: Some("t-aaaaaa".to_string()),
        peek: Some(true),
        wait_secs: Some(7),
        limit: Some(3),
        ..InboxFields::default()
    };

    let call = inbox_call(fields.clone(), false).expect("a peek");

    assert_eq!(Ok(&call), InboxCall::from_fields(fields).as_ref());
    assert_eq!(call.body.mark, Mark::Peek);
    assert_eq!(call.wait, Duration::from_secs(7));
}

#[test]
fn announce_is_the_announcing_read_of_the_selection_and_never_waits() {
    let call = announce(InboxFields {
        thread: Some("t-aaaaaa".to_string()),
        limit: Some(3),
        ..InboxFields::default()
    })
    .expect("an announcing read");

    assert_eq!(call.body.kind(), Ok(ReadKind::Announcing));
    assert_eq!(
        call.body,
        InboxBody {
            select: InboxSelect::Thread(ThreadId::parse("t-aaaaaa").expect("a thread id")),
            unread: true,
            mark: Mark::Notify,
            limit: Some(3),
            cursor: None,
        }
    );
    assert!(call.wait.is_zero());
}

#[test]
fn announce_beside_a_field_at_its_default_is_not_refused() {
    let call = announce(InboxFields {
        id: Some(String::new()),
        unread: Some(true),
        peek: Some(false),
        wait_secs: Some(0),
        cursor: Some(String::new()),
        ..InboxFields::default()
    })
    .expect("an announcing read");

    assert_eq!(call.body.select, InboxSelect::All);
    assert_eq!(call.body.kind(), Ok(ReadKind::Announcing));
}

#[test]
fn announce_is_refused_beside_every_field_that_asks_for_another_read() {
    let with = |change: fn(&mut InboxFields)| {
        let mut fields = InboxFields::default();
        change(&mut fields);
        announce(fields)
    };

    assert_eq!(with(|f| f.id = Some("t-aaaaaa.1".to_string())), not_applicable("id"));
    assert_eq!(with(|f| f.peek = Some(true)), not_applicable("peek"));
    assert_eq!(with(|f| f.unread = Some(false)), not_applicable("unread"));
    assert_eq!(with(|f| f.wait_secs = Some(1)), not_applicable("wait_secs"));
    assert_eq!(
        with(|f| f.cursor = Some("abc".to_string())),
        Err(CommsError::CursorNotApplicable)
    );
    assert_eq!(
        with(|f| f.wait_secs = Some(41)),
        not_applicable("wait_secs"),
        "the field that does not apply is named before its range"
    );
}

#[test]
fn a_read_goes_to_the_store_unless_it_asks_to_wait() {
    let fake = FakeAgents::new();
    let owner = Owner::new("alice");
    let caller = owner.session().caller(None).expect("the principal");
    let asked = RefCell::new(Vec::new());
    let waiting_read = |caller: &Caller, body: InboxBody, wait: Duration| -> Result<InboxReply, CommsError> {
        asked.borrow_mut().push((caller.clone(), body, wait));
        Err(CommsError::Backend {
            detail: "the wait was asked".to_string(),
        })
    };
    let call = |wait_secs| {
        inbox_call(
            InboxFields {
                wait_secs: Some(wait_secs),
                ..InboxFields::default()
            },
            false,
        )
        .expect("a marking read")
    };

    let at_once = inbox_read(&fake, &caller, call(0), &waiting_read).expect("the store's reply");
    assert!(at_once.messages.is_empty());
    assert_eq!(at_once.wait, WaitOutcome::NotWaited);
    assert!(asked.borrow().is_empty());

    let waited = inbox_read(&fake, &caller, call(5), &waiting_read);
    assert_eq!(
        waited,
        Err(CommsError::Backend {
            detail: "the wait was asked".to_string()
        })
    );
    assert_eq!(
        asked.into_inner(),
        [(caller, InboxBody::default(), Duration::from_secs(5))]
    );
}

#[test]
fn a_refusal_names_the_surface_and_keeps_the_fix() {
    let refused = refusal("agent post", &CommsError::EmptyBody);

    assert_eq!(
        refused,
        Refusal {
            message: "agent post: `body` is empty: write the message to post".to_string(),
            store_failed: false,
        }
    );
}

#[test]
fn only_a_store_that_failed_or_is_newer_is_not_the_callers_mistake() {
    let thread = ThreadId::parse("t-aaaaaa").expect("a thread id");
    let backend = refusal(
        "`agent_inbox`",
        &CommsError::Backend {
            detail: "disk full".to_string(),
        },
    );
    let newer = refusal(
        "`agent_post`",
        &CommsError::StoreNewer {
            store: 1001,
            binary: 1000,
        },
    );

    assert!(backend.store_failed);
    assert_eq!(
        backend.message,
        "`agent_inbox`: the comms store failed: disk full; retry, and run `basemind agent status` if it persists"
    );
    assert!(newer.store_failed);
    for error in [
        CommsError::NotMember { thread: thread.clone() },
        CommsError::ThreadArchived { thread },
        CommsError::MessageNotFound {
            id: MessageId::parse("t-aaaaaa.1").expect("a message id"),
        },
        CommsError::Unknown {
            code: "from_the_future".to_string(),
            message: "do something else".to_string(),
        },
    ] {
        assert!(!refusal("agent threads", &error).store_failed, "{error}");
    }
}

#[test]
fn a_refused_as_names_the_field() {
    let error = CommsError::InvalidId(InvalidId::new(IdKind::Sub, "Bad Name"));

    let refused = as_refusal("agent inbox", &error);

    assert_eq!(refused.message, format!("agent inbox: `as`: {error}"));
    assert!(!refused.store_failed);
}
