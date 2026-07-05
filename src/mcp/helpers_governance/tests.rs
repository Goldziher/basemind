use std::sync::Arc;

use super::super::MapCache;
use super::super::OutlineEntry;
use super::super::helpers::{HashMode, symbol_fingerprint};
use super::super::types_memory::{MemoryRecord, Provenance, SymbolRef, VerifyState, Visibility};
use super::{
    ARCHIVE_AFTER_MICROS, AuditCtx, EntryOutcome, STALE_DECAY, audit_one_record, audit_scope_persist, evaluate_one,
    write_live,
};
use crate::index::keys::memory_by_key;
use crate::path::RelPath;

// ── Fixture helpers ──────────────────────────────────────────────────────

/// Scan a single Rust file and return `(TempDir, Store, MapCache)`.
fn scanned_fixture(rel_path: &str, src: &[u8]) -> (tempfile::TempDir, crate::store::Store, MapCache) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::write(root.join(rel_path), src).expect("write fixture");
    let _ = crate::lang::ensure_grammars().expect("grammars");
    let cfg = crate::config::default_for_root(root);
    let mut store = crate::store::Store::open(root, crate::store::VIEW_WORKING).expect("open store");
    crate::scanner::scan(
        root,
        &mut store,
        &cfg,
        crate::scanner::ScanSource::WorkingTree,
        crate::scanner::EmbedMode::Inline,
    )
    .expect("scan");
    let cache = MapCache::build(&store);
    (dir, store, cache)
}

/// `MemoryRecord` with empty provenance; importance `1.0`.
fn bare_record() -> MemoryRecord {
    MemoryRecord {
        value: "test".to_string(),
        tags: vec![],
        created_at: 1,
        updated_at: 1,
        provenance: Provenance::default(),
        verified: VerifyState::Unverified,
        last_verified: 0,
        importance: 1.0,
    }
}

fn encode(record: &MemoryRecord) -> Vec<u8> {
    rmp_serde::to_vec_named(record).expect("encode")
}

// ── T1: audit_one_record branch coverage ─────────────────────────────────

#[test]
fn should_return_unverified_when_provenance_is_empty() {
    let (_dir, store, cache) = scanned_fixture("a.rs", b"pub fn alpha() {}\n");
    let record = bare_record();
    let verdict = audit_one_record(&cache, &store, _dir.path(), &record);
    assert_eq!(verdict.state, VerifyState::Unverified);
    assert!(
        verdict.reasons.iter().any(|r| r.contains("no provenance")),
        "expected 'no provenance' reason, got: {:?}",
        verdict.reasons
    );
}

#[test]
fn should_return_stale_when_referenced_file_deleted() {
    let (_dir, store, cache) = scanned_fixture("a.rs", b"pub fn alpha() {}\n");
    let record = MemoryRecord {
        provenance: Provenance {
            files: vec![RelPath::from("gone.rs")],
            ..Default::default()
        },
        ..bare_record()
    };
    let verdict = audit_one_record(&cache, &store, _dir.path(), &record);
    assert_eq!(verdict.state, VerifyState::Stale);
    assert!(!verdict.reasons.is_empty(), "Stale verdict must carry a reason");
}

#[test]
fn should_return_verified_when_referenced_file_present() {
    let (_dir, store, cache) = scanned_fixture("a.rs", b"pub fn alpha() {}\n");
    let record = MemoryRecord {
        provenance: Provenance {
            files: vec![RelPath::from("a.rs")],
            ..Default::default()
        },
        ..bare_record()
    };
    let verdict = audit_one_record(&cache, &store, _dir.path(), &record);
    assert_eq!(verdict.state, VerifyState::Verified);
}

#[test]
fn should_return_stale_when_symbol_file_gone() {
    let (_dir, store, cache) = scanned_fixture("a.rs", b"pub fn alpha() {}\n");
    let record = MemoryRecord {
        provenance: Provenance {
            symbols: vec![SymbolRef {
                path: RelPath::from("gone.rs"),
                name: "alpha".to_string(),
                kind: None,
                structural_hash: None,
            }],
            ..Default::default()
        },
        ..bare_record()
    };
    let verdict = audit_one_record(&cache, &store, _dir.path(), &record);
    assert_eq!(verdict.state, VerifyState::Stale);
    assert!(!verdict.reasons.is_empty());
}

