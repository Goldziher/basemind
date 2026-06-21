//! Governance helpers for the `memory_audit` MCP tool (W10).
//!
//! `audit_one_record` is a pure in-RAM, sync function — no async calls. It:
//! - checks file provenance against `MapCache.by_path` (file deleted → Stale)
//! - checks symbol provenance against the in-RAM L1 map (symbol missing → Stale)
//! - checks structural hashes by reading the working-tree source bytes from disk and
//!   computing `symbol_fingerprint(…, HashMode::Structural)` (body changed → Stale)
//! - treats commands as advisory (command path missing ⇒ warn reason, never Stale)
//!
//! `run_memory_audit` is the async MCP entrypoint: it reads the Fjall index, drives
//! `audit_one_record`, persists mutations unless `dry_run=true`, and optionally
//! auto-archives records that have been Stale for more than 90 days.
//!
//! `audit_scope_on_rescan` is the lightweight background maintenance pass injected by
//! `scan_and_refresh` after every rescan. It is fail-open: any error is warn-logged and
//! the rest of the audit continues.

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::OutlineEntry;
use super::ServerState;
use super::helpers::{HashMode, json_result, symbol_fingerprint};
use super::types_governance::{AuditResult, AuditVerdict, MemoryAuditParams, MemoryAuditResponse};
use super::types_memory::{MemoryRecord, VerifyState};

// ─── Named constants ──────────────────────────────────────────────────────────

/// Importance decay multiplier applied to Stale records on every audit run.
const STALE_DECAY: f32 = 0.5;
/// After a record has been continuously Stale for this many microseconds (90 days)
/// it is moved to the `memory_archive` keyspace instead of the live one.
const ARCHIVE_AFTER_MICROS: i64 = 90 * 24 * 60 * 60 * 1_000_000;
/// Default number of records to audit in a single call.
pub(super) const DEFAULT_AUDIT_LIMIT: u32 = 100;
/// Hard ceiling on the number of records to audit in a single call.
const MAX_AUDIT_LIMIT: u32 = 1000;

// ─── Helpers from memory.rs (re-exported here to avoid duplication) ───────────

/// Write a `MemoryRecord` into the **live** `memory_by_key` keyspace.
pub(super) fn write_live(
    idx: &crate::index::IndexDb,
    scope: &str,
    vis_byte: u8,
    owner: &str,
    key: &str,
    record: &MemoryRecord,
) -> Result<(), McpError> {
    let raw_key = crate::index::keys::memory_by_key(scope, vis_byte, owner, key);
    let bytes = rmp_serde::to_vec_named(record)
        .map_err(|e| McpError::internal_error(format!("serialize memory record: {e}"), None))?;
    idx.memory_by_key
        .insert(raw_key, bytes)
        .map_err(|e| McpError::internal_error(format!("fjall insert: {e}"), None))
}

/// Write a `MemoryRecord` into the **archive** `memory_archive` keyspace.
fn write_archive(
    idx: &crate::index::IndexDb,
    scope: &str,
    vis_byte: u8,
    owner: &str,
    key: &str,
    record: &MemoryRecord,
) -> Result<(), McpError> {
    let raw_key = crate::index::keys::memory_by_key(scope, vis_byte, owner, key);
    let bytes = rmp_serde::to_vec_named(record)
        .map_err(|e| McpError::internal_error(format!("serialize archive record: {e}"), None))?;
    idx.memory_archive
        .insert(raw_key, bytes)
        .map_err(|e| McpError::internal_error(format!("fjall archive insert: {e}"), None))
}

/// Remove a `MemoryRecord` from the **live** `memory_by_key` keyspace.
fn delete_live(
    idx: &crate::index::IndexDb,
    scope: &str,
    vis_byte: u8,
    owner: &str,
    key: &str,
) -> Result<(), McpError> {
    let raw_key = crate::index::keys::memory_by_key(scope, vis_byte, owner, key);
    idx.memory_by_key
        .remove(raw_key)
        .map_err(|e| McpError::internal_error(format!("fjall remove: {e}"), None))
}

