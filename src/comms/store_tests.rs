use super::*;
use crate::comms::model::AgentCard;

fn temp_store() -> (tempfile::TempDir, CommsStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = CommsStore::open(dir.path()).expect("open store");
    (dir, store)
}

fn thread_id(s: &str) -> ThreadId {
    ThreadId::parse(s).expect("thread")
}

fn agent_id(s: &str) -> AgentId {
    AgentId::parse(s).expect("agent")
}

fn sample_thread(id: &str) -> Thread {
    Thread {
        id: thread_id(id),
        subject: Some("topic".to_string()),
        path: None,
        members: vec![agent_id("a")],
        creator: agent_id("a"),
        active: true,
        created_at: now_micros(),
        last_activity: 0,
    }
}

#[test]
fn post_then_history_returns_meta_and_body_is_not_loaded() {
    let (_d, store) = temp_store();
    let thread = thread_id("th-1");
    store.put_thread(&sample_thread("th-1")).expect("put thread");

    let body = b"the quick brown fox".to_vec();
    let meta = build_meta(
        "m-1".to_string(),
        thread.clone(),
        agent_id("agent-1"),
        "subj".to_string(),
        vec![],
        None,
        &body,
    );
    let (seq, _) = store
        .post(&thread, meta.clone(), MessageBody(body.clone()))
        .expect("post");
    assert_eq!(seq, 1, "first message in a thread gets seq 1");

    let page = store.history(&thread, 0, 10).expect("history");
    assert_eq!(page.messages.len(), 1);
    let (got_seq, got) = &page.messages[0];
    assert_eq!(*got_seq, 1);
    assert_eq!(got.id, "m-1");
    assert_eq!(got.subject, "subj");
    assert_eq!(got.body_len as usize, body.len());
    assert_eq!(got.body_sha, body_hash_hex(&body));

    let fetched = store.get_body("m-1").expect("get_body");
    assert_eq!(fetched.as_deref(), Some(body.as_slice()));
    assert_eq!(store.get_body("nope").expect("get_body"), None);
}

#[test]
fn history_paginates_by_seq() {
    let (_d, store) = temp_store();
    let thread = thread_id("th-1");
    for i in 0..5u32 {
        let body = format!("body-{i}").into_bytes();
        let meta = build_meta(
            format!("m-{i}"),
            thread.clone(),
            agent_id("a"),
            format!("s-{i}"),
            vec![],
            None,
            &body,
        );
        store.post(&thread, meta, MessageBody(body)).expect("post");
    }
    let page1 = store.history(&thread, 0, 2).expect("history");
    assert_eq!(page1.messages.len(), 2);
    assert!(page1.more);
    let page2 = store.history(&thread, page1.last_seq, 2).expect("history");
    assert_eq!(page2.messages.len(), 2);
    assert_eq!(page2.messages[0].1.id, "m-2");
}

#[test]
fn seq_counter_persists_across_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let thread = thread_id("th-1");
    let post = |store: &CommsStore, id: &str| {
        let body = id.as_bytes().to_vec();
        let meta = build_meta(
            id.to_string(),
            thread.clone(),
            agent_id("a"),
            id.to_string(),
            vec![],
            None,
            &body,
        );
        store.post(&thread, meta, MessageBody(body)).expect("post").0
    };
    {
        let store = CommsStore::open(dir.path()).expect("open");
        assert_eq!(post(&store, "m-1"), 1);
        assert_eq!(post(&store, "m-2"), 2);
    }
    {
        let store = CommsStore::open(dir.path()).expect("reopen");
        assert_eq!(post(&store, "m-3"), 3, "seq must continue past reopen");
        let page = store.history(&thread, 0, 10).expect("history");
        assert_eq!(page.messages.len(), 3);
        let ids: Vec<&str> = page.messages.iter().map(|(_, m)| m.id.as_str()).collect();
        assert_eq!(ids, ["m-1", "m-2", "m-3"]);
    }
}

#[test]
fn prune_expired_deletes_old_messages_and_bodies_but_keeps_recent() {
    let (_d, store) = temp_store();
    let thread = thread_id("th-1");
    let stale_body = b"stale".to_vec();
    let mut stale = build_meta(
        "old".to_string(),
        thread.clone(),
        agent_id("a"),
        "old".to_string(),
        vec![],
        None,
        &stale_body,
    );
    stale.ts_micros = now_micros() - 10 * 24 * 60 * 60 * 1_000_000;
    store.post(&thread, stale, MessageBody(stale_body)).expect("post stale");
    let fresh_body = b"fresh".to_vec();
    let fresh = build_meta(
        "new".to_string(),
        thread.clone(),
        agent_id("a"),
        "new".to_string(),
        vec![],
        None,
        &fresh_body,
    );
    store.post(&thread, fresh, MessageBody(fresh_body)).expect("post fresh");

    let pruned = store
        .prune_expired(std::time::Duration::from_secs(24 * 60 * 60))
        .expect("prune");
    assert_eq!(pruned, 1, "exactly the stale message is pruned");

    let page = store.history(&thread, 0, 10).expect("history");
    let ids: Vec<&str> = page.messages.iter().map(|(_, m)| m.id.as_str()).collect();
    assert_eq!(ids, ["new"]);
    assert_eq!(store.get_body("old").expect("get_body"), None);
}

