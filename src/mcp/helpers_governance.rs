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

/// Importance decay multiplier applied to Stale records on every audit run.
const STALE_DECAY: f32 = 0.5;
/// After a record has been continuously Stale for this many microseconds (90 days)
/// it is moved to the `memory_archive` keyspace instead of the live one.
const ARCHIVE_AFTER_MICROS: i64 = 90 * 24 * 60 * 60 * 1_000_000;
/// Default number of records to audit in a single call.
pub(super) const DEFAULT_AUDIT_LIMIT: u32 = 100;
/// Hard ceiling on the number of records to audit in a single call.
const MAX_AUDIT_LIMIT: u32 = 1000;

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
fn delete_live(idx: &crate::index::IndexDb, scope: &str, vis_byte: u8, owner: &str, key: &str) -> Result<(), McpError> {
    let raw_key = crate::index::keys::memory_by_key(scope, vis_byte, owner, key);
    idx.memory_by_key
        .remove(raw_key)
        .map_err(|e| McpError::internal_error(format!("fjall remove: {e}"), None))
}

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
    let prov = &record.provenance;
    if prov.files.is_empty() && prov.symbols.is_empty() && prov.commands.is_empty() {
        return AuditVerdict {
            state: VerifyState::Unverified,
            reasons: vec!["no provenance".to_string()],
        };
    }

    let mut reasons: Vec<String> = Vec::new();
    let mut stale = false;

    for rel in &prov.files {
        if !cache.by_path.contains_key(rel) {
            reasons.push(format!("file deleted: {}", rel.to_str_lossy()));
            stale = true;
        }
    }

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

        if let Some(stored_hash) = sym_ref.structural_hash
            && let Some(lang) = crate::lang::intern(&l1.language)
        {
            let abs_path = root.join(sym_ref.path.to_path_buf());
            if let Ok(source) = std::fs::read(&abs_path) {
                let entry = OutlineEntry {
                    map: Arc::new(l1.clone()),
                    source: Arc::new(source),
                };
                let kind_opt = sym_ref.kind.as_deref().and_then(parse_kind_opt);
                if let Some(current_hash) =
                    symbol_fingerprint(&entry, &sym_ref.name, kind_opt, lang, HashMode::Structural)
                    && current_hash != stored_hash
                {
                    reasons.push(format!("symbol body changed: {}", sym_ref.name));
                    stale = true;
                }
            }
        }
        let _ = sym;
    }

    for cmd in &prov.commands {
        let first_token = cmd.split_whitespace().next().unwrap_or(cmd.as_str());
        let cmd_rel = crate::path::RelPath::from(first_token);
        if cache.by_path.contains_key(&cmd_rel) && store.lookup(first_token).is_none() {
            reasons.push(format!("command may be stale: {cmd}"));
        }
    }

    if stale {
        return AuditVerdict {
            state: VerifyState::Stale,
            reasons,
        };
    }

    if !prov.files.is_empty() || !prov.symbols.is_empty() {
        return AuditVerdict {
            state: VerifyState::Verified,
            reasons,
        };
    }

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
        "heading" => SymbolKind::Heading,
        _ => return None,
    })
}

/// Internal return value from `evaluate_one` — carries the updated record and the
/// `archived` flag so the caller can dispatch the right Fjall write without holding
/// a mutable borrow on the `results` vector at the same time.
struct EntryOutcome {
    record: MemoryRecord,
    audit_result: AuditResult,
}

/// Read-only context shared across all `evaluate_one` calls in a single `run_memory_audit`.
/// `pub(super)` so the testable `audit_scope_persist` can take it as a parameter.
pub(super) struct AuditCtx<'a> {
    cache: &'a super::MapCache,
    store: &'a crate::store::Store,
    root: &'a std::path::Path,
    dry_run: bool,
    now: i64,
}