// ─── Core audit logic ─────────────────────────────────────────────────────────

/// Compute the audit verdict for one `MemoryRecord`.
///
/// Entirely in-RAM (plus optional disk reads for structural hashes).  No async — this
/// function is called from both the async `run_memory_audit` and the sync background
/// maintenance pass.
///
/// Parameters:
/// - `cache`  — the current `MapCache` snapshot (pre-loaded by the caller, lock-free).
/// - `store`  — needed to read source bytes for structural-hash comparison.
/// - `root`   — absolute repo root for resolving relative paths to disk files.
/// - `record` — the record being evaluated.
pub(super) fn audit_one_record(
    cache: &super::MapCache,
    store: &crate::store::Store,
    root: &std::path::Path,
    record: &MemoryRecord,
) -> AuditVerdict {
    // Empty provenance → nothing to verify; mark Unverified rather than Stale.
    let prov = &record.provenance;
    if prov.files.is_empty() && prov.symbols.is_empty() && prov.commands.is_empty() {
        return AuditVerdict {
            state: VerifyState::Unverified,
            reasons: vec!["no provenance".to_string()],
        };
    }

    let mut reasons: Vec<String> = Vec::new();
    let mut stale = false;

    // ── File provenance ────────────────────────────────────────────────────────
    for rel in &prov.files {
        if !cache.by_path.contains_key(rel) {
            reasons.push(format!("file deleted: {}", rel.to_str_lossy()));
            stale = true;
        }
    }

    // ── Symbol provenance ──────────────────────────────────────────────────────
    for sym_ref in &prov.symbols {
        let Some(l1) = cache.by_path.get(&sym_ref.path) else {
            reasons.push(format!(
                "symbol not found: {} (file gone: {})",
                sym_ref.name,
                sym_ref.path.to_str_lossy()
            ));
            stale = true;
            continue;
        };

        let sym = l1.symbols.iter().find(|s| {
            s.name == sym_ref.name
                && sym_ref
                    .kind
                    .as_deref()
                    .is_none_or(|k| crate::mcp::helpers::kind_to_str(s.kind) == k)
        });

        let Some(sym) = sym else {
            reasons.push(format!("symbol not found: {}", sym_ref.name));
            stale = true;
            continue;
        };

        // Structural hash comparison — only when the stored ref carries a hash.
        //
        // PERF (deferred — cold in v1): this branch only runs for *symbol* provenance, which
        // nothing populates yet (W11 cochange skills carry file refs). Before a symbol-capture
        // path ships, address: the `l1.clone()` below (Arc-wrap `FileMap` in the cache instead),
        // the redundant find here vs inside `symbol_fingerprint`, reading bytes from the
        // content-addressed blob store rather than disk, and dropping the store guard across the
        // parse in `audit_scope_on_rescan`.
        if let Some(stored_hash) = sym_ref.structural_hash {
            // Intern the language name to a `'static` `LangId`; skip hash check when
            // the language is unknown (tolerate gracefully, do not flip to Stale).
            if let Some(lang) = crate::lang::intern(&l1.language) {
                let abs_path = root.join(sym_ref.path.to_path_buf());
                if let Ok(source) = std::fs::read(&abs_path) {
                    // Build a minimal OutlineEntry wrapping the in-RAM L1 and the source.
                    let entry = OutlineEntry {
                        map: Arc::new(l1.clone()),
                        source: Arc::new(source),
                    };
                    let kind_opt = sym_ref.kind.as_deref().and_then(parse_kind_opt);
                    if let Some(current_hash) = symbol_fingerprint(
                        &entry,
                        &sym_ref.name,
                        kind_opt,
                        lang,
                        HashMode::Structural,
                    ) && current_hash != stored_hash
                    {
                        reasons.push(format!("symbol body changed: {}", sym_ref.name));
                        stale = true;
                    }
                    // Cannot compute hash (no tree-sitter support / parse failure) →
                    // tolerate silently; do not flip to Stale.
                }
                // Cannot read from disk (race with deletion / permissions) →
                // do not flip to Stale; the file-existence check above covers deletion.
            }
        }
        let _ = sym; // suppress unused-variable warning
    }

    // ── Command provenance (advisory only) ────────────────────────────────────
    for cmd in &prov.commands {
        let first_token = cmd.split_whitespace().next().unwrap_or(cmd.as_str());
        let cmd_rel = crate::path::RelPath::from(first_token);
        if cache.by_path.contains_key(&cmd_rel) {
            // First token looks like a tracked path — check it in the store.
            if store.lookup(first_token).is_none() {
                reasons.push(format!("command may be stale: {cmd}"));
                // Do NOT set stale = true — commands are advisory.
            }
        }
    }

    // ── Final verdict ──────────────────────────────────────────────────────────
    if stale {
        return AuditVerdict {
            state: VerifyState::Stale,
            reasons,
        };
    }

    // At least one code reference was present and all resolved cleanly → Verified.
    if !prov.files.is_empty() || !prov.symbols.is_empty() {
        return AuditVerdict {
            state: VerifyState::Verified,
            reasons,
        };
    }

    // Only commands (advisory) → Unverified.
    AuditVerdict {
        state: VerifyState::Unverified,
        reasons,
    }
}

