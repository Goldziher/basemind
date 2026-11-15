//! What survives a reopen and how layout versions are handled.

use std::fs;

use fjall::OwnedWriteBatch;

use super::*;

const MARKER: &[u8] = b"migrated_by";

fn first_step(ks: &Keyspaces, batch: &mut OwnedWriteBatch) -> Result<(), CommsError> {
    batch.insert(&ks.meta, MARKER, b"first");
    Ok(())
}

fn second_step(ks: &Keyspaces, batch: &mut OwnedWriteBatch) -> Result<(), CommsError> {
    let seen_first = ks
        .meta
        .get(MARKER)
        .expect("read")
        .is_some_and(|value| &*value == b"first");
    batch.insert(
        &ks.meta,
        MARKER,
        if seen_first {
            &b"first,second"[..]
        } else {
            &b"second"[..]
        },
    );
    Ok(())
}

fn failing_step(ks: &Keyspaces, batch: &mut OwnedWriteBatch) -> Result<(), CommsError> {
    batch.insert(&ks.meta, MARKER, b"half");
    Err(CommsError::Backend {
        detail: "step failed".to_string(),
    })
}

/// The store's directory as an error message names it.
fn store_dir(harness: &Harness) -> String {
    harness.dir.path().join(STORE_DIR).display().to_string()
}

fn marker(harness: &Harness) -> Option<Vec<u8>> {
    harness
        .store()
        .ks
        .meta
        .get(MARKER)
        .expect("read")
        .map(|value| value.to_vec())
}

#[test]
fn fresh_directory_is_created_at_the_current_version() {
    let harness = Harness::new();

    assert_eq!(harness.store().store_version(), COMMS_STORE_VER);
    assert!(harness.dir.path().join(STORE_DIR).is_dir());
    assert_eq!(harness.arrivals(), 0);
}

#[test]
fn reopening_a_dropped_handle_keeps_messages_positions_counters_and_replays() {
    let mut harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b", "c"]);
    let relayed = harness
        .post(&keyed("a", "relay-1"), into(&created.thread, "second"))
        .expect("post");
    let delivered = harness
        .inbox(&keyed("b", "relay-2"), InboxBody::default())
        .expect("inbox");
    harness
        .inbox(
            &caller("c"),
            InboxBody {
                mark: Mark::Notify,
                limit: Some(1),
                ..InboxBody::default()
            },
        )
        .expect("inbox");
    let arrivals = harness.arrivals();

    harness.reopen();

    assert_eq!(harness.arrivals(), arrivals);
    assert_eq!(harness.position(&created.thread, &caller("b").actor), Some((2, 2)));
    assert_eq!(harness.position(&created.thread, &caller("c").actor), Some((0, 1)));
    assert_eq!(
        harness.post(&keyed("a", "relay-1"), into(&created.thread, "second")),
        Ok(relayed)
    );
    assert_eq!(
        harness.inbox(&keyed("b", "relay-2"), InboxBody::default()),
        Ok(delivered)
    );
    assert_eq!(harness.arrivals(), arrivals, "a replay stores nothing");

    let third = say(&harness, "a", &created.thread, "third");
    assert_eq!(third.id.seq(), 3);
    assert_eq!(harness.arrivals(), arrivals + 1);
    let whole = harness.inbox(&caller("c"), history()).expect("inbox");
    assert_eq!(bodies(&whole), ["first", "second", "third"]);
    assert!(whole.messages[1].ts_micros < whole.messages[2].ts_micros);
    assert_eq!(start(&harness, "a", "again", &["b", "d"]).pending, [caller("d").actor]);
}

/// That the flush reached the disk cannot be shown from inside the process that wrote: a reopen
/// here reads the same operating-system buffers. This only holds `flush` to returning `Ok` and
/// to leaving the store readable.
#[test]
fn flush_returns_ok_and_leaves_the_store_readable_after_a_reopen() {
    let mut harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);

    harness.store().flush().expect("flush");
    harness.reopen();

    assert_eq!(
        bodies(&harness.inbox(&caller("b"), InboxBody::default()).expect("inbox")),
        ["first"]
    );
    assert_eq!(harness.position(&created.thread, &caller("b").actor), Some((1, 1)));
}

