//! W11 proposal helpers: co-change association-rule mining + proposal lifecycle.
//!
//! All functions are `#[cfg(feature = "memory")]`-gated because they read/write the `proposals`
//! Fjall keyspace (which exists in every build, but the logic is only compiled with the gate).
//!
//! ## Mining algorithm
//!
//! Walk `window` recent commits. For each commit:
//! - Skip if the file-set size exceeds `MAX_FILES_PER_COMMIT` (bulk/vendor commits).
//! - Count `freq[file]` (how often each file changes) and `cochange[(a,b)]` for every
//!   unordered sorted pair of files that appear in the same commit.
//!
//! After the walk, for every pair `(a,b)` with `cochange >= min_support` and
//! `confidence = cochange / freq[a] >= min_confidence`:
//! - Cluster transitively (a file + all its high-confidence co-change partners).
//! - Derive a content-addressed `id = hex(blake3(sorted_file_set))`.
//! - Skip if a tombstone exists for this id (rejected by a previous `proposal_reject`).
//! - Write/overwrite the proposal in the `proposals` Fjall keyspace.
//!
//! ## Accept / reject
//!
//! `proposal_accept`: reads the proposal, promotes it to a `MemoryRecord` with
//! `tags = ["skill","cochange"]`, stamps `verified` via `audit_one_record`, writes to Fjall
//! AND embeds into LanceDB (via `embed_memory_row`), deletes the proposal.
//!
//! `proposal_reject`: deletes the proposal and writes a tombstone under
//! `PROPOSAL_KIND_TOMBSTONE` so re-mining won't resurface it.

#[cfg(feature = "memory")]
use ahash::{AHashMap, AHashSet};
#[cfg(feature = "memory")]
use rmcp::ErrorData as McpError;
#[cfg(feature = "memory")]
use rmcp::model::CallToolResult;

#[cfg(feature = "memory")]
use super::ServerState;
#[cfg(feature = "memory")]
use super::helpers::json_result;
#[cfg(feature = "memory")]
use super::types_governance::{
    ProposalAcceptParams, ProposalAcceptResponse, ProposalEntry, ProposalRecord, ProposalRejectParams,
    ProposalRejectResponse, ProposalsListParams, ProposalsListResponse, ProposalsMineParams, ProposalsMineResponse,
};
#[cfg(feature = "memory")]
use super::types_memory::{MemoryRecord, Provenance, VerifyState};
#[cfg(feature = "memory")]
use crate::index::keys::{PROPOSAL_KIND_SKILL, PROPOSAL_KIND_TOMBSTONE};
#[cfg(feature = "memory")]
use crate::path::RelPath;

// ─── Named constants ──────────────────────────────────────────────────────────

/// Default number of recent commits to walk when mining.
#[cfg(feature = "memory")]
const DEFAULT_MINE_WINDOW: u32 = 200;
/// Hard ceiling on the mining window (prevents accidentally scanning 10k+ commits).
#[cfg(feature = "memory")]
const MAX_MINE_WINDOW: u32 = 2000;
/// Default minimum co-change count (support) for a candidate to be emitted.
#[cfg(feature = "memory")]
const DEFAULT_MIN_SUPPORT: u32 = 5;
/// Default minimum confidence (support / anchor_freq).
#[cfg(feature = "memory")]
const DEFAULT_MIN_CONFIDENCE: f32 = 0.6;
/// Default maximum file count per commit before the commit is skipped (bulk/vendor guard).
#[cfg(feature = "memory")]
const DEFAULT_MAX_FILES_PER_COMMIT: u32 = 25;
/// Hard maximum for `max_files_per_commit`.
#[cfg(feature = "memory")]
const HARD_MAX_FILES_PER_COMMIT: u32 = 200;
/// Default and max for `proposals_list` pagination.
#[cfg(feature = "memory")]
const DEFAULT_LIST_LIMIT: u32 = 100;
#[cfg(feature = "memory")]
const MAX_LIST_LIMIT: u32 = 1000;
/// Prefix of the short id embedded in auto-derived memory keys.
#[cfg(feature = "memory")]
const MEMORY_KEY_PREFIX: &str = "skill/cochange-";
/// Number of hex chars to use from the full blake3 id in the auto-derived memory key.
#[cfg(feature = "memory")]
const SHORT_ID_LEN: usize = 12;
/// Tags applied to memory records promoted from co-change proposals.
#[cfg(feature = "memory")]
const COCHANGE_TAGS: &[&str] = &["skill", "cochange"];