/// Parse a kind string back to `SymbolKind` for the optional kind filter in `symbol_fingerprint`.
fn parse_kind_opt(k: &str) -> Option<crate::extract::SymbolKind> {
    use crate::extract::SymbolKind;
    Some(match k {
        "function" => SymbolKind::Function,
        "method" => SymbolKind::Method,
        "struct" => SymbolKind::Struct,
        "enum" => SymbolKind::Enum,
        "class" => SymbolKind::Class,
        "interface" => SymbolKind::Interface,
        "trait" => SymbolKind::Trait,
        "type" => SymbolKind::Type,
        "const" => SymbolKind::Const,
        "module" => SymbolKind::Module,
        "macro" => SymbolKind::Macro,
        "impl" => SymbolKind::Impl,
        "namespace" => SymbolKind::Namespace,
        "getter" => SymbolKind::Getter,
        "setter" => SymbolKind::Setter,
        "field" => SymbolKind::Field,
        "variable" => SymbolKind::Variable,
        "enum_variant" => SymbolKind::EnumVariant,
        "constructor" => SymbolKind::Constructor,
        "decorator" => SymbolKind::Decorator,
        _ => return None,
    })
}

// ─── Async entrypoint ─────────────────────────────────────────────────────────

/// Internal return value from `evaluate_one` — carries the updated record and the
/// `archived` flag so the caller can dispatch the right Fjall write without holding
/// a mutable borrow on the `results` vector at the same time.
struct EntryOutcome {
    record: MemoryRecord,
    audit_result: AuditResult,
}

/// Read-only context shared across all `evaluate_one` calls in a single `run_memory_audit`.
struct AuditCtx<'a> {
    cache: &'a super::MapCache,
    store: &'a crate::store::Store,
    root: &'a std::path::Path,
    dry_run: bool,
    now: i64,
}

/// Decode, audit, and mutate one memory record. Returns `None` when the raw bytes
/// cannot be decoded (silently skip). Does NOT write to Fjall.
fn evaluate_one(
    ctx: &AuditCtx<'_>,
    key: &str,
    raw_val: &[u8],
    from_archive: bool,
) -> Option<EntryOutcome> {
    let mut record: MemoryRecord = rmp_serde::from_slice(raw_val).ok()?;

    let verdict = audit_one_record(ctx.cache, ctx.store, ctx.root, &record);
    let state_str = verdict.state_str().to_string();

    let mut archived = false;
    if !ctx.dry_run && !from_archive {
        // Capture old `last_verified` BEFORE updating so the archive threshold
        // measures how long the record has been stale.
        let prev_last_verified = record.last_verified;
        record.verified = verdict.state;
        record.last_verified = ctx.now;

        if verdict.state == VerifyState::Stale {
            record.importance *= STALE_DECAY;
            let stale_since = if prev_last_verified > 0 {
                prev_last_verified
            } else {
                record.updated_at
            };
            if ctx.now.saturating_sub(stale_since) > ARCHIVE_AFTER_MICROS {
                archived = true;
            }
        }
    }

    Some(EntryOutcome {
        record,
        audit_result: AuditResult {
            key: key.to_string(),
            state: state_str,
            reasons: verdict.reasons,
            archived,
        },
    })
}