#[test]
fn older_store_is_migrated_step_by_step_and_keeps_its_data() {
    let mut harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);
    harness.store().force_store_version(COMMS_STORE_VER - 2);
    let steps = [
        Migration {
            from: COMMS_STORE_VER - 1,
            to: COMMS_STORE_VER,
            run: second_step,
        },
        Migration {
            from: COMMS_STORE_VER - 2,
            to: COMMS_STORE_VER - 1,
            run: first_step,
        },
    ];

    harness.reopen_migrating(&steps).expect("migrated");

    assert_eq!(harness.store().store_version(), COMMS_STORE_VER);
    assert_eq!(marker(&harness).as_deref(), Some(&b"first,second"[..]));
    assert_eq!(
        bodies(&harness.inbox(&caller("b"), InboxBody::default()).expect("inbox")),
        ["first"]
    );
    say(&harness, "a", &created.thread, "after the migration");

    harness.reopen_migrating(&steps).expect("nothing left to migrate");
    assert_eq!(marker(&harness).as_deref(), Some(&b"first,second"[..]));
}

#[test]
fn failed_step_leaves_the_store_at_the_version_it_reached() {
    let mut harness = Harness::new();
    start(&harness, "a", "topic", &["b"]);
    harness.store().force_store_version(COMMS_STORE_VER - 2);
    let steps = [
        Migration {
            from: COMMS_STORE_VER - 2,
            to: COMMS_STORE_VER - 1,
            run: first_step,
        },
        Migration {
            from: COMMS_STORE_VER - 1,
            to: COMMS_STORE_VER,
            run: failing_step,
        },
    ];

    let refused = harness.reopen_migrating(&steps).expect_err("the second step fails");
    assert!(refused.to_string().contains("step failed"), "{refused}");

    let resumed = [Migration {
        from: COMMS_STORE_VER - 1,
        to: COMMS_STORE_VER,
        run: second_step,
    }];
    harness
        .reopen_migrating(&resumed)
        .expect("only the step that failed runs again");
    assert_eq!(marker(&harness).as_deref(), Some(&b"first,second"[..]));
}

#[test]
fn older_store_without_a_migration_path_is_refused_and_left_as_it_was() {
    let mut harness = Harness::new();
    start(&harness, "a", "topic", &["b"]);
    harness.store().force_store_version(COMMS_STORE_VER - 1);

    let refused = harness.reopen_migrating(&[]).expect_err("no step from this version");

    assert!(matches!(refused, CommsError::Backend { .. }), "{refused:?}");
    assert!(refused.to_string().contains("no migration"), "{refused}");
    assert!(refused.to_string().contains(&store_dir(&harness)), "{refused}");
    let step = [Migration {
        from: COMMS_STORE_VER - 1,
        to: COMMS_STORE_VER,
        run: first_step,
    }];
    harness.reopen_migrating(&step).expect("the store was not touched");
    assert_eq!(
        bodies(&harness.inbox(&caller("b"), InboxBody::default()).expect("inbox")),
        ["first"]
    );
}

#[test]
fn unreadable_layout_version_or_counter_is_refused_naming_the_directory() {
    for (key, what) in [(META_STORE_VER, "layout version"), (META_POSTS, "post counter")] {
        let mut harness = Harness::new();
        start(&harness, "a", "topic", &["b"]);
        harness.store().ks.meta.insert(key, [1, 2, 3]).expect("write");

        let refused = harness.reopen_migrating(&[]).expect_err("three bytes are no number");

        assert!(matches!(refused, CommsError::Backend { .. }), "{refused:?}");
        let message = refused.to_string();
        assert!(
            message.contains(what) && message.contains(&store_dir(&harness)),
            "{message}"
        );
    }
}