#[test]
fn should_return_stale_when_symbol_name_absent() {
    let (_dir, store, cache) = scanned_fixture("a.rs", b"pub fn alpha() {}\n");
    let record = MemoryRecord {
        provenance: Provenance {
            symbols: vec![SymbolRef {
                path: RelPath::from("a.rs"),
                name: "missing_fn".to_string(),
                kind: None,
                structural_hash: None,
            }],
            ..Default::default()
        },
        ..bare_record()
    };
    let verdict = audit_one_record(&cache, &store, _dir.path(), &record);
    assert_eq!(verdict.state, VerifyState::Stale);
    assert!(!verdict.reasons.is_empty());
}

fn structural_hash_of_alpha(_root: &std::path::Path, src: &[u8]) -> [u8; 32] {
    let lang = crate::lang::intern("rust").expect("rust lang");
    let _ = crate::lang::ensure_grammars().expect("grammars");
    let l1 = crate::extract::l1::extract_l1(lang, src).expect("extract l1");
    let entry = OutlineEntry {
        map: Arc::new(l1),
        source: Arc::new(src.to_vec()),
    };
    let hash_vec = symbol_fingerprint(&entry, "alpha", None, lang, HashMode::Structural).expect("fingerprint");
    hash_vec.try_into().expect("[u8; 32] from hash vec")
}

#[test]
fn should_return_verified_when_symbol_hash_matches() {
    let src = b"pub fn alpha() { let x = 1; x }\n";
    let (_dir, store, cache) = scanned_fixture("a.rs", src);
    let hash = structural_hash_of_alpha(_dir.path(), src);
    let record = MemoryRecord {
        provenance: Provenance {
            symbols: vec![SymbolRef {
                path: RelPath::from("a.rs"),
                name: "alpha".to_string(),
                kind: None,
                structural_hash: Some(hash),
            }],
            ..Default::default()
        },
        ..bare_record()
    };
    let verdict = audit_one_record(&cache, &store, _dir.path(), &record);
    assert_eq!(
        verdict.state,
        VerifyState::Verified,
        "unchanged body must produce Verified; reasons: {:?}",
        verdict.reasons
    );
}

#[test]
fn should_return_stale_when_symbol_body_changed() {
    let original_src = b"pub fn alpha() { let x = 1; x }\n";
    let edited_src = b"pub fn alpha() { let x = 99; x }\n";

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let _ = crate::lang::ensure_grammars().expect("grammars");
    let original_hash = structural_hash_of_alpha(root, original_src);

    // Scan the semantically edited body.
    std::fs::write(root.join("a.rs"), edited_src).expect("write edited");
    let cfg = crate::config::default_for_root(root);
    let mut store = crate::store::Store::open(root, crate::store::VIEW_WORKING).expect("open");
    crate::scanner::scan(
        root,
        &mut store,
        &cfg,
        crate::scanner::ScanSource::WorkingTree,
        crate::scanner::EmbedMode::Inline,
    )
    .expect("scan");
    let cache = MapCache::build(&store);
    // Stored hash was for original body; disk now has edited body.
    let record = MemoryRecord {
        provenance: Provenance {
            symbols: vec![SymbolRef {
                path: RelPath::from("a.rs"),
                name: "alpha".to_string(),
                kind: None,
                structural_hash: Some(original_hash),
            }],
            ..Default::default()
        },
        ..bare_record()
    };
    let verdict = audit_one_record(&cache, &store, root, &record);
    assert_eq!(
        verdict.state,
        VerifyState::Stale,
        "semantic body change must produce Stale"
    );
    assert!(
        verdict.reasons.iter().any(|r| r.contains("symbol body changed")),
        "expected 'symbol body changed' reason, got: {:?}",
        verdict.reasons
    );
}