/// Audit memory records for the given scope / visibility tier.
///
/// For each record: calls `audit_one_record`, applies mutations unless `dry_run`, and
/// optionally archives records that have been Stale for more than 90 days.
pub(super) async fn run_memory_audit(
    state: &ServerState,
    params: MemoryAuditParams,
) -> Result<CallToolResult, McpError> {
    let limit = params
        .limit
        .unwrap_or(DEFAULT_AUDIT_LIMIT)
        .min(MAX_AUDIT_LIMIT) as usize;
    let scan_cap = limit.saturating_mul(8).max(1_000);

    // Resolve namespace coordinates — mirrors `memory::namespace` exactly.
    let vis_byte = params.visibility.vis_byte();
    let owner: &str = match params.visibility {
        super::types_memory::Visibility::Individual => &state.agent_id,
        super::types_memory::Visibility::Group => "",
    };

    // Load a stable cache snapshot.
    let cache = state.cache.load_full();
    let root = state.root.clone();

    // Acquire store read guard once for the Fjall queries.
    let store_guard = state.store.read().await;
    let idx = store_guard
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;

    let now = crate::lance::now_micros();
    let ctx = AuditCtx {
        cache: &cache,
        store: &store_guard,
        root: &root,
        dry_run: params.dry_run,
        now,
    };
    let mut results: Vec<AuditResult> = Vec::new();

    if let Some(ref single_key) = params.key {
        // Single-key mode.
        let raw_key = crate::index::keys::memory_by_key(&state.scope, vis_byte, owner, single_key);
        let keyspace = if params.include_archived {
            &idx.memory_archive
        } else {
            &idx.memory_by_key
        };
        let raw_val_opt = keyspace
            .get(&raw_key)
            .map_err(|e| McpError::internal_error(format!("fjall get: {e}"), None))?;
        if let Some(raw_val) = raw_val_opt
            && let Some(outcome) = evaluate_one(&ctx, single_key, &raw_val, params.include_archived)
        {
            if !ctx.dry_run {
                if outcome.audit_result.archived {
                    write_archive(
                        idx,
                        &state.scope,
                        vis_byte,
                        owner,
                        single_key,
                        &outcome.record,
                    )?;
                    delete_live(idx, &state.scope, vis_byte, owner, single_key)?;
                } else {
                    write_live(
                        idx,
                        &state.scope,
                        vis_byte,
                        owner,
                        single_key,
                        &outcome.record,
                    )?;
                }
            }
            results.push(outcome.audit_result);
        }
    } else {
        // Range-scan mode — walk the live namespace.
        let ns_prefix = crate::index::keys::memory_by_key_ns_prefix(&state.scope, vis_byte, owner);

        for (scanned, guard) in idx.memory_by_key.prefix(&ns_prefix).enumerate() {
            if results.len() >= limit || scanned >= scan_cap {
                break;
            }
            let (raw_key_bytes, raw_val) = guard
                .into_inner()
                .map_err(|e| McpError::internal_error(format!("index iter: {e}"), None))?;
            let Some(key) = crate::index::keys::parse_memory_key_only(&raw_key_bytes) else {
                continue;
            };
            let key_str = key.to_string();
            if let Some(outcome) = evaluate_one(&ctx, &key_str, &raw_val, false) {
                if !ctx.dry_run {
                    if outcome.audit_result.archived {
                        write_archive(
                            idx,
                            &state.scope,
                            vis_byte,
                            owner,
                            &key_str,
                            &outcome.record,
                        )?;
                        delete_live(idx, &state.scope, vis_byte, owner, &key_str)?;
                    } else {
                        write_live(
                            idx,
                            &state.scope,
                            vis_byte,
                            owner,
                            &key_str,
                            &outcome.record,
                        )?;
                    }
                }
                results.push(outcome.audit_result);
            }
        }

        // Optionally also scan the archive keyspace (read-only; no mutations on archived rows).
        if params.include_archived {
            for (arch_scanned, guard) in idx.memory_archive.prefix(&ns_prefix).enumerate() {
                if results.len() >= limit || arch_scanned >= scan_cap {
                    break;
                }
                let (raw_key_bytes, raw_val) = guard
                    .into_inner()
                    .map_err(|e| McpError::internal_error(format!("archive iter: {e}"), None))?;
                let Some(key) = crate::index::keys::parse_memory_key_only(&raw_key_bytes) else {
                    continue;
                };
                let key_str = key.to_string();
                if let Some(outcome) = evaluate_one(&ctx, &key_str, &raw_val, true) {
                    results.push(outcome.audit_result);
                }
            }
        }
    }

    let audited = results.len();
    json_result(&MemoryAuditResponse { audited, results })
}