#[test]
fn newer_store_opens_for_reading_refuses_writes_and_is_never_changed() {
    let mut harness = Harness::new();
    let created = start(&harness, "a", "topic", &["b"]);
    harness.store().force_store_version(COMMS_STORE_VER + 3);
    harness.reopen();
    let rows = harness.store().stored_rows();
    let newer = CommsError::StoreNewer {
        store: COMMS_STORE_VER + 3,
        binary: COMMS_STORE_VER,
    };

    assert_eq!(harness.store().store_version(), COMMS_STORE_VER + 3);
    assert_eq!(
        harness.post(&caller("a"), into(&created.thread, "late")),
        Err(newer.clone())
    );
    assert_eq!(harness.inbox(&caller("b"), InboxBody::default()), Err(newer.clone()));
    assert_eq!(
        harness.threads_update(
            &caller("b"),
            ThreadUpdate {
                thread: created.thread.clone(),
                action: ThreadAction::Leave,
            }
        ),
        Err(newer.clone())
    );
    assert_eq!(harness.store().sweep(), Err(newer));
    assert_eq!(bodies(&harness.inbox(&caller("b"), peek()).expect("inbox")), ["first"]);
    assert_eq!(
        bodies(&harness.inbox(&caller("b"), history()).expect("inbox")),
        ["first"]
    );
    assert_eq!(
        harness
            .threads_query(&caller("b"), ThreadQuery::default())
            .expect("threads")
            .threads
            .len(),
        1
    );

    harness.clock.advance(MESSAGE_TTL + Duration::from_secs(1));
    assert_eq!(bodies(&harness.inbox(&caller("b"), peek()).expect("inbox")), ["first"]);
    harness.reopen();
    assert_eq!(harness.store().store_version(), COMMS_STORE_VER + 3);
    assert_eq!(harness.store().stored_rows(), rows, "nothing is removed or recorded");
}

#[test]
fn store_that_cannot_be_opened_is_a_backend_error_and_is_left_in_place() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let blocked = dir.path().join(STORE_DIR);
    fs::write(&blocked, b"not a store").expect("write");

    let refused = FjallAgents::open(dir.path()).err().expect("a file is not a store");

    assert!(matches!(refused, CommsError::Backend { .. }), "{refused:?}");
    assert!(refused.to_string().contains("left untouched"), "{refused}");
    assert_eq!(fs::read(&blocked).expect("read"), b"not a store");
}

#[test]
fn store_with_rows_but_no_version_is_refused_and_left_in_place() {
    let mut harness = Harness::new();
    start(&harness, "a", "topic", &["b"]);
    harness.store().ks.meta.remove(META_STORE_VER).expect("remove");
    let rows = harness.store().stored_rows();

    let refused = harness.reopen_migrating(&[]).expect_err("no version");
    assert!(refused.to_string().contains("no layout version"), "{refused}");

    let reopened = FjallAgents::open_with(harness.dir.path(), options(&harness.clock, Vec::new()));
    assert!(reopened.is_err(), "a second attempt does not adopt the store either");
    drop(reopened);
    let db = fjall::Database::builder(harness.dir.path().join(STORE_DIR))
        .open()
        .expect("the directory still holds the database");
    let threads = db
        .keyspace("threads", fjall::KeyspaceCreateOptions::default)
        .expect("keyspace");
    assert_eq!(threads.len().expect("count"), rows[0]);
}

#[test]
fn newer_store_without_a_keyspace_of_this_layout_is_refused_and_gains_none() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join(STORE_DIR);
    {
        let db = fjall::Database::builder(&path).open().expect("create");
        let meta = db
            .keyspace("meta", fjall::KeyspaceCreateOptions::default)
            .expect("keyspace");
        meta.insert(META_STORE_VER, (COMMS_STORE_VER + 1).to_be_bytes())
            .expect("write");
        meta.insert(META_POSTS, b"another encoding").expect("write");
    }

    let refused = FjallAgents::open(dir.path())
        .err()
        .expect("a layout this binary cannot read");

    assert!(refused.to_string().contains("keyspace"), "{refused}");
    let db = fjall::Database::builder(&path).open().expect("reopen");
    assert_eq!(db.keyspace_count(), 1, "no keyspace was created in the newer store");
}