// ─── Content-addressed proposal id ───────────────────────────────────────────

/// Compute the proposal id as the hex-encoded blake3 hash of the sorted file-set bytes.
/// The sort is byte-order (RelPath implements Ord lexicographically on raw bytes) so the id is
/// deterministic regardless of which file is the "anchor" in the pair loop.
#[cfg(feature = "memory")]
fn proposal_id(sorted_files: &[RelPath]) -> String {
    let mut hasher = blake3::Hasher::new();
    for f in sorted_files {
        hasher.update(f.as_bytes());
        hasher.update(b"\x00"); // NUL separator — safe because RelPath never contains NUL
    }
    hex::encode(hasher.finalize().as_bytes())
}

// ─── Cluster helpers ──────────────────────────────────────────────────────────

/// Transitively cluster a file with all partners that exceed both `min_support` and
/// `min_confidence` (using `file` as the anchor). Returns the sorted file-set.
///
/// Transitivity is bounded: we only consider direct partners of the anchor (depth-1 BFS),
/// which keeps the cluster small and avoids the O(n²) explosion of full transitive closure on
/// large co-change graphs. The anchor is always the file with the highest `freq[file]` in the
/// pair, which biases toward "when you change X, also check Y" rather than the reverse.
///
/// Works on interned file indices (`files[i]`) so the co-change map is keyed by cheap
/// `(usize, usize)` pairs — no `RelPath` heap clones in the hot loop. `RelPath`s are only
/// materialized into the returned sorted file-set.
#[cfg(feature = "memory")]
fn build_cluster(
    anchor: usize,
    files: &[RelPath],
    cochange: &AHashMap<(usize, usize), u32>,
    freq: &[u32],
    min_support: u32,
    min_confidence: f32,
) -> Vec<RelPath> {
    let anchor_freq = freq.get(anchor).copied().unwrap_or(1).max(1);
    let mut cluster: AHashSet<usize> = AHashSet::new();
    cluster.insert(anchor);

    for (&(a, b), &count) in cochange {
        let partner = if a == anchor {
            b
        } else if b == anchor {
            a
        } else {
            continue;
        };
        if count < min_support {
            continue;
        }
        let confidence = count as f32 / anchor_freq as f32;
        if confidence >= min_confidence {
            cluster.insert(partner);
        }
    }

    let mut sorted: Vec<RelPath> = cluster.into_iter().map(|i| files[i].clone()).collect();
    sorted.sort();
    sorted
}

/// Build a human-readable description from a co-change cluster.
#[cfg(feature = "memory")]
fn build_description(anchor: &RelPath, cluster: &[RelPath], support: u32, anchor_freq: u32) -> String {
    let partners: Vec<String> = cluster
        .iter()
        .filter(|f| *f != anchor)
        .map(|f| f.to_str_lossy().into_owned())
        .collect();

    if partners.is_empty() {
        return format!(
            "File {} changed frequently ({} commits).",
            anchor.to_str_lossy(),
            anchor_freq,
        );
    }

    format!(
        "When editing {}, also check {} — co-changed in {} of {} recent commits.",
        anchor.to_str_lossy(),
        partners.join(", "),
        support,
        anchor_freq,
    )
}

// ─── `proposals_mine` ─────────────────────────────────────────────────────────