#[test]
fn should_remain_verified_after_formatting_only_edit() {
    let original_src = b"pub fn alpha() { let x = 1; x }\n";
    // Extra spaces + comment — structurally identical to the original.
    let formatted_src = b"pub fn alpha() {\n    // formatting\n    let x = 1;\n    x\n}\n";

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let _ = crate::lang::ensure_grammars().expect("grammars");
    let original_hash = structural_hash_of_alpha(root, original_src);

    std::fs::write(root.join("a.rs"), formatted_src).expect("write formatted");
    let cfg = crate::config::default_for_root(root);
    let mut store = crate::store::Store::open(root, crate::store::VIEW_WORKING).expect("open");
    crate::scanner::scan(
        root,
        &mut store,
        &cfg,
        crate::scanner::ScanSource::WorkingTree,
        crate::scanner::EmbedMode::Inline,
    )
    .expect("scan");
    let cache = MapCache::build(&store);

    let record = MemoryRecord {
        provenance: Provenance {
            symbols: vec![SymbolRef {
                path: RelPath::from("a.rs"),
                name: "alpha".to_string(),
                kind: None,
                structural_hash: Some(original_hash),
            }],
            ..Default::default()
        },
        ..bare_record()
    };
    let verdict = audit_one_record(&cache, &store, root, &record);
    assert_eq!(
        verdict.state,
        VerifyState::Verified,
        "formatting-only change must not flag Stale; reasons: {:?}",
        verdict.reasons
    );
}

#[test]
fn should_not_set_stale_for_command_with_missing_path() {
    // `cargo` is not a tracked path → commands-only provenance → Unverified, not Stale.
    let (_dir, store, cache) = scanned_fixture("a.rs", b"pub fn alpha() {}\n");
    let record = MemoryRecord {
        provenance: Provenance {
            commands: vec!["cargo test".to_string()],
            ..Default::default()
        },
        ..bare_record()
    };
    let verdict = audit_one_record(&cache, &store, _dir.path(), &record);
    // Only commands → Unverified (not Stale).
    assert_eq!(
        verdict.state,
        VerifyState::Unverified,
        "commands-only provenance must yield Unverified, not Stale"
    );
}

// ── T2: evaluate_one branch coverage ─────────────────────────────────────

fn stale_record_via_deleted_file(importance: f32, last_verified: i64) -> MemoryRecord {
    MemoryRecord {
        provenance: Provenance {
            files: vec![RelPath::from("deleted.rs")],
            ..Default::default()
        },
        importance,
        last_verified,
        ..bare_record()
    }
}

#[test]
fn should_archive_stale_record_exceeding_archive_threshold() {
    let (_dir, store, cache) = scanned_fixture("a.rs", b"pub fn alpha() {}\n");
    let now: i64 = 1_000_000_000_000_000; // a large fixed µs timestamp
    // last_verified is far enough in the past that (now - last_verified) > ARCHIVE_AFTER_MICROS
    let stale_since = now - ARCHIVE_AFTER_MICROS - 1;
    let record = stale_record_via_deleted_file(1.0, stale_since);
    let raw = encode(&record);

    let ctx = AuditCtx {
        cache: &cache,
        store: &store,
        root: _dir.path(),
        dry_run: false,
        now,
    };
    let EntryOutcome {
        record: out,
        audit_result,
    } = evaluate_one(&ctx, "k", &raw, false).expect("evaluate_one returned None");

    assert!(audit_result.archived, "record stale > 90 days must be archived");
    assert_eq!(
        out.importance,
        1.0 * STALE_DECAY,
        "importance must be halved by STALE_DECAY"
    );
}

