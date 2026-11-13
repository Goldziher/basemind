use std::fs;
use std::io::{self, Write};
use std::path::Path;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::IndexDb;
use super::layered_shadow::DeltaManifest;
use super::writer::IndexWriter;

const MARKER: &str = "overlay.json";
const DELTA: &str = "delta.msgpack";
const SEQUENCE: &[u8] = b"delta_seq";

#[derive(Deserialize, Serialize)]
struct Marker {
    format: u16,
    base_generation: u64,
    base_commit: String,
    delta_seq: u64,
    state: State,
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum State {
    Dirty,
    Ready,
}

fn commit(
    path: &Path,
    db: &IndexDb,
    delta: &DeltaManifest,
    stage_rows: impl FnOnce(&mut IndexWriter) -> Result<()>,
) -> Result<()> {
    ensure!(delta.format == 1, "unsupported delta format");
    let current = read(path, db, delta.base_generation, &delta.base_commit)?;
    let previous = current.as_ref().map_or(0, |delta| delta.seq);
    ensure!(delta.seq > previous, "overlay sequence must advance");
    ensure!(
        current.is_some() || !path.join(MARKER).exists(),
        "discard incomplete overlay before writing"
    );
    let mut marker = Marker {
        format: delta.format,
        base_generation: delta.base_generation,
        base_commit: delta.base_commit.clone(),
        delta_seq: delta.seq,
        state: State::Dirty,
    };
    replace(path, MARKER, &serde_json::to_vec(&marker)?)?;
    let mut writer = db.writer();
    stage_rows(&mut writer)?;
    writer.commit_overlay(SEQUENCE, delta.seq)?;
    replace(path, DELTA, &rmp_serde::to_vec_named(delta)?)?;
    marker.state = State::Ready;
    replace(path, MARKER, &serde_json::to_vec(&marker)?)?;
    Ok(())
}

fn read(path: &Path, db: &IndexDb, generation: u64, commit: &str) -> Result<Option<DeltaManifest>> {
    let bytes = match fs::read(path.join(MARKER)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Ok(marker) = serde_json::from_slice::<Marker>(&bytes) else {
        return Ok(None);
    };
    if marker.format != 1
        || marker.state != State::Ready
        || marker.base_generation != generation
        || marker.base_commit != commit
    {
        return Ok(None);
    }
    let Some(bytes) = db.meta.get(SEQUENCE)? else {
        return Ok(None);
    };
    if bytes.as_ref() != marker.delta_seq.to_be_bytes() {
        return Ok(None);
    }
    let bytes = match fs::read(path.join(DELTA)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Ok(delta) = rmp_serde::from_slice::<DeltaManifest>(&bytes) else {
        return Ok(None);
    };
    Ok((delta.format == marker.format
        && delta.seq == marker.delta_seq
        && delta.base_generation == generation
        && delta.base_commit == commit)
        .then_some(delta))
}

fn replace(root: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(root)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(root.join(name))?;
    #[cfg(unix)]
    fs::File::open(root)?.sync_all()?;
    Ok(())
}

#[test]
fn rejects_ready_marker_with_mismatched_fjall_or_delta_sequence() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();
    let db = IndexDb::open(path).unwrap();
    let first = DeltaManifest::empty(7, "commit".into(), 1);
    commit(path, &db, &first, |_| Ok(())).unwrap();
    let second = DeltaManifest::empty(7, "commit".into(), 2);

    replace(path, DELTA, &rmp_serde::to_vec_named(&second).unwrap()).unwrap();
    assert!(read(path, &db, 7, "commit").unwrap().is_none());
    replace(path, DELTA, &rmp_serde::to_vec_named(&first).unwrap()).unwrap();
    assert_eq!(read(path, &db, 7, "commit").unwrap().unwrap().seq, 1);

    db.writer().commit_overlay(SEQUENCE, 2).unwrap();
    assert!(read(path, &db, 7, "commit").unwrap().is_none());
}

#[test]
fn accepts_only_ready_matching_overlay_commits() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();
    let db = IndexDb::open(path).unwrap();
    let first = DeltaManifest::empty(7, "commit".into(), 1);
    commit(path, &db, &first, |writer| {
        writer.upsert_grep_bloom("hash", b"value".to_vec());
        Ok(())
    })
    .unwrap();
    assert_eq!(read(path, &db, 7, "commit").unwrap().unwrap().seq, 1);
    assert!(read(path, &db, 8, "commit").unwrap().is_none());
    assert_eq!(&*db.grep_bloom.get(b"hash").unwrap().unwrap(), b"value");
    assert!(commit(path, &db, &first, |_| Ok(())).is_err());

    let second = DeltaManifest::empty(7, "commit".into(), 2);
    assert!(commit(path, &db, &second, |_| anyhow::bail!("injected interruption")).is_err());
    assert!(read(path, &db, 7, "commit").unwrap().is_none());
    assert!(commit(path, &db, &second, |_| Ok(())).is_err());
}