#[test]
fn post_keeps_only_the_newest_messages_per_thread() {
    let (_d, store) = temp_store();
    let thread = thread_id("th-capped");
    for seq in 0..=MAX_MESSAGES_PER_THREAD {
        let id = format!("m-{seq}");
        let body = id.as_bytes().to_vec();
        let meta = build_meta(id.clone(), thread.clone(), agent_id("a"), id, vec![], None, &body);
        store.post(&thread, meta, MessageBody(body)).expect("post");
    }

    let page = store.history(&thread, 0, usize::MAX).expect("history");
    assert_eq!(page.messages.len(), MAX_MESSAGES_PER_THREAD as usize);
    assert_eq!(page.messages.first().unwrap().1.id, "m-1");
    assert_eq!(store.get_body("m-0").expect("old body"), None);
}

#[test]
fn active_thread_count_is_bounded() {
    let (_d, store) = temp_store();
    for index in 0..MAX_ACTIVE_THREADS {
        store
            .put_thread(&sample_thread(&format!("th-{index}")))
            .expect("thread below ceiling");
    }
    let error = store
        .put_thread(&sample_thread("th-overflow"))
        .expect_err("thread ceiling");
    assert!(matches!(error, CommsStoreError::Limit("maximum active thread count")));
}

#[test]
fn stale_generated_agent_cards_are_pruned_but_explicit_agents_survive() {
    let (_d, store) = temp_store();
    let stale_at = now_micros() - 4 * 24 * 60 * 60 * 1_000_000;
    for id in ["session-stale", "named-agent"] {
        store
            .put_agent(&AgentRecord {
                agent_id: agent_id(id),
                card: AgentCard::default(),
                kind: super::super::model::AgentKind::Other,
                first_seen: stale_at,
                last_seen: stale_at,
            })
            .expect("put agent");
    }

    assert_eq!(store.prune_ephemeral_agents(EPHEMERAL_AGENT_TTL).expect("prune"), 1);
    assert!(
        store
            .get_agent(&agent_id("session-stale"))
            .expect("get stale")
            .is_none()
    );
    assert!(store.get_agent(&agent_id("named-agent")).expect("get named").is_some());
}

#[test]
fn agent_activity_touch_is_throttled() {
    let (_d, store) = temp_store();
    let id = agent_id("session-active");
    let stale_at = now_micros() - 2 * 60 * 60 * 1_000_000;
    store
        .put_agent(&AgentRecord {
            agent_id: id.clone(),
            card: AgentCard::default(),
            kind: super::super::model::AgentKind::Other,
            first_seen: stale_at,
            last_seen: stale_at,
        })
        .expect("put agent");

    assert!(
        store
            .touch_agent_if_stale(&id, AGENT_TOUCH_INTERVAL)
            .expect("first touch")
    );
    assert!(
        !store
            .touch_agent_if_stale(&id, AGENT_TOUCH_INTERVAL)
            .expect("throttled touch")
    );
}

#[test]
fn archive_idle_flips_only_stale_active_threads() {
    let (_d, store) = temp_store();
    let mut stale = sample_thread("stale");
    stale.last_activity = now_micros() - 30 * 24 * 60 * 60 * 1_000_000;
    store.put_thread(&stale).expect("put stale");
    let mut fresh = sample_thread("fresh");
    fresh.last_activity = now_micros();
    store.put_thread(&fresh).expect("put fresh");

    let archived = store
        .archive_idle(std::time::Duration::from_secs(14 * 24 * 60 * 60))
        .expect("archive");
    assert_eq!(archived, 1, "only the stale thread archives");
    assert!(!store.get_thread(&thread_id("stale")).unwrap().unwrap().active);
    assert!(store.get_thread(&thread_id("fresh")).unwrap().unwrap().active);

    assert_eq!(
        store
            .archive_idle(std::time::Duration::from_secs(14 * 24 * 60 * 60))
            .expect("archive again"),
        0
    );
}