#[test]
fn should_decay_importance_but_not_archive_recently_stale_record() {
    let (_dir, store, cache) = scanned_fixture("a.rs", b"pub fn alpha() {}\n");
    let now: i64 = 1_000_000_000_000_000;
    // last_verified is 1 µs ago — well within the archive threshold.
    let record = stale_record_via_deleted_file(1.0, now - 1);
    let raw = encode(&record);

    let ctx = AuditCtx {
        cache: &cache,
        store: &store,
        root: _dir.path(),
        dry_run: false,
        now,
    };
    let EntryOutcome {
        record: out,
        audit_result,
    } = evaluate_one(&ctx, "k", &raw, false).expect("evaluate_one returned None");

    assert!(!audit_result.archived, "recently stale record must not be archived");
    assert_eq!(out.importance, 1.0 * STALE_DECAY, "importance must still be halved");
}

#[test]
fn should_not_mutate_record_when_dry_run_is_true() {
    let (_dir, store, cache) = scanned_fixture("a.rs", b"pub fn alpha() {}\n");
    let now: i64 = 1_000_000_000_000_000;
    let record = stale_record_via_deleted_file(1.0, 0);
    let raw = encode(&record);

    let ctx = AuditCtx {
        cache: &cache,
        store: &store,
        root: _dir.path(),
        dry_run: true,
        now,
    };
    let EntryOutcome {
        record: out,
        audit_result,
    } = evaluate_one(&ctx, "k", &raw, false).expect("evaluate_one returned None");

    // dry_run → importance unchanged, verified/last_verified not updated, not archived.
    assert_eq!(out.importance, 1.0, "dry_run must not decay importance");
    assert_eq!(
        out.verified,
        VerifyState::Unverified,
        "dry_run must not update verified"
    );
    assert_eq!(out.last_verified, 0, "dry_run must not update last_verified");
    assert!(!audit_result.archived, "dry_run must not archive");
    // But the state string in audit_result still reflects the computed verdict.
    assert_eq!(audit_result.state, "stale");
}

#[test]
fn should_not_mutate_record_when_from_archive_is_true() {
    let (_dir, store, cache) = scanned_fixture("a.rs", b"pub fn alpha() {}\n");
    let now: i64 = 1_000_000_000_000_000;
    let record = stale_record_via_deleted_file(1.0, 0);
    let raw = encode(&record);

    let ctx = AuditCtx {
        cache: &cache,
        store: &store,
        root: _dir.path(),
        dry_run: false,
        now,
    };
    let EntryOutcome {
        record: out,
        audit_result,
    } = evaluate_one(&ctx, "k", &raw, true).expect("evaluate_one returned None");

    // from_archive → no mutations.
    assert_eq!(out.importance, 1.0, "from_archive must not decay importance");
    assert!(!audit_result.archived, "from_archive path must never set archived");
    assert_eq!(audit_result.state, "stale");
}

// ── T3: audit_scope_persist — the background rescan pass, end-to-end ──────────
//
// These seed crafted `MemoryRecord`s straight into Fjall (timestamps + provenance MCP cannot
// inject), run the real `audit_scope_persist` loop, then read the PERSISTED record back from the
// index — so they prove the background pass mutated durable state, not just that a fresh audit
// recomputes a verdict. `now` is a large fixed timestamp so the 90-day arithmetic stays positive.

const NOW: i64 = 1_000_000_000_000_000_000;

/// Build an `AuditCtx` over a scanned fixture (live, non-dry-run) pinned to `NOW`.
fn persist_ctx<'a>(cache: &'a MapCache, store: &'a crate::store::Store, root: &'a std::path::Path) -> AuditCtx<'a> {
    AuditCtx {
        cache,
        store,
        root,
        dry_run: false,
        now: NOW,
    }
}

/// Decode the live record at `(scope, vis, owner, key)`, or `None` if absent.
fn read_live(idx: &crate::index::IndexDb, scope: &str, vis: u8, owner: &str, key: &str) -> Option<MemoryRecord> {
    let raw = idx
        .memory_by_key
        .get(memory_by_key(scope, vis, owner, key))
        .expect("fjall get live")?;
    Some(rmp_serde::from_slice(&raw).expect("decode live"))
}

