//! Tests of the index database's open path and keyspace configuration.

use super::*;

/// Bytes written into one keyspace to provoke a rotation. Above every tier in
/// [`KEYSPACE_MEMTABLE_BYTES`], and far below fjall's unconfigured 64 MiB default — so the
/// flush this triggers happens *because* the keyspace was sized, and would not happen at the
/// default. Also well below fjall's 512 MiB `max_journaling_size`, which is the only other
/// thing that could rotate a memtable here.
const ROTATION_PROBE_BYTES: usize = 24 * 1_024 * 1_024;

fn write_until_rotation_probe(keyspace: &Keyspace) {
    let value = vec![0u8; 4_096];
    for i in 0..(ROTATION_PROBE_BYTES / value.len()) {
        keyspace
            .insert(i.to_be_bytes(), value.as_slice())
            .expect("probe insert");
    }
}

/// True once fjall has flushed at least one memtable of `keyspace` to disk. Rotation is
/// requested synchronously on insert but performed by a background worker, so this polls.
fn flushed_within(keyspace: &Keyspace, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if keyspace.disk_space() > 0 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    keyspace.disk_space() > 0
}

/// fjall has no getter for the compression policy, so observe it on disk: highly compressible
/// rows flushed to L0 (which the default policy leaves raw) must occupy far less than their
/// raw size, and must still read back after a reopen.
#[test]
fn index_keyspaces_compress_level_zero_and_round_trip() {
    const ROWS: usize = 512;
    const VALUE_LEN: usize = 4_096;
    let dir = tempfile::tempdir().unwrap();
    {
        let db = IndexDb::open(dir.path()).unwrap();
        for ks in [&db.code_bm25_postings, &db.refs_by_def, db.notes()] {
            for i in 0..ROWS {
                ks.insert(i.to_be_bytes(), vec![7u8; VALUE_LEN]).unwrap();
            }
            ks.rotate_memtable_and_wait().unwrap();
            let raw = (ROWS * VALUE_LEN) as u64;
            assert!(
                ks.disk_space() < raw / 4,
                "L0 table of {raw} raw bytes occupies {} on disk; LZ4 is not applied",
                ks.disk_space()
            );
        }
    }
    let db = IndexDb::open(dir.path()).unwrap();
    let row = db
        .code_bm25_postings
        .get(7usize.to_be_bytes())
        .unwrap()
        .expect("row survives reopen");
    assert_eq!(&*row, vec![7u8; VALUE_LEN].as_slice());
}

#[test]
fn probe_candidates_walk_up_the_ancestor_directories() {
    assert_eq!(
        probe_candidates("a/b/c.ts").collect::<Vec<_>>(),
        vec!["a/b/c.ts", "a/b", "a"]
    );
    assert_eq!(
        probe_candidates("tsconfig.json").collect::<Vec<_>>(),
        vec!["tsconfig.json"]
    );
}

#[test]
fn keyspace_memtable_table_covers_every_partition() {
    let dir = tempfile::tempdir().unwrap();
    let db = IndexDb::open(dir.path()).unwrap();
    let mut opened: Vec<String> = db.db.list_keyspace_names().iter().map(ToString::to_string).collect();
    opened.extend(db.memory().unwrap().keyspace_names());
    opened.sort();
    let mut sized: Vec<String> = KEYSPACE_MEMTABLE_BYTES
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect();
    sized.sort();
    assert_eq!(
        opened, sized,
        "every opened keyspace must be sized by the table, and the table must name no keyspace \
         that is never opened — an unlisted name silently falls back to the cold tier"
    );
}

#[test]
fn memtable_ceiling_is_bounded() {
    let partitions = u64::try_from(KEYSPACE_MEMTABLE_BYTES.len()).expect("keyspace count fits a u64");
    let unsized_ceiling = partitions * (1 + MEMTABLE_SEALED_LIMIT) * 64 * 1_024 * 1_024;
    // A const block, because both sides are constants: this then fails the build rather than a
    // test run, which is the right moment to learn that a new keyspace blew the ceiling. ~keep
    const { assert!(INDEX_MEMTABLE_CEILING_BYTES <= 1_024 * 1_024 * 1_024) };
    assert!(
        INDEX_MEMTABLE_CEILING_BYTES * 4 <= unsized_ceiling,
        "sizing must buy at least a 4x cut against fjall's 64 MiB default"
    );
}

/// The load-bearing half of the sizing: fjall persists `max_memtable_size` when the keyspace is
/// *created* and recovers it on every later open, ignoring the create options for a keyspace
/// that already exists. A test that only checked a freshly created DB would pass even if the
/// value never survived a restart — which is the state every long-lived index is actually in.
///
/// `max_memtable_size` has no public getter, so this observes the size behaviourally: write
/// more than the configured size (but far less than fjall's 64 MiB default) into a cold-tier
/// keyspace *after* reopening, and require that fjall rotated and flushed it.
#[test]
fn configured_memtable_size_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = IndexDb::open(dir.path()).unwrap();
        assert_eq!(db.notes().disk_space(), 0, "nothing written yet");
    }
    let db = IndexDb::open(dir.path()).unwrap();
    write_until_rotation_probe(db.notes());
    assert!(
        flushed_within(db.notes(), std::time::Duration::from_secs(30)),
        "a reopened keyspace must still rotate at its configured {MEMTABLE_COLD_BYTES}-byte \
         memtable size; no flush means fjall recovered the 64 MiB default instead"
    );
}

/// The read-only open writes nothing into the index it reads: an index it finds without a schema
/// version is left without one, for the writer that fills it to stamp.
#[test]
fn a_read_only_open_stamps_no_schema_version() {
    let dir = tempfile::tempdir().unwrap();
    let memory_dir = dir.path().join("memory.fjall");

    drop(IndexDb::open_tolerating_held_memory(dir.path(), &memory_dir).unwrap());
    assert_eq!(peek_index_schema_ver(dir.path()).unwrap(), None);

    drop(IndexDb::open_with_memory(dir.path(), &memory_dir).unwrap());
    assert_eq!(peek_index_schema_ver(dir.path()).unwrap(), Some(INDEX_SCHEMA_VER));
}
