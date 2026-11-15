//! The shared rules, where a property is easier to state directly than through a store.

use std::cell::Cell;

use super::*;
use crate::comms::v2::api::{MESSAGE_TTL, RESPONSE_BUDGET_BYTES};

fn actor(name: &str) -> Actor {
    Actor::parse(name).expect("actor")
}

fn thread(bits: u32) -> ThreadId {
    ThreadId::from_bits(bits)
}

fn message(order: u64, body_len: usize) -> Result<(u64, InboxMessage), CommsError> {
    let id = MessageId::new(thread(0), order).expect("message id");
    Ok((
        order,
        render(
            id,
            "topic",
            &actor("a"),
            0,
            None,
            &"x".repeat(body_len),
            BodyForm::Whole,
        ),
    ))
}

const MEMBER: Access = Access {
    member: true,
    open: false,
    same_repo: true,
    archived: false,
    creator: false,
};

#[test]
fn take_page_pulls_one_candidate_past_the_limit_and_no_further() {
    let pulled = Cell::new(0);
    let candidates = (1..=50).map(|order| {
        pulled.set(pulled.get() + 1);
        message(order, 1)
    });

    let (page, more) = take_page(candidates, Some(3)).expect("page");

    assert_eq!(page.len(), 3);
    assert!(more);
    assert_eq!(pulled.get(), 4);
}

#[test]
fn take_page_stops_at_the_first_message_over_the_budget() {
    let candidates = vec![message(1, 10), message(2, RESPONSE_BUDGET_BYTES), message(3, 1)];

    let (page, more) = take_page(candidates.into_iter(), None).expect("page");

    assert_eq!(page.iter().map(|(order, _)| *order).collect::<Vec<_>>(), [1]);
    assert!(more);
}

#[test]
fn take_page_reports_no_more_when_the_candidates_run_out() {
    let (page, more) = take_page(vec![message(1, 1), message(2, 1)].into_iter(), Some(2)).expect("page");

    assert_eq!(page.len(), 2);
    assert!(!more);
}

#[test]
fn take_page_returns_a_candidate_failure_instead_of_a_short_page() {
    let failure = CommsError::Backend {
        detail: "disk".to_string(),
    };
    let candidates = vec![message(1, 1), Err(failure.clone()), message(3, 1)];

    assert_eq!(take_page(candidates.into_iter(), None), Err(failure));
}

#[test]
fn post_refusals_come_in_one_order() {
    let id = thread(7);
    let target = MessageId::new(id.clone(), 9).expect("message id");
    let outsider = Access {
        member: false,
        archived: true,
        ..MEMBER
    };
    let archived = Access {
        archived: true,
        ..MEMBER
    };

    assert_eq!(
        authorize_post(&id, None, None, true),
        Err(CommsError::ThreadNotFound { thread: id.clone() })
    );
    assert_eq!(
        authorize_post(&id, None, Some(&target), false),
        Err(CommsError::MessageNotFound { id: target.clone() })
    );
    assert_eq!(
        authorize_post(&id, Some(&outsider), Some(&target), false),
        Err(CommsError::NotMember { thread: id.clone() })
    );
    assert_eq!(
        authorize_post(&id, Some(&archived), Some(&target), false),
        Err(CommsError::MessageNotFound { id: target.clone() })
    );
    assert_eq!(
        authorize_post(&id, Some(&archived), Some(&target), true),
        Err(CommsError::ThreadArchived { thread: id.clone() })
    );
    assert_eq!(authorize_post(&id, Some(&MEMBER), Some(&target), true), Ok(()));
}

#[test]
fn only_a_marking_read_moves_read_seq_and_a_writing_read_never_moves_back() {
    let mut position = Position {
        read_seq: 2,
        notified_seq: 5,
    };

    assert!(!position.advance(ReadKind::Peek, 9));
    assert!(!position.advance(ReadKind::History, 9));
    assert_eq!((position.read_seq, position.notified_seq), (2, 5));

    assert!(!position.advance(ReadKind::Announcing, 4));
    assert_eq!((position.read_seq, position.notified_seq), (2, 5));
    assert_eq!(position.floor(ReadKind::Announcing), 5);
    assert_eq!(position.floor(ReadKind::Marking), 2);

    assert!(position.advance(ReadKind::Marking, 3));
    assert_eq!((position.read_seq, position.notified_seq), (3, 5));
    assert!(!position.advance(ReadKind::Marking, 3));
}

