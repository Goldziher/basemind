//! [`MapCache::refreshed`] against [`MapCache::build`]: a refresh must map a rescanned store
//! exactly as a rebuild would, while decoding only the outlines the scan changed.
//!
//! The blob store is shared by every test in the process and the epoch step rewrites a blob in
//! place, so every source here carries a token no other test scans.

use std::path::Path;
use std::sync::Arc;

use super::MapCache;
use crate::extract::FileMapL1;
use crate::path::RelPath;
use crate::store::{Store, VIEW_WORKING};

fn scan(root: &Path, store: &mut Store) {
    let cfg = crate::config::ConfigV1::with_defaults();
    crate::scanner::scan(
        root,
        store,
        &cfg,
        crate::scanner::ScanSource::WorkingTree,
        crate::scanner::EmbedMode::Inline,
        crate::retrieval_wiring::backend(),
    )
    .expect("scan");
}

/// One symbol of a file's term index: its name and kind.
type TermSymbol = (Vec<u8>, crate::extract::SymbolKind);

fn names(l1: &FileMapL1) -> Vec<String> {
    l1.symbols.iter().map(|s| s.name.clone()).collect()
}

/// Everything a reader can observe of a map: its file view, document paths, every
/// outline and the term index.
fn assert_same(refreshed: &MapCache, built: &MapCache, step: &str) {
    let view: Vec<_> = refreshed.file_metas().collect();
    assert_eq!(view, built.file_metas().collect::<Vec<_>>(), "{step}: file view");
    assert_eq!(refreshed.doc_paths, built.doc_paths, "{step}: document paths");
    for (path, _) in &view {
        assert_eq!(
            refreshed.get(path).map(|l1| names(&l1)),
            built.get(path).map(|l1| names(&l1)),
            "{step}: outline of {path}"
        );
    }
    let terms = |cache: &MapCache| -> Vec<(RelPath, Vec<TermSymbol>)> {
        cache
            .terms()
            .iter()
            .map(|(path, terms)| {
                let symbols = terms.symbols().map(|(_, name, kind)| (name.to_vec(), kind)).collect();
                (path.clone(), symbols)
            })
            .collect()
    };
    assert_eq!(terms(refreshed), terms(built), "{step}: term index");
}

/// A small deterministic generator, so a failing sequence replays from its seed. Every write takes
/// a fresh `serial`, so no two paths share content and the epoch step's in-place blob rewrite
/// touches exactly the one path it restamps.
struct Steps(u64);

impl Steps {
    fn next(&mut self, below: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % below as u64) as usize
    }
}

fn source(token: &str, n: usize) -> String {
    format!("pub fn {token}_{n}() {{}}\npub struct {token}_type_{n};\n")
}