/// Mine co-change skill proposals from the recent git history.
///
/// See module-level docs for the algorithm. Requires git (returns an MCP error when not in a
/// git repo). Safe to call repeatedly — the content-addressed id means re-mining the same
/// candidate overwrites rather than duplicates.
#[cfg(feature = "memory")]
pub(super) async fn run_proposals_mine(
    state: &ServerState,
    params: ProposalsMineParams,
) -> Result<CallToolResult, McpError> {
    use super::helpers::{head_sha, require_git_repo};

    let window = params.window.unwrap_or(DEFAULT_MINE_WINDOW).min(MAX_MINE_WINDOW);
    let min_support = params.min_support.unwrap_or(DEFAULT_MIN_SUPPORT);
    let min_confidence = params.min_confidence.unwrap_or(DEFAULT_MIN_CONFIDENCE).clamp(0.0, 1.0);
    let max_files_per_commit = params
        .max_files_per_commit
        .unwrap_or(DEFAULT_MAX_FILES_PER_COMMIT)
        .min(HARD_MAX_FILES_PER_COMMIT);

    // ── Git log ──────────────────────────────────────────────────────────────
    let repo = require_git_repo(state)?;
    let head = head_sha(repo)?;
    let commits = state
        .git_cache
        .log(repo, &head, None, window, true)
        .map_err(|e| McpError::internal_error(format!("git log: {e}"), None))?;

    // ── Co-change counting ────────────────────────────────────────────────────
    //
    // Each distinct `RelPath` is interned once into `files` (the owned-path arena); `freq` and the
    // `cochange` pair map are then keyed by the cheap `usize` index instead of cloning the heap
    // `bstr::BString` per pair. At defaults (window=200, max=25 files) the old code cloned a
    // `RelPath` for every one of the ~62.5k pair updates; interning drops that to one clone per
    // distinct path at first sight.
    //
    // `files[id]`: the interned path. `freq[id]`: how many commits touched it.
    // `cochange[(a,b)]`: how many commits touched both, with the smaller index first so
    // `(a,b) == (b,a)` collapses to one canonical key.
    let mut interner: AHashMap<RelPath, usize> = AHashMap::new();
    let mut files: Vec<RelPath> = Vec::new();
    let mut freq: Vec<u32> = Vec::new();
    let mut cochange: AHashMap<(usize, usize), u32> = AHashMap::new();
    let mut skipped_bulk: u32 = 0;

    // Reused per-commit scratch buffer of interned indices — avoids reallocating each iteration.
    let mut commit_ids: Vec<usize> = Vec::new();

    for commit in commits.as_ref() {
        // Changed/Added/Modified files for this commit (no Deleted).
        let is_changed = |kind: &crate::git::ChangeKind| !matches!(kind, crate::git::ChangeKind::Deleted);

        // Bulk/vendor guard: count changed files before interning so a skipped commit costs nothing.
        let changed_count = commit.files.iter().filter(|(_, kind)| is_changed(kind)).count();
        if changed_count > max_files_per_commit as usize {
            skipped_bulk += 1;
            continue;
        }

        // Intern each changed path and record per-file frequency. One clone per distinct path
        // (only when first seen); repeat sightings reuse the existing index.
        commit_ids.clear();
        for (path, _) in commit.files.iter().filter(|(_, kind)| is_changed(kind)) {
            let id = match interner.get(path) {
                Some(&id) => id,
                None => {
                    let id = files.len();
                    files.push(path.clone());
                    freq.push(0);
                    interner.insert(path.clone(), id);
                    id
                }
            };
            freq[id] += 1;
            commit_ids.push(id);
        }

        // Update pair co-change counts (O(files²) per commit — bounded by max_files_per_commit).
        for i in 0..commit_ids.len() {
            for j in (i + 1)..commit_ids.len() {
                // Canonical key: smaller index first so (a,b) == (b,a).
                let (a, b) = if commit_ids[i] <= commit_ids[j] {
                    (commit_ids[i], commit_ids[j])
                } else {
                    (commit_ids[j], commit_ids[i])
                };
                *cochange.entry((a, b)).or_insert(0) += 1;
            }
        }
    }

    // ── Candidate extraction ──────────────────────────────────────────────────
    //
    // Collect anchors that have at least one qualifying partner. An anchor is the higher-freq
    // file in each qualifying pair — biases toward "when you change the hot file, check the
    // co-changed file" rather than the reverse. We deduplicate by cluster id (content-addressed
    // blake3 of the sorted file-set) to avoid emitting the same cluster from both ends.

    // Identify anchors (interned indices) with at least one partner exceeding thresholds.
    let mut anchor_candidates: AHashSet<usize> = AHashSet::new();
    for (&(a, b), &count) in &cochange {
        if count < min_support {
            continue;
        }
        let fa = freq.get(a).copied().unwrap_or(1).max(1);
        let fb = freq.get(b).copied().unwrap_or(1).max(1);
        // Use the higher-frequency file as anchor.
        if fa >= fb {
            let conf = count as f32 / fa as f32;
            if conf >= min_confidence {
                anchor_candidates.insert(a);
            }
        } else {
            let conf = count as f32 / fb as f32;
            if conf >= min_confidence {
                anchor_candidates.insert(b);
            }
        }
    }

    // ── Write proposals ───────────────────────────────────────────────────────
    let store_guard = state.store.read().await;
    let idx = store_guard
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("proposals index not available", None))?;

    let now = crate::lance::now_micros();
    let mut seen_ids: AHashSet<String> = AHashSet::new();
    let mut mined: usize = 0;

    for &anchor in &anchor_candidates {
        let anchor_path = &files[anchor];
        let cluster = build_cluster(anchor, &files, &cochange, &freq, min_support, min_confidence);
        if cluster.len() < 2 {
            // Degenerate cluster (no qualifying partner survived) — skip.
            continue;
        }

        let id = proposal_id(&cluster);
        if !seen_ids.insert(id.clone()) {
            // Already wrote this cluster from a different anchor in this run.
            continue;
        }

        // Skip if a tombstone exists for this id (user previously rejected it).
        let tombstone_key = crate::index::keys::proposal_by_id(&state.scope, PROPOSAL_KIND_TOMBSTONE, &id);
        let has_tombstone = idx
            .proposals
            .get(&tombstone_key)
            .map_err(|e| McpError::internal_error(format!("proposals get: {e}"), None))?
            .is_some();
        if has_tombstone {
            continue;
        }

        // Find the co-change count for the anchor ↔ each partner pair (use the max support
        // across pairs in the cluster as the representative support for the description).
        // Partners are mapped back to their interned index via the interner to key `cochange`.
        let anchor_freq = freq.get(anchor).copied().unwrap_or(1).max(1);
        let max_support = cluster
            .iter()
            .filter(|f| *f != anchor_path)
            .map(|partner| {
                let Some(&p) = interner.get(partner) else {
                    return 0;
                };
                let pair = if anchor <= p { (anchor, p) } else { (p, anchor) };
                *cochange.get(&pair).unwrap_or(&0)
            })
            .max()
            .unwrap_or(0);

        let confidence = max_support as f32 / anchor_freq as f32;
        // Importance: support / window (normalised to [0,1)).
        let importance = (max_support as f32 / window as f32).min(0.99);
        let description = build_description(anchor_path, &cluster, max_support, anchor_freq);

        let record = ProposalRecord {
            kind: PROPOSAL_KIND_SKILL,
            files: cluster,
            support: max_support,
            window,
            confidence,
            description,
            importance,
            created_at: now,
        };

        let raw_key = crate::index::keys::proposal_by_id(&state.scope, PROPOSAL_KIND_SKILL, &id);
        let bytes = rmp_serde::to_vec_named(&record)
            .map_err(|e| McpError::internal_error(format!("serialize proposal: {e}"), None))?;
        idx.proposals
            .insert(raw_key, bytes)
            .map_err(|e| McpError::internal_error(format!("proposals insert: {e}"), None))?;
        mined += 1;
    }

    json_result(&ProposalsMineResponse {
        mined,
        window_inspected: window,
        skipped_bulk,
    })
}