/// Decode, audit, and mutate one memory record. Returns `None` when the raw bytes
/// cannot be decoded (silently skip). Does NOT write to Fjall.
fn evaluate_one(ctx: &AuditCtx<'_>, key: &str, raw_val: &[u8], from_archive: bool) -> Option<EntryOutcome> {
    let mut record: MemoryRecord = rmp_serde::from_slice(raw_val).ok()?;

    let verdict = audit_one_record(ctx.cache, ctx.store, ctx.root, &record);
    let state_str = verdict.state_str().to_string();

    let mut archived = false;
    if !ctx.dry_run && !from_archive {
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
    let limit = params.limit.unwrap_or(DEFAULT_AUDIT_LIMIT).min(MAX_AUDIT_LIMIT) as usize;
    let scan_cap = limit.saturating_mul(8).max(1_000);

    let vis_byte = params.visibility.vis_byte();
    let owner: &str = match params.visibility {
        super::types_memory::Visibility::Individual => &state.agent_id,
        super::types_memory::Visibility::Group => "",
    };

    let cache = state.cache.load_full();
    let root = state.root.clone();

    let store_guard = state.store.read().await;

    let now = crate::lance::now_micros();
    let ctx = AuditCtx {
        cache: &cache,
        store: &store_guard,
        root: &root,
        dry_run: params.dry_run,
        now,
    };

    let records = gather_audit_records(state, &store_guard, &params, vis_byte, owner, limit, scan_cap).await?;

    let mut results: Vec<AuditResult> = Vec::new();
    let mut actions: Vec<PersistAction> = Vec::new();
    for (key, raw_val, from_archive, persist) in records {
        if let Some(outcome) = evaluate_one(&ctx, &key, &raw_val, from_archive) {
            if !params.dry_run && persist {
                actions.push(PersistAction {
                    vis_byte,
                    owner: owner.to_string(),
                    key,
                    record: outcome.record,
                    archive: outcome.audit_result.archived,
                });
            }
            results.push(outcome.audit_result);
        }
    }

    if !params.dry_run && !actions.is_empty() {
        persist_audit_actions(state, &store_guard, actions).await?;
    }

    let audited = results.len();
    json_result(&MemoryAuditResponse { audited, results })
}

/// A serve-computed audit verdict awaiting persistence. Deliberately NOT the comms-only
/// `AuditMutation` wire struct: the local (non-comms) persist path builds and consumes these too, so
/// the shared code must not name a `comms`-gated type. The `daemon_writer` branch maps it to
/// `AuditMutation` before forwarding.
#[cfg(feature = "memory")]
struct PersistAction {
    vis_byte: u8,
    owner: String,
    key: String,
    record: MemoryRecord,
    archive: bool,
}

/// Gather the raw `(key, value, from_archive, persist)` records to audit, forwarding the fjall scan
/// under `daemon_writer` and reading the local index otherwise. Faithful to `run_memory_audit`'s
/// original keyspace semantics: single-key always persists; the range path scans live (persist) then
/// archive (no persist), sharing one record `limit`.
#[cfg(feature = "memory")]
async fn gather_audit_records(
    state: &ServerState,
    store: &crate::store::Store,
    params: &MemoryAuditParams,
    vis_byte: u8,
    owner: &str,
    limit: usize,
    scan_cap: usize,
) -> Result<Vec<(String, Vec<u8>, bool, bool)>, McpError> {
    use super::proposals_ops::AuditScanArgs;

    let mut out: Vec<(String, Vec<u8>, bool, bool)> = Vec::new();

    if let Some(single_key) = params.key.as_deref() {
        let args = AuditScanArgs {
            vis_byte,
            owner,
            key: Some(single_key),
            from_archive: params.include_archived,
            limit: 1,
            scan_cap,
        };
        for (key, value) in scan_audit_keyspace(state, store, &args).await? {
            out.push((key, value, params.include_archived, true));
        }
        return Ok(out);
    }

    let live_args = AuditScanArgs {
        vis_byte,
        owner,
        key: None,
        from_archive: false,
        limit,
        scan_cap,
    };
    let live = scan_audit_keyspace(state, store, &live_args).await?;
    let live_count = live.len();
    for (key, value) in live {
        out.push((key, value, false, true));
    }

    if params.include_archived && live_count < limit {
        let archive_args = AuditScanArgs {
            vis_byte,
            owner,
            key: None,
            from_archive: true,
            limit: limit - live_count,
            scan_cap,
        };
        for (key, value) in scan_audit_keyspace(state, store, &archive_args).await? {
            out.push((key, value, true, false));
        }
    }
    Ok(out)
}

/// Scan one memory keyspace (live or archive) for `memory_audit`, forwarding to the daemon under
/// `daemon_writer` and reading the local index otherwise. `args.key = Some` fetches one record.
#[cfg(feature = "memory")]
async fn scan_audit_keyspace(
    state: &ServerState,
    store: &crate::store::Store,
    args: &super::proposals_ops::AuditScanArgs<'_>,
) -> Result<Vec<(String, Vec<u8>)>, McpError> {
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if state.daemon_writer {
        use super::helpers_comms::{comms_err, resolve_comms_client};
        use crate::comms::proposals_proto::{GovernanceOp, GovernanceOutcome};

        let op = GovernanceOp::AuditScan {
            vis_byte: args.vis_byte,
            owner: args.owner.to_string(),
            key: args.key.map(str::to_string),
            from_archive: args.from_archive,
            limit: args.limit as u32,
            scan_cap: args.scan_cap as u32,
        };
        let client = resolve_comms_client(state, None).await?;
        let mut guard = client.lock().await;
        let outcome = guard
            .governance_op(state.root.clone(), state.scope.clone(), op)
            .await
            .map_err(comms_err)?;
        return match outcome {
            GovernanceOutcome::AuditScanned { items } => Ok(items),
            other => Err(McpError::internal_error(
                format!("memory_audit: unexpected daemon outcome {other:?}"),
                None,
            )),
        };
    }

    let idx = store
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;
    Ok(super::proposals_ops::audit_scan_core(idx, &state.scope, args)?)
}

/// Persist serve-computed audit verdicts, forwarding under `daemon_writer` and writing the local
/// index otherwise (reusing `write_live` / `write_archive` / `delete_live`).
#[cfg(feature = "memory")]
async fn persist_audit_actions(
    state: &ServerState,
    store: &crate::store::Store,
    actions: Vec<PersistAction>,
) -> Result<(), McpError> {
    #[cfg(all(feature = "comms", any(unix, windows)))]
    if state.daemon_writer {
        use super::helpers_comms::{comms_err, resolve_comms_client};
        use crate::comms::proposals_proto::{AuditMutation, GovernanceOp, GovernanceOutcome};

        let mutations: Vec<AuditMutation> = actions
            .into_iter()
            .map(|action| AuditMutation {
                vis_byte: action.vis_byte,
                owner: action.owner,
                key: action.key,
                record: action.record,
                archive: action.archive,
            })
            .collect();
        let op = GovernanceOp::AuditPersist { mutations };
        let client = resolve_comms_client(state, None).await?;
        let mut guard = client.lock().await;
        let outcome = guard
            .governance_op(state.root.clone(), state.scope.clone(), op)
            .await
            .map_err(comms_err)?;
        return match outcome {
            GovernanceOutcome::AuditPersisted => Ok(()),
            other => Err(McpError::internal_error(
                format!("memory_audit: unexpected daemon outcome {other:?}"),
                None,
            )),
        };
    }

    let idx = store
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;
    for action in actions {
        if action.archive {
            write_archive(
                idx,
                &state.scope,
                action.vis_byte,
                &action.owner,
                &action.key,
                &action.record,
            )?;
            delete_live(idx, &state.scope, action.vis_byte, &action.owner, &action.key)?;
        } else {
            write_live(
                idx,
                &state.scope,
                action.vis_byte,
                &action.owner,
                &action.key,
                &action.record,
            )?;
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "memory"))]
mod tests;

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

    audit_scope_persist(idx, &ctx, &state.scope, DEFAULT_AUDIT_LIMIT as usize);
}