/// Random edit, add, delete, rename, touch, document and extract-epoch sequences: after every
/// step the refreshed map equals a map built from scratch over the same store.
#[test]
fn a_refreshed_map_equals_a_rebuilt_one() {
    crate::store::init_isolated_cache();
    for seed in [0x9e37_79b9_7f4a_7c15_u64, 0x2545_f491_4f6c_dd1d] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let token = format!("delta_equiv_{:x}", seed & 0xffff);
        let mut steps = Steps(seed);
        let mut live: Vec<String> = Vec::new();
        let mut serial = 0;
        for _ in 0..6 {
            let name = format!("f{serial}.rs");
            std::fs::write(root.join(&name), source(&token, serial)).unwrap();
            live.push(name);
            serial += 1;
        }
        let mut store = Store::open(root, VIEW_WORKING).unwrap();
        scan(root, &mut store);
        let mut cache = MapCache::build(&store, 0);
        cache.terms();

        for step in 0..24 {
            let op = steps.next(7);
            let pick = steps.next(live.len().max(1));
            let label = format!("seed {seed:x} step {step} op {op}");
            match op {
                0 if !live.is_empty() => {
                    serial += 1;
                    std::fs::write(root.join(&live[pick]), source(&token, serial)).unwrap();
                }
                1 => {
                    serial += 1;
                    let name = format!("f{serial}.rs");
                    std::fs::write(root.join(&name), source(&token, serial)).unwrap();
                    live.push(name);
                }
                2 if live.len() > 1 => {
                    std::fs::remove_file(root.join(live.swap_remove(pick))).unwrap();
                }
                3 if !live.is_empty() => {
                    let renamed = format!("r{serial}.rs");
                    serial += 1;
                    std::fs::rename(root.join(&live[pick]), root.join(&renamed)).unwrap();
                    live[pick] = renamed;
                }
                4 if !live.is_empty() => {
                    let path = root.join(&live[pick]);
                    let bytes = std::fs::read(&path).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    std::fs::write(&path, bytes).unwrap();
                }
                5 => {
                    let doc = root.join(format!("notes{}.md", pick % 2));
                    if doc.exists() {
                        std::fs::remove_file(doc).unwrap();
                    } else {
                        std::fs::write(doc, format!("# {token} notes\n")).unwrap();
                    }
                }
                6 if !live.is_empty() => {
                    epoch_round_trip(&mut store, &mut cache, &RelPath::from(live[pick].as_str()), &label);
                    continue;
                }
                _ => continue,
            }
            scan(root, &mut store);
            if let Some(next) = cache.refreshed(&store, 0) {
                cache = next;
            }
            assert_same(&cache, &MapCache::build(&store, 0), &label);
        }
    }
}

/// What a newer extractor does to an unchanged file: rewrite its blob under the same content hash
/// and stamp the entry with the newer epoch. Both the stale and the refreshed outline must be what
/// a rebuild serves.
fn epoch_round_trip(store: &mut Store, cache: &mut MapCache, rel: &RelPath, label: &str) {
    let hash_hex = store.lookup(rel).unwrap().hash_hex.clone();
    let current = store.read_l1_by_hex(&hash_hex).unwrap().unwrap();
    let mut stale = current.clone();
    stale.extract_epoch = crate::extract::EXTRACT_EPOCH - 1;
    if let Some(symbol) = stale.symbols.first_mut() {
        symbol.name = format!("{}_before_refresh", symbol.name);
    }
    for (outline, epoch) in [(&stale, stale.extract_epoch), (&current, crate::extract::EXTRACT_EPOCH)] {
        store.overwrite_filemap_hex(&hash_hex, outline, None).unwrap();
        store.index.files.get_mut(rel).unwrap().extract_epoch = epoch;
        if let Some(next) = cache.refreshed(store, 0) {
            *cache = next;
        }
        assert_same(cache, &MapCache::build(store, 0), &format!("{label} epoch {epoch}"));
    }
}

/// A rescan that changed one file of many decodes that file's outline and no other, forgets the
/// outline of the content it replaced, and a rescan that changed nothing publishes nothing.
#[test]
fn a_refresh_after_one_edit_decodes_one_outline() {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let token = "delta_one_edit";
    for n in 0..30 {
        std::fs::write(root.join(format!("f{n}.rs")), source(token, n)).unwrap();
    }
    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    scan(root, &mut store);
    let cache = Arc::new(MapCache::build(&store, 0));
    cache.terms();
    let decoded = cache.l1_cache().misses();
    assert_eq!(decoded, 30, "the warm decodes every outline once");

    let rel = RelPath::from("f7.rs");
    let old_hash = store.lookup(&rel).unwrap().hash_hex.clone();
    std::fs::write(root.join("f7.rs"), source(token, 100)).unwrap();
    scan(root, &mut store);
    let new_hash = store.lookup(&rel).unwrap().hash_hex.clone();
    let next = cache.refreshed(&store, 0).expect("an edit changes the map");
    assert_eq!(
        names(&next.get(&rel).unwrap()),
        vec![format!("{token}_100"), format!("{token}_type_100")]
    );
    next.terms();
    assert_eq!(
        next.l1_cache().misses() - decoded,
        1,
        "only the edited file's outline is decoded"
    );
    assert!(
        !next.l1_cache().holds(&old_hash),
        "the replaced content's outline is forgotten"
    );
    assert!(next.l1_cache().holds(&new_hash));

    scan(root, &mut store);
    assert!(next.refreshed(&store, 0).is_none(), "an unchanged rescan keeps the map");
}