// ─── `proposals_list` ─────────────────────────────────────────────────────────

/// List pending proposals for the current scope, optionally filtered by kind.
/// Paginated via Fjall-backed cursors (stable across rescans).
#[cfg(feature = "memory")]
pub(super) async fn run_proposals_list(
    state: &ServerState,
    params: ProposalsListParams,
) -> Result<CallToolResult, McpError> {
    use std::ops::Bound;

    use super::cursor::prefix_upper_bound;

    let limit = params.limit.unwrap_or(DEFAULT_LIST_LIMIT).min(MAX_LIST_LIMIT) as usize;
    let scan_cap = limit.saturating_mul(8).max(1_000);

    // Determine which kind byte(s) to scan. When `kind` is omitted we scan both
    // PROPOSAL_KIND_MEMORY and PROPOSAL_KIND_SKILL (skip tombstones).
    let kind_bytes: Vec<u8> = match params.kind.as_deref() {
        Some("skill") => vec![PROPOSAL_KIND_SKILL],
        Some("memory") => vec![crate::index::keys::PROPOSAL_KIND_MEMORY],
        None | Some(_) => vec![crate::index::keys::PROPOSAL_KIND_MEMORY, PROPOSAL_KIND_SKILL],
    };

    let store_guard = state.store.read().await;
    let idx = store_guard
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("proposals index not available", None))?;

    let mut proposals: Vec<ProposalEntry> = Vec::new();
    let mut truncated = false;
    let mut last_key_bytes: Option<Vec<u8>> = None;

    // Resume from cursor when provided: decode the raw Fjall key and start after it.
    let resume_key: Option<Vec<u8>> = if let Some(c) = &params.cursor {
        Some(c.decode_fjall()?)
    } else {
        None
    };

    'outer: for kind_byte in kind_bytes {
        let prefix = crate::index::keys::proposal_ns_prefix(&state.scope, kind_byte);
        let upper = prefix_upper_bound(&prefix);

        let lower_bound: Bound<Vec<u8>> = if let Some(ref key) = resume_key {
            // Only skip past the resume key if it is within THIS kind's prefix.
            if key.starts_with(&prefix) {
                Bound::Excluded(key.clone())
            } else {
                Bound::Included(prefix.clone())
            }
        } else {
            Bound::Included(prefix.clone())
        };

        let upper_bound: Bound<Vec<u8>> = match upper {
            Some(u) => Bound::Excluded(u),
            None => Bound::Unbounded,
        };

        let iter = idx.proposals.range::<Vec<u8>, _>((lower_bound, upper_bound));

        for (scanned, guard) in iter.enumerate() {
            if scanned >= scan_cap {
                truncated = true;
                break 'outer;
            }
            if proposals.len() >= limit {
                truncated = true;
                break 'outer;
            }
            let (raw_key, raw_val) = guard
                .into_inner()
                .map_err(|e| McpError::internal_error(format!("proposals iter: {e}"), None))?;
            let Some((_, _, id)) = crate::index::keys::parse_proposal_by_id(&raw_key) else {
                continue;
            };
            let Ok(record) = rmp_serde::from_slice::<ProposalRecord>(&raw_val) else {
                continue;
            };
            last_key_bytes = Some(raw_key.to_vec());
            proposals.push(ProposalEntry {
                id,
                kind: record.kind,
                files: record.files,
                support: record.support,
                window: record.window,
                confidence: record.confidence,
                description: record.description,
                importance: record.importance,
                created_at: record.created_at,
            });
        }
    }

    let total = proposals.len();
    let next_cursor = if truncated {
        last_key_bytes.map(|k| super::cursor::Cursor::encode_fjall(&k))
    } else {
        None
    };

    json_result(&ProposalsListResponse {
        total,
        truncated,
        proposals,
        next_cursor,
    })
}