// ─── Background maintenance ───────────────────────────────────────────────────

/// Lightweight background audit pass injected by `scan_and_refresh` after every rescan.
///
/// Scans up to `DEFAULT_AUDIT_LIMIT` live memory records **for this server's scope only** (across
/// both visibility tiers), decays Stale records' importance, and auto-archives records that have
/// been Stale longer than `ARCHIVE_AFTER_MICROS`. Only Stale records are written back — Verified
/// rows are left untouched to avoid write amplification on every rescan. Reuses `evaluate_one` so
/// the decay/archive logic stays single-sourced with the on-demand `memory_audit` tool.
///
/// Fail-open: any error is warn-logged and the pass continues on the next record; never panics.
/// The scope filter is a correctness guard — a memory from another repo would resolve its
/// provenance against *this* repo's code map and be falsely flagged Stale.
pub(super) async fn audit_scope_on_rescan(state: &Arc<ServerState>) {
    let cache = state.cache.load_full();
    let root = state.root.clone();

    let store_guard = state.store.read().await;
    let idx = match store_guard.index_db.as_ref() {
        Some(idx) => idx,
        None => return,
    };

    let ctx = AuditCtx {
        cache: &cache,
        store: &store_guard,
        root: &root,
        dry_run: false,
        now: crate::lance::now_micros(),
    };

    // Scope-bounded prefix scan — never iterates another repo's keys (the prefix encodes this
    // repo's scope across every visibility tier / owner), so the per-key scope check is needless.
    let scope_prefix = crate::index::keys::memory_scope_prefix(&state.scope);
    for (count, guard) in idx.memory_by_key.prefix(&scope_prefix).enumerate() {
        if count >= DEFAULT_AUDIT_LIMIT as usize {
            break;
        }
        let (raw_key_bytes, raw_val) = match guard.into_inner() {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "audit_scope_on_rescan: iter error");
                continue;
            }
        };
        let Some((_scope, vis_byte, owner, key_str)) =
            crate::index::keys::parse_memory_by_key(&raw_key_bytes)
        else {
            continue;
        };
        let Some(outcome) = evaluate_one(&ctx, &key_str, &raw_val, false) else {
            continue;
        };
        // Persist only Stale records (decay always; archive once stale > 90 days). Verified /
        // Unverified rows are left as-is so a rescan doesn't rewrite the whole store.
        if outcome.record.verified != VerifyState::Stale {
            continue;
        }
        // `state.scope` is safe here — the prefix scan guarantees every key is in this scope.
        let write = if outcome.audit_result.archived {
            write_archive(
                idx,
                &state.scope,
                vis_byte,
                &owner,
                &key_str,
                &outcome.record,
            )
            .and_then(|()| delete_live(idx, &state.scope, vis_byte, &owner, &key_str))
        } else {
            write_live(
                idx,
                &state.scope,
                vis_byte,
                &owner,
                &key_str,
                &outcome.record,
            )
        };
        if let Err(e) = write {
            tracing::warn!(key = key_str, error = ?e, "audit_scope_on_rescan: persist failed");
        }
    }
}
