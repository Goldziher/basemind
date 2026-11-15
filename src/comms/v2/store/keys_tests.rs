//! Key encodings: round trips, prefix isolation and byte order.

use super::*;

fn actor(name: &str) -> Actor {
    Actor::parse(name).expect("actor")
}

fn thread_id(bits: u32) -> ThreadId {
    ThreadId::from_bits(bits)
}

#[test]
fn thread_key_round_trips_and_refuses_bits_outside_the_id_space() {
    let id = thread_id(0x2a5f_1234 % ThreadId::SPACE);

    assert_eq!(parse_thread(&thread(&id)), Some(id));
    assert_eq!(parse_thread(&ThreadId::SPACE.to_be_bytes()), None);
    assert_eq!(parse_thread(&[0, 0, 1]), None);
}

#[test]
fn message_keys_of_one_thread_sort_by_sequence_number() {
    let id = thread_id(7);
    let mut keys = [message(&id, 256), message(&id, 2), message(&id, 1), message(&id, 255)];
    keys.sort();

    let seqs: Vec<u64> = keys.iter().filter_map(|key| parse_message_seq(key)).collect();
    assert_eq!(seqs, [1, 2, 255, 256]);
    assert!(message(&thread_id(8), 1) > message(&id, u64::MAX));
    assert_eq!(parse_message_seq(&message(&id, 9)[..11]), None);
}

#[test]
fn post_order_row_round_trips_with_a_negative_timestamp() {
    let id = thread_id(99);
    let key = order(-5, 7);

    assert_eq!(
        parse_order_row(&key, &message(&id, 41)),
        Some(OrderEntry {
            thread: id.clone(),
            seq: 41,
            ts_micros: -5,
        })
    );
    assert_eq!(parse_order_row(&key[..15], &message(&id, 41)), None);
    assert_eq!(parse_order_row(&key, &message(&id, 41)[..11]), None);
}

#[test]
fn post_order_keys_sort_by_stamp_before_post_position() {
    let mut keys = [order(10, 1), order(-3, 9), order(10, 0), order(1 << 40, 2), order(0, 8)];
    keys.sort();

    assert_eq!(
        keys,
        [order(-3, 9), order(0, 8), order(10, 0), order(10, 1), order(1 << 40, 2)],
        "a later post stamped earlier sorts first, which is the order the two expire in"
    );
}

#[test]
fn member_key_names_its_agent_and_starts_with_its_thread() {
    let (id, agent) = (thread_id(3), actor("host-repo-abc123.helper"));
    let key = position(&id, &agent);

    assert!(key.starts_with(&thread(&id)));
    assert!(!key.starts_with(&thread(&thread_id(4))));
    assert_eq!(parse_position(&key), Some(agent.clone()));
    assert!(key.starts_with(&positions_prefix(&id, agent.principal())));
    assert_eq!(
        parse_member(&member(&id, &agent.principal_id())),
        Some(agent.principal_id())
    );
}

#[test]
fn member_threads_prefix_of_an_agent_excludes_agents_it_is_a_prefix_of() {
    let id = thread_id(3);
    let prefix = threads_of_prefix("a");

    assert!(thread_of("a", &id).starts_with(&prefix));
    assert!(!thread_of("ab", &id).starts_with(&prefix));
    assert!(!thread_of("a.helper", &id).starts_with(&prefix));
    assert_eq!(parse_trailing_thread(&thread_of("a", &id)), Some(id));
}

#[test]
fn repo_threads_prefix_separates_repositories_and_states() {
    let id = thread_id(11);
    let repo = RepoKey::parse("github.com/acme/app").expect("repo");
    let longer = RepoKey::parse("github.com/acme/app2").expect("repo");
    let active = repo_threads_prefix(&repo, false);

    assert!(repo_thread(&repo, false, &id).starts_with(&active));
    assert!(!repo_thread(&repo, true, &id).starts_with(&active));
    assert!(!repo_thread(&longer, false, &id).starts_with(&active));
    assert_eq!(parse_trailing_thread(&repo_thread(&repo, true, &id)), Some(id));
    assert_eq!(parse_trailing_thread(&[1, 2, 3]), None);
}

#[test]
fn replay_keys_differ_by_kind_agent_thread_and_key() {
    let key = ReplayKey::parse("relay-1").expect("replay key");
    let other_key = ReplayKey::parse("relay-11").expect("replay key");
    let (a, id) = (actor("a"), thread_id(5));
    let keys = [
        replay_new_thread(&a, &key),
        replay_inbox(&a, &key),
        replay_post(&a, &id, &key),
        replay_leave(&a, &id, &key),
        replay_post(&a, &thread_id(6), &key),
        replay_post(&a, &id, &other_key),
        replay_post(&actor("b"), &id, &key),
        replay_new_thread(&actor("a.helper"), &key),
    ];

    for (index, key) in keys.iter().enumerate() {
        assert_eq!(keys.iter().position(|other| other == key), Some(index));
    }
}

#[test]
fn expiry_keys_sort_by_time_within_a_kind_and_keep_their_payload() {
    let payload = thread(&thread_id(1));
    let mut keys = [
        expiry(Expiry::Thread, 10, &payload),
        expiry(Expiry::Thread, -3, &payload),
        expiry(Expiry::Thread, 1 << 40, &payload),
        expiry(Expiry::Thread, 0, &payload),
    ];
    keys.sort();

    let times: Vec<i64> = keys.iter().filter_map(|key| Some(parse_expiry(key)?.0)).collect();
    assert_eq!(times, [-3, 0, 10, 1 << 40]);
    assert_eq!(parse_expiry(&keys[0]), Some((-3, payload.as_slice())));
    assert!(keys.iter().all(|key| key.starts_with(&expiry_prefix(Expiry::Thread))));
    assert!(!expiry(Expiry::Seen, 10, b"a").starts_with(&expiry_prefix(Expiry::Thread)));
    assert_eq!(parse_expiry(&[1, 2, 3]), None);
}