/// Scan only `paths`, as a watcher batch does, and return the paths the scan reports.
fn scan_scoped(root: &Path, store: &mut Store, paths: &[std::path::PathBuf]) -> crate::engine::scan::RescanPaths {
    let cfg = crate::config::ConfigV1::with_defaults();
    let mut delta = crate::engine::scan::RescanPaths::default();
    crate::scanner::scan_paths_with_observer(
        root,
        store,
        &cfg,
        paths,
        crate::scanner::EmbedMode::Inline,
        &crate::scanner::ScanCancel::new(),
        &mut delta,
        crate::retrieval_wiring::backend(),
    )
    .expect("scoped scan");
    delta
}

/// The watcher path: random edit, add, delete, rename, touch and document sequences, each scanned
/// over the paths it touched and applied with [`MapCache::with_delta`] from the scan's report. After
/// every step the patched map equals a map built from scratch over the same store.
#[test]
fn a_map_patched_from_scoped_scans_equals_a_rebuilt_one() {
    crate::store::init_isolated_cache();
    for seed in [0x6a09_e667_f3bc_c908_u64, 0xbb67_ae85_84ca_a73b] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let token = format!("delta_scoped_{:x}", seed & 0xffff);
        let mut steps = Steps(seed);
        let mut live: Vec<String> = Vec::new();
        let mut serial = 0;
        for _ in 0..6 {
            let name = format!("f{serial}.rs");
            std::fs::write(root.join(&name), source(&token, serial)).unwrap();
            live.push(name);
            serial += 1;
        }
        let mut store = Store::open(root, VIEW_WORKING).unwrap();
        scan(root, &mut store);
        let mut cache = MapCache::build(&store, 0);
        cache.terms();

        for step in 0..24 {
            let op = steps.next(6);
            let pick = steps.next(live.len().max(1));
            let label = format!("seed {seed:x} step {step} op {op}");
            let touched: Vec<String> = match op {
                0 if !live.is_empty() => {
                    serial += 1;
                    std::fs::write(root.join(&live[pick]), source(&token, serial)).unwrap();
                    vec![live[pick].clone()]
                }
                1 => {
                    serial += 1;
                    let name = format!("f{serial}.rs");
                    std::fs::write(root.join(&name), source(&token, serial)).unwrap();
                    live.push(name.clone());
                    vec![name]
                }
                2 if live.len() > 1 => {
                    let name = live.swap_remove(pick);
                    std::fs::remove_file(root.join(&name)).unwrap();
                    vec![name]
                }
                3 if !live.is_empty() => {
                    let renamed = format!("r{serial}.rs");
                    serial += 1;
                    std::fs::rename(root.join(&live[pick]), root.join(&renamed)).unwrap();
                    vec![std::mem::replace(&mut live[pick], renamed.clone()), renamed]
                }
                4 if !live.is_empty() => {
                    let path = root.join(&live[pick]);
                    let bytes = std::fs::read(&path).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    std::fs::write(&path, bytes).unwrap();
                    vec![live[pick].clone()]
                }
                5 => {
                    let doc = format!("notes{}.md", pick % 2);
                    if root.join(&doc).exists() {
                        std::fs::remove_file(root.join(&doc)).unwrap();
                    } else {
                        std::fs::write(root.join(&doc), format!("# {token} notes\n")).unwrap();
                    }
                    vec![doc]
                }
                _ => continue,
            };
            let paths: Vec<std::path::PathBuf> = touched.iter().map(|name| root.join(name)).collect();
            let delta = scan_scoped(root, &mut store, &paths);
            cache = cache.with_delta(&store, &delta.updated, &delta.removed);
            assert_same(&cache, &MapCache::build(&store, 0), &label);
        }
    }
}