// ─── `proposal_accept` ────────────────────────────────────────────────────────

/// Accept a proposal: promote it to a `MemoryRecord` (Fjall + LanceDB), then delete the
/// proposal from the `proposals` keyspace.
///
/// The memory record is stamped `Verified` by calling `audit_one_record` on the file
/// provenance (if all referenced files exist in the current index). This is the
/// code-grounded-staleness proof: a later `memory_audit` will flip it to `Stale` the moment
/// one of the referenced files disappears.
#[cfg(feature = "memory")]
pub(super) async fn run_proposal_accept(
    state: &ServerState,
    params: ProposalAcceptParams,
) -> Result<CallToolResult, McpError> {
    // ── Read proposal ─────────────────────────────────────────────────────────
    let raw_key = crate::index::keys::proposal_by_id(&state.scope, PROPOSAL_KIND_SKILL, &params.id);

    let proposal: ProposalRecord = {
        let store_guard = state.store.read().await;
        let idx = store_guard
            .index_db
            .as_ref()
            .ok_or_else(|| McpError::internal_error("proposals index not available", None))?;
        let raw = idx
            .proposals
            .get(&raw_key)
            .map_err(|e| McpError::internal_error(format!("proposals get: {e}"), None))?
            .ok_or_else(|| McpError::invalid_params(format!("proposal not found: {}", params.id), None))?;
        rmp_serde::from_slice::<ProposalRecord>(&raw)
            .map_err(|e| McpError::internal_error(format!("decode proposal: {e}"), None))?
    };

    // ── Derive memory key ─────────────────────────────────────────────────────
    let memory_key = params.key.clone().unwrap_or_else(|| {
        let short = &params.id[..params.id.len().min(SHORT_ID_LEN)];
        format!("{MEMORY_KEY_PREFIX}{short}")
    });

    // ── Build MemoryRecord ────────────────────────────────────────────────────
    let now = crate::lance::now_micros();
    let tags: Vec<String> = COCHANGE_TAGS.iter().map(|s| s.to_string()).collect();
    let provenance = Provenance {
        files: proposal.files.clone(),
        symbols: Vec::new(),
        commands: Vec::new(),
    };

    // Build the record once (Unverified), then stamp the verdict from a single audit pass —
    // avoids two struct literals that could silently drift out of sync.
    let mut record = MemoryRecord {
        value: proposal.description.clone(),
        tags: tags.clone(),
        created_at: now,
        updated_at: now,
        provenance,
        verified: VerifyState::Unverified,
        last_verified: 0,
        importance: proposal.importance,
    };

    let cache = state.cache.load_full();
    let root = state.root.clone();
    let store_guard = state.store.read().await;
    let verdict = super::helpers_governance::audit_one_record(&cache, &store_guard, &root, &record);
    record.verified = verdict.state;
    record.last_verified = now;

    // ── Write to Fjall (group tier, owner = "") ───────────────────────────────
    let idx = store_guard
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("memory_by_key index not available", None))?;

    super::helpers_governance::write_live(
        idx,
        &state.scope,
        crate::index::keys::MEMORY_VIS_GROUP,
        "",
        &memory_key,
        &record,
    )?;

    // Delete the proposal from the proposals keyspace.
    idx.proposals
        .remove(&raw_key)
        .map_err(|e| McpError::internal_error(format!("proposals remove: {e}"), None))?;

    // Drop the store guard before the async Lance embed.
    drop(store_guard);

    // ── Embed into LanceDB ────────────────────────────────────────────────────
    {
        let embedding = super::memory::embed_query(state, &proposal.description).await?;
        let lance = super::memory::lance_store(state).await?;
        let row = crate::lance::MemoryRow {
            scope: state.scope.clone(),
            key: memory_key.clone(),
            value: proposal.description.clone(),
            tags,
            visibility: "group".to_string(),
            agent_id: String::new(),
            embedding,
            created_at: now,
            updated_at: now,
        };
        let lance_clone = std::sync::Arc::clone(&lance);
        tokio::task::spawn_blocking(move || lance_clone.upsert_memory(row))
            .await
            .map_err(|e| McpError::internal_error(format!("spawn_blocking: {e}"), None))?
            .map_err(|e| McpError::internal_error(format!("upsert_memory: {e}"), None))?;
    }

    json_result(&ProposalAcceptResponse {
        accepted: true,
        memory_key,
    })
}