#[test]
fn should_persist_stale_and_decay_via_scope_pass() {
    let (dir, store, cache) = scanned_fixture("a.rs", b"pub fn alpha() {}\n");
    let idx = store.index_db.as_ref().expect("index_db present");
    let scope = "scope-a";
    let vis = Visibility::Group.vis_byte();
    // Recently-verified, stale-making record (references a file absent from the scan).
    let mut record = stale_record_via_deleted_file(1.0, NOW - 1);
    record.verified = VerifyState::Unverified;
    write_live(idx, scope, vis, "", "k1", &record).expect("seed live record");

    let ctx = persist_ctx(&cache, &store, dir.path());
    audit_scope_persist(idx, &ctx, scope, 100);

    let persisted = read_live(idx, scope, vis, "", "k1").expect("record still live (decayed)");
    assert_eq!(
        persisted.verified,
        VerifyState::Stale,
        "background pass must persist the Stale verdict"
    );
    assert_eq!(
        persisted.importance,
        1.0 * STALE_DECAY,
        "background pass must decay importance"
    );
}

#[test]
fn should_archive_via_scope_pass_when_stale_over_90_days() {
    let (dir, store, cache) = scanned_fixture("a.rs", b"pub fn alpha() {}\n");
    let idx = store.index_db.as_ref().expect("index_db present");
    let scope = "scope-a";
    let vis = Visibility::Group.vis_byte();
    // Continuously stale for > 90 days → must be archived out of the live keyspace.
    let mut record = stale_record_via_deleted_file(1.0, NOW - ARCHIVE_AFTER_MICROS - 1);
    record.verified = VerifyState::Unverified;
    write_live(idx, scope, vis, "", "k2", &record).expect("seed live record");

    let ctx = persist_ctx(&cache, &store, dir.path());
    audit_scope_persist(idx, &ctx, scope, 100);

    assert!(
        read_live(idx, scope, vis, "", "k2").is_none(),
        "a >90-day-stale record must be removed from the live keyspace"
    );
    let raw = idx
        .memory_archive
        .get(memory_by_key(scope, vis, "", "k2"))
        .expect("fjall get archive")
        .expect("record must be moved to the archive keyspace");
    let archived: MemoryRecord = rmp_serde::from_slice(&raw).expect("decode archived");
    assert_eq!(archived.verified, VerifyState::Stale);
    assert_eq!(archived.importance, 1.0 * STALE_DECAY);
}

#[test]
fn should_not_touch_foreign_scope_record() {
    let (dir, store, cache) = scanned_fixture("a.rs", b"pub fn alpha() {}\n");
    let idx = store.index_db.as_ref().expect("index_db present");
    let vis = Visibility::Group.vis_byte();
    // Two identical stale-making records: one in the audited scope, one in a foreign scope.
    let mut local = stale_record_via_deleted_file(1.0, NOW - 1);
    local.verified = VerifyState::Unverified;
    let foreign = local.clone();
    write_live(idx, "scope-a", vis, "", "kl", &local).expect("seed local record");
    write_live(idx, "scope-other", vis, "", "kf", &foreign).expect("seed foreign record");

    // Run the pass for scope-a only; the scope-prefix scan must never reach scope-other.
    let ctx = persist_ctx(&cache, &store, dir.path());
    audit_scope_persist(idx, &ctx, "scope-a", 100);

    // The local record proves the pass actually ran (flipped Stale + decayed)...
    let local_after = read_live(idx, "scope-a", vis, "", "kl").expect("local record must be live");
    assert_eq!(
        local_after.verified,
        VerifyState::Stale,
        "the audited scope's record must be flipped Stale, proving the pass ran"
    );
    assert_eq!(local_after.importance, 1.0 * STALE_DECAY);

    // ...while the foreign-scope record is left completely untouched.
    let untouched = read_live(idx, "scope-other", vis, "", "kf").expect("foreign record must remain live");
    assert_eq!(
        untouched.verified,
        VerifyState::Unverified,
        "foreign-scope record must not be flipped Stale by another scope's pass"
    );
    assert_eq!(
        untouched.importance, 1.0,
        "foreign-scope importance must be left untouched"
    );
}