/// Sync core of [`audit_scope_on_rescan`], split out so it is testable without standing up a
/// full `ServerState`: it takes the concrete dependencies (the open index, a prepared
/// [`AuditCtx`], the scope, and a record cap) and drives the scope-bounded persist loop.
///
/// Scans up to `limit` live records under `scope`, runs each through [`evaluate_one`], and writes
/// back **only** Stale records (decay always; archive once stale > `ARCHIVE_AFTER_MICROS`). The
/// scope-prefix scan guarantees every key belongs to `scope`, so foreign-scope records are never
/// touched. Fail-open: any per-record error is warn-logged and the loop continues.
pub(super) fn audit_scope_persist(idx: &crate::index::IndexDb, ctx: &AuditCtx<'_>, scope: &str, limit: usize) {
    let scope_prefix = crate::index::keys::memory_scope_prefix(scope);
    let scan_cap = limit.saturating_mul(8).max(1_000);
    let mut evaluated = 0usize;
    for (scanned, guard) in idx.memory_by_key.prefix(&scope_prefix).enumerate() {
        if evaluated >= limit || scanned >= scan_cap {
            break;
        }
        let (raw_key_bytes, raw_val) = match guard.into_inner() {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "audit_scope_on_rescan: iter error");
                continue;
            }
        };
        let Some((_scope, vis_byte, owner, key_str)) = crate::index::keys::parse_memory_by_key(&raw_key_bytes) else {
            continue;
        };
        let Some(outcome) = evaluate_one(ctx, &key_str, &raw_val, false) else {
            continue;
        };
        evaluated += 1;
        if outcome.record.verified != VerifyState::Stale {
            continue;
        }
        let write = if outcome.audit_result.archived {
            write_archive(idx, scope, vis_byte, &owner, &key_str, &outcome.record)
                .and_then(|()| delete_live(idx, scope, vis_byte, &owner, &key_str))
        } else {
            write_live(idx, scope, vis_byte, &owner, &key_str, &outcome.record)
        };
        if let Err(e) = write {
            tracing::warn!(key = key_str, error = ?e, "audit_scope_on_rescan: persist failed");
        }
    }
}