#[test]
fn newer_store_with_counters_in_another_encoding_still_opens_for_reading() {
    let mut harness = Harness::new();
    start(&harness, "a", "topic", &["b"]);
    harness.store().force_store_version(COMMS_STORE_VER + 1);
    harness
        .store()
        .ks
        .meta
        .insert(META_POSTS, b"another encoding")
        .expect("write");

    harness.reopen();

    assert_eq!(bodies(&harness.inbox(&caller("b"), peek()).expect("inbox")), ["first"]);
}

/// The rows of one keyspace, as `(key, value)`.
type Rows<'a> = &'a [(&'a [u8], &'a [u8])];

/// A directory holding `keyspaces`, made the way another program or an interrupted start would
/// have left it. Each is `(name, rows)`.
fn directory_with(keyspaces: &[(&str, Rows<'_>)]) -> TempDir {
    let dir = tempfile::tempdir().expect("temporary directory");
    let db = fjall::Database::builder(dir.path().join(STORE_DIR))
        .open()
        .expect("create");
    for (name, rows) in keyspaces {
        let keyspace = db
            .keyspace(name, fjall::KeyspaceCreateOptions::default)
            .expect("keyspace");
        for (key, value) in *rows {
            keyspace.insert(*key, *value).expect("write");
        }
    }
    dir
}

fn keyspaces_in(dir: &TempDir) -> usize {
    fjall::Database::builder(dir.path().join(STORE_DIR))
        .open()
        .expect("the directory still holds the database")
        .keyspace_count()
}

#[test]
fn store_at_the_current_version_without_a_keyspace_is_refused_and_gains_none() {
    let version = COMMS_STORE_VER.to_be_bytes();
    let dir = directory_with(&[("meta", &[(META_STORE_VER, &version)]), ("threads", &[])]);

    let refused = FjallAgents::open(dir.path())
        .err()
        .expect("a store is not completed by whoever opens it");

    assert!(matches!(refused, CommsError::Backend { .. }), "{refused:?}");
    let message = refused.to_string();
    assert!(
        message.contains("no `messages` keyspace") && message.contains("left untouched"),
        "{message}"
    );
    assert_eq!(keyspaces_in(&dir), 2, "no keyspace was created");
}

#[test]
fn rows_without_a_layout_version_are_refused_whichever_keyspace_holds_them() {
    let dir = directory_with(&[("messages", &[(b"key", b"value")]), ("threads", &[])]);

    let refused = FjallAgents::open(dir.path()).err().expect("rows nobody vouches for");

    assert!(refused.to_string().contains("no layout version"), "{refused}");
    assert_eq!(keyspaces_in(&dir), 2, "no keyspace was created, `meta` included");
}

#[test]
fn keyspace_of_another_layout_without_a_version_is_refused() {
    let dir = directory_with(&[("somebody_elses", &[])]);

    let refused = FjallAgents::open(dir.path()).err().expect("not a comms store");

    assert!(refused.to_string().contains("another layout"), "{refused}");
    assert_eq!(keyspaces_in(&dir), 1);
}

#[test]
fn creation_interrupted_before_the_version_was_written_is_completed() {
    let dir = directory_with(&[("meta", &[]), ("threads", &[]), ("expiry", &[])]);

    let store = FjallAgents::open(dir.path()).expect("empty keyspaces and no version hold no store");

    assert_eq!(store.store_version(), COMMS_STORE_VER);
    let body = PostBody {
        target: PostTarget::NewThread {
            subject: "topic".to_string(),
            to: Vec::new(),
            paths: Vec::new(),
        },
        body: "first".to_string(),
    };
    assert_eq!(store.post(&caller("a"), body).expect("post").id.seq(), 1);
}