#[test]
fn purge_archived_reaps_stale_archived_threads_and_all_their_rows_only() {
    let (_d, store) = temp_store();
    let ttl = std::time::Duration::from_secs(30 * 24 * 60 * 60);
    let sixty_days_micros = 60 * 24 * 60 * 60 * 1_000_000i64;

    let stale_id = thread_id("th-stale");
    let mut stale = sample_thread("th-stale");
    stale.active = false;
    stale.last_activity = now_micros() - sixty_days_micros;
    store.put_thread(&stale).expect("put stale");
    store
        .add_member(&Membership {
            agent_id: agent_id("a"),
            thread: stale_id.clone(),
            created_at: now_micros() - sixty_days_micros,
        })
        .expect("add member");
    let body = b"stale-msg".to_vec();
    let meta = build_meta(
        "s-1".to_string(),
        stale_id.clone(),
        agent_id("a"),
        "s".to_string(),
        vec![],
        None,
        &body,
    );
    store.post(&stale_id, meta, MessageBody(body)).expect("post stale");
    store.set_read_cursor(&agent_id("a"), &stale_id, 1).expect("cursor");

    let mut fresh_archived = sample_thread("th-fresh-archived");
    fresh_archived.active = false;
    fresh_archived.last_activity = now_micros();
    store.put_thread(&fresh_archived).expect("put fresh-archived");

    let mut active_old = sample_thread("th-active-old");
    active_old.last_activity = now_micros() - sixty_days_micros;
    store.put_thread(&active_old).expect("put active-old");

    let purged = store.purge_archived(ttl).expect("purge");
    assert_eq!(purged, 1, "only the stale ARCHIVED thread is purged");

    assert!(
        store.get_thread(&stale_id).expect("get stale").is_none(),
        "stale thread row deleted"
    );
    assert!(
        store.get_body("s-1").expect("get body").is_none(),
        "stale message body deleted"
    );
    assert_eq!(
        store.history(&stale_id, 0, 10).expect("history").messages.len(),
        0,
        "stale message front-matter deleted"
    );
    assert!(
        store.members(&stale_id).expect("members").is_empty(),
        "stale membership + subs deleted"
    );
    assert_eq!(
        store.read_cursor(&agent_id("a"), &stale_id).expect("cursor"),
        0,
        "stale read cursor deleted"
    );

    assert!(
        store
            .get_thread(&thread_id("th-fresh-archived"))
            .expect("get")
            .is_some(),
        "recently-archived thread survives the retention window"
    );
    assert!(
        store.get_thread(&thread_id("th-active-old")).expect("get").is_some(),
        "an active thread is never purged, however stale"
    );

    assert_eq!(store.purge_archived(ttl).expect("purge again"), 0);
}

#[test]
fn membership_round_trips() {
    let (_d, store) = temp_store();
    let thread = thread_id("th-1");
    let agent = agent_id("agent-1");
    store
        .add_member(&Membership {
            agent_id: agent.clone(),
            thread: thread.clone(),
            created_at: now_micros(),
        })
        .expect("add");
    assert!(store.is_member(&thread, &agent).expect("is_member"));
    assert_eq!(store.members(&thread).expect("members"), vec![agent.clone()]);
    assert_eq!(store.threads_for_agent(&agent).expect("threads"), vec![thread.clone()]);
    store.remove_member(&thread, &agent).expect("remove");
    assert!(store.members(&thread).expect("members").is_empty());
    assert!(!store.is_member(&thread, &agent).expect("is_member"));
}

#[test]
fn read_cursor_is_monotonic() {
    let (_d, store) = temp_store();
    let thread = thread_id("th-1");
    let agent = agent_id("agent-1");
    assert_eq!(store.read_cursor(&agent, &thread).expect("read"), 0);
    store.set_read_cursor(&agent, &thread, 5).expect("set");
    assert_eq!(store.read_cursor(&agent, &thread).expect("read"), 5);
    store.set_read_cursor(&agent, &thread, 3).expect("set");
    assert_eq!(store.read_cursor(&agent, &thread).expect("read"), 5);
}

#[test]
fn resolve_ids_maps_each_id_to_its_thread_and_seq() {
    let (_d, store) = temp_store();
    let thread_a = thread_id("th-a");
    let thread_b = thread_id("th-b");
    let mk = |store: &CommsStore, thread: &ThreadId, id: &str| {
        let body = id.as_bytes().to_vec();
        let meta = build_meta(
            id.to_string(),
            thread.clone(),
            agent_id("a"),
            id.to_string(),
            vec![],
            None,
            &body,
        );
        store.post(thread, meta, MessageBody(body)).expect("post").0
    };
    let s_a1 = mk(&store, &thread_a, "m-a1");
    let _s_a2 = mk(&store, &thread_a, "m-a2");
    let s_b1 = mk(&store, &thread_b, "m-b1");

    let mut got = store
        .resolve_ids(&["m-a1".to_string(), "m-b1".to_string(), "ghost".to_string()])
        .expect("resolve_ids");
    got.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(
        got,
        vec![
            ("m-a1".to_string(), thread_a.clone(), s_a1),
            ("m-b1".to_string(), thread_b.clone(), s_b1),
        ]
    );
    assert!(store.resolve_ids(&[]).expect("resolve_ids").is_empty());
}

#[test]
fn agent_records_round_trip() {
    let (_d, store) = temp_store();
    let rec = AgentRecord {
        agent_id: agent_id("agent-1"),
        card: AgentCard {
            name: "n".to_string(),
            description: "d".to_string(),
            version: "1".to_string(),
            skills: vec![],
        },
        kind: super::super::model::AgentKind::Cli,
        first_seen: now_micros(),
        last_seen: now_micros(),
    };
    store.put_agent(&rec).expect("put");
    assert_eq!(store.get_agent(&agent_id("agent-1")).expect("get"), Some(rec));
}