#[test]
fn a_new_actor_starts_at_its_principals_read_position_or_the_members_start() {
    let at = |seq| Position {
        read_seq: seq,
        notified_seq: seq,
    };

    assert_eq!(Position::start(None, 0), at(0), "a member from creation");
    assert_eq!(Position::start(None, 7), at(7), "the head as of joining");
    assert_eq!(Position::start(Some(3), 7), at(3), "a sibling actor has read");
}

#[test]
fn a_record_is_old_once_it_is_more_than_its_retention_old() {
    let ttl = ttl_micros(MESSAGE_TTL);

    assert!(!older_than(1, 1 + ttl, MESSAGE_TTL));
    assert!(older_than(1, 2 + ttl, MESSAGE_TTL));
    assert!(
        !older_than(i64::MAX, i64::MIN, MESSAGE_TTL),
        "stamped ahead of the clock"
    );
    assert_eq!(
        ttl_micros(SEEN_RETENTION),
        ttl_micros(SEEN_TTL) + ttl_micros(SEEN_REFRESH_INTERVAL)
    );
}

#[test]
fn a_post_is_stamped_by_the_clock_unless_that_is_not_later_than_its_threads_last_stamp() {
    assert_eq!(post_timestamp(500, None), 500, "the post that creates the thread");
    assert_eq!(post_timestamp(500, Some(499)), 500);
    assert_eq!(post_timestamp(500, Some(500)), 501, "the clock stood still");
    assert_eq!(
        post_timestamp(500, Some(9_000_000_000)),
        9_000_000_001,
        "the clock went back"
    );
    assert_eq!(post_timestamp(500, Some(i64::MAX)), i64::MAX);
}

#[test]
fn a_seen_record_is_written_on_the_first_call_and_again_once_it_is_a_day_old() {
    let day = ttl_micros(SEEN_REFRESH_INTERVAL);

    assert!(seen_due(None, 1_000), "a first call is recorded");
    assert!(!seen_due(Some(1_000), 1_000));
    assert!(!seen_due(Some(1_000), 1_000 + day - 1));
    assert!(seen_due(Some(1_000), 1_000 + day));
}

#[test]
fn joins_and_invitations_stop_at_the_member_cap() {
    let id = thread(7);
    let open = Access { open: true, ..MEMBER };
    let outsider = Access { member: false, ..open };
    let full = Err(CommsError::MemberCapReached {
        thread: id.clone(),
        cap: MAX_THREAD_MEMBERS,
    });
    let members = [actor("a").principal_id(), actor("b").principal_id()];
    let is_member = |principal: &Principal| members.contains(principal);
    let adding = |names: &[&str]| Change::Invite(names.iter().map(|name| actor(name).principal_id()).collect());
    let invite = |names: &[&str]| ThreadAction::Invite(names.iter().map(|name| actor(name)).collect());
    let cap = MAX_THREAD_MEMBERS;

    assert_eq!(
        authorize_update(&id, &ThreadAction::Join, &outsider, cap - 1, is_member),
        Ok(Change::Join)
    );
    assert_eq!(
        authorize_update(&id, &ThreadAction::Join, &outsider, cap, is_member),
        full
    );
    assert_eq!(
        authorize_update(&id, &ThreadAction::Join, &open, cap, is_member),
        Ok(Change::Nothing),
        "a member that joins again adds nobody"
    );
    assert_eq!(
        authorize_update(&id, &invite(&["c", "d"]), &MEMBER, cap - 2, is_member),
        Ok(adding(&["c", "d"]))
    );
    assert_eq!(
        authorize_update(&id, &invite(&["c", "d"]), &MEMBER, cap - 1, is_member),
        full
    );
    assert_eq!(
        authorize_update(&id, &invite(&["c", "c.helper", "a", "b"]), &MEMBER, cap - 1, is_member),
        Ok(adding(&["c"])),
        "a principal named twice and the members named are not counted"
    );
    assert_eq!(
        authorize_update(&id, &invite(&["a", "b.helper"]), &MEMBER, cap, is_member),
        Ok(Change::Nothing)
    );
    assert_eq!(
        authorize_update(&id, &ThreadAction::Leave, &MEMBER, cap, is_member),
        Ok(Change::Leave)
    );
    let archived = Access {
        archived: true,
        ..outsider
    };
    assert_eq!(
        authorize_update(&id, &ThreadAction::Join, &archived, cap, is_member),
        Err(CommsError::ThreadArchived { thread: id }),
        "the cap is the last thing a join is refused for"
    );
}