fn set_mtime(path: &Path, since_epoch: std::time::Duration) {
    let file = std::fs::OpenOptions::new().write(true).open(path).expect("open");
    file.set_modified(std::time::UNIX_EPOCH + since_epoch)
        .expect("set mtime");
}

/// A watcher batch that only moved a file's stamp (a touch) leaves its content unchanged but its
/// index entry restamped; the workspace's patched map takes the new stamp a rebuild reads. Both
/// stamps are settled, years from the clock, so the scan records them as they are.
#[test]
fn a_scoped_rescan_of_a_touched_file_maps_its_new_stamp() {
    crate::store::init_isolated_cache();
    let (tmp, git_cache) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let root = tmp.path();
    let file = root.join("a.rs");
    std::fs::write(&file, source("delta_touch", 0)).unwrap();
    set_mtime(&file, std::time::Duration::from_secs(1_000_000_000));
    let ws = crate::engine::workspace::Workspace::new(
        Store::open(root, VIEW_WORKING).unwrap(),
        root.to_path_buf(),
        Arc::new(crate::config::Config::with_defaults()),
        None,
        Arc::new(crate::git_cache::GitCache::open(git_cache.path(), 1, false).unwrap()),
        None,
        crate::engine::workspace::OpenOptions {
            background: false,
            read_only: false,
            forwards_writes: false,
            lazy_cache: false,
            history_sync: None,
            #[cfg(feature = "memory")]
            memory_owner: |_| "owner".to_string(),
        },
    );
    ws.rescan(None, crate::scanner::EmbedMode::Deferred, false).unwrap();

    set_mtime(&file, std::time::Duration::from_secs(1_100_000_000));
    let report = ws
        .rescan(Some(vec![file.clone()]), crate::scanner::EmbedMode::Deferred, false)
        .unwrap();
    assert_eq!(
        (report.stats.updated, report.stats.skipped_unchanged),
        (0, 1),
        "the touch reads as unchanged content"
    );

    let rel = RelPath::from("a.rs");
    let patched = ws.cache.load();
    let meta = patched
        .file_metas()
        .find(|(path, _)| **path == rel)
        .map(|(_, meta)| meta);
    assert_eq!(meta.map(|meta| meta.mtime_ns), Some(1_100_000_000_000_000_000));
    assert_same(&patched, &MapCache::build(&ws.store_read(), 0), "touch");
}

fn doc_entry() -> crate::store::DocEntry {
    crate::store::DocEntry {
        hash_hex: "d".repeat(64),
        embedding_preset: String::new(),
        size_bytes: 1,
        mtime: 0,
        embedded: true,
        embed_attempted: true,
        config_digest: String::new(),
    }
}

/// A delta patches the document paths it names, and a document the delta does not name (a full
/// pass drops a vanished one unreported) still leaves the paths a rebuild would map.
#[test]
fn a_delta_maps_the_documents_a_rebuild_maps() {
    crate::store::init_isolated_cache();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("f0.rs"), source("delta_docs", 0)).unwrap();
    let mut store = Store::open(root, VIEW_WORKING).unwrap();
    scan(root, &mut store);
    let (kept, added, dropped) = (RelPath::from("a.md"), RelPath::from("b.md"), RelPath::from("c.md"));
    store.index.doc_files.insert(kept.clone(), doc_entry());
    store.index.doc_files.insert(dropped.clone(), doc_entry());
    let cache = MapCache::build(&store, 0);

    store.index.doc_files.insert(added.clone(), doc_entry());
    let next = cache.with_delta(&store, std::slice::from_ref(&added), &[]);
    assert_eq!(
        next.doc_paths.iter().cloned().collect::<Vec<_>>(),
        [kept.clone(), added.clone(), dropped.clone()]
    );

    store.index.doc_files.remove(&dropped);
    let after = next.with_delta(&store, &[], &[]);
    assert_eq!(after.doc_paths.iter().cloned().collect::<Vec<_>>(), [kept, added]);
    assert_eq!(after.doc_paths, MapCache::build(&store, 0).doc_paths);
}