// ─── `proposal_reject` ────────────────────────────────────────────────────────

/// Reject a proposal: delete it from the `proposals` keyspace and write a tombstone so
/// re-mining will not resurface the same candidate.
#[cfg(feature = "memory")]
pub(super) async fn run_proposal_reject(
    state: &ServerState,
    params: ProposalRejectParams,
) -> Result<CallToolResult, McpError> {
    if let Some(ref reason) = params.reason {
        tracing::info!(id = %params.id, reason = %reason, "proposal rejected");
    }

    let proposal_key = crate::index::keys::proposal_by_id(&state.scope, PROPOSAL_KIND_SKILL, &params.id);
    let tombstone_key = crate::index::keys::proposal_by_id(&state.scope, PROPOSAL_KIND_TOMBSTONE, &params.id);

    let store_guard = state.store.read().await;
    let idx = store_guard
        .index_db
        .as_ref()
        .ok_or_else(|| McpError::internal_error("proposals index not available", None))?;

    // Delete proposal (if it still exists — idempotent).
    idx.proposals
        .remove(&proposal_key)
        .map_err(|e| McpError::internal_error(format!("proposals remove: {e}"), None))?;

    // Write tombstone (empty value — marker only).
    idx.proposals
        .insert(tombstone_key, b"")
        .map_err(|e| McpError::internal_error(format!("tombstone insert: {e}"), None))?;

    json_result(&ProposalRejectResponse { rejected: true })
}
