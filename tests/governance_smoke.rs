//! Integration smoke tests for W11 git-mined skill-proposal boundaries.
//!
//! All tests drive behavior through a `basemind serve` MCP subprocess so
//! the association-rule thresholds, tombstone logic, and pagination are exercised
//! end-to-end over the real MCP wire — not against mocked internals.
//!
//! ## Coverage
//!
//! 1. **confidence boundary** — verify the ≈0.454-confidence pair is rejected at 0.5 and
//!    accepted at 0.4, proving the confidence gate fires at the exact boundary.
//! 2. **support gating** — verify a pair with support=5 is rejected at min_support=6
//!    and accepted at min_support=5.
//! 3. **skipped_bulk** — verify commits exceeding `max_files_per_commit` are counted
//!    in `skipped_bulk` and their pairs are NOT counted toward co-change.
//! 4. **deterministic proposal_id** — mining the same fixture twice yields the same `id`.
//! 5. **proposals_list pagination + kind filter** — two disjoint clusters; limit=1
//!    returns truncated=true; following `next_cursor` yields the second; kind="memory"
//!    returns zero results.
//! 6. **idempotent reject** — `proposal_reject` twice is a no-op; tombstone suppresses
//!    re-mining the same candidate.
//!
//! ## Visibility note
//!
//! These tests use only the public MCP surface (`proposals_mine`, `proposals_list`,
//! `proposal_accept`, `proposal_reject`). All Fjall internals are `pub(crate)` or
//! `pub(super)` and are not reachable from an external test crate.

#![cfg(feature = "memory")]

use std::path::Path;
use std::process::Command;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::Command as AsyncCommand;

// ─── Shared helpers ───────────────────────────────────────────────────────────

/// Run a `git` command in `repo`, propagating identity env vars so CI works.
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .status()
        .expect("git in PATH");
    assert!(status.success(), "git {args:?} failed in {}", repo.display());
}

/// Build a minimal git repo that co-changes two files across two commits so
/// `proposals_mine(min_support=1)` deterministically yields a co-change cluster.
///
/// Layout:
/// - commit 1 ("init"): `core.rs` + `helper.rs` + `extra.rs`  — all three staged together.
/// - commit 2 ("update core"): only `core.rs` modified.
///
/// With `min_support=1` / `min_confidence=0.1` the (`core.rs`, `helper.rs`) pair
/// co-changed in commit 1, which is enough for at least one proposal.
fn build_governance_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    std::fs::write(root.join("core.rs"), b"pub fn process() { helper(); }\n").unwrap();
    std::fs::write(root.join("helper.rs"), b"pub fn helper() {}\n").unwrap();
    std::fs::write(root.join("extra.rs"), b"pub fn extra() {}\n").unwrap();

    git(root, &["add", "core.rs", "helper.rs", "extra.rs"]);
    git(root, &["commit", "-qm", "init"]);

    // Touch core.rs in a second commit so the repo has some co-change history.
    std::fs::write(root.join("core.rs"), b"pub fn process() { helper(); let _ = 1; }\n").unwrap();
    git(root, &["commit", "-aqm", "update core"]);

    dir
}

/// Scan the repo into a working-tree index (same pattern as `mcp_smoke.rs::run_scan`).
fn run_scan(root: &Path) {
    let cfg = basemind::config::default_for_root(root);
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");
    // Run the scan on a dedicated std thread, OFF this `#[tokio::test]` runtime: the scanner's
    // deferred vector flush opens `LanceStore`, which `block_on`s its own current-thread runtime and
    // panics if driven from a tokio worker thread once the ONNX model is cached. Mirrors the
    // production context (CLI thread / `spawn_blocking`) — the pattern `mcp_smoke::run_scan` uses.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut store = basemind::store::Store::open(root, basemind::store::VIEW_WORKING).expect("open store");
            basemind::scanner::scan(root, &mut store, &cfg, basemind::scanner::ScanSource::WorkingTree).expect("scan");
        });
    });
}

/// Decode the first JSON text payload from an MCP `CallToolResult`.
fn decode_text(result: &CallToolResult) -> Value {
    use rmcp::model::ContentBlock;
    let raw = result
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or(Value::Null)
}

/// Build a `CallToolRequestParams` from a tool name and a JSON args object.
fn call_params(name: &'static str, args: Value) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(name);
    if let Some(obj) = args.as_object() {
        params = params.with_arguments(obj.clone());
    }
    params
}

// ─── Confidence-boundary fixture ──────────────────────────────────────────────
//
// Commits that build up exactly: freq[a]=10, freq[b]=5, cochange(a,b)=5.
//   anchor = a (higher freq), confidence = 5/10 = 0.5
//
// Layout (files created in separate commits so the init does NOT inflate cochange):
//   1. init-a:    only a.rs (Added)
//   2. init-b:    only b.rs (Added)
//   3..7. both-N: a.rs + b.rs together  → cochange += 1 each (5 commits)
//   8..12. only-N: a.rs only            → cochange unchanged (5 commits)
//
// After walking all commits:
//   freq[a] = 1(init-a) + 5(both) + 5(only) = 11 — wait, init-a also counted.
//   Actually: freq[a] = 1+5+5 = 11, freq[b] = 1+5 = 6, cochange = 5.
//   anchor = a (freq 11 >= 6), confidence = 5/11 ≈ 0.454.
//
// We therefore set boundary thresholds at 0.5 (reject) and 0.4 (accept),
// and support boundary at 6 (reject) and 5 (accept).
//
// This fixture serves both the confidence-boundary test and the support-gating test.
fn build_confidence_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    // Create a.rs alone (does not contribute to cochange with b.rs).
    std::fs::write(root.join("a.rs"), b"pub fn a() {}\n").unwrap();
    git(root, &["add", "a.rs"]);
    git(root, &["commit", "-qm", "init-a"]);

    // Create b.rs alone (does not contribute to cochange with a.rs).
    std::fs::write(root.join("b.rs"), b"pub fn b() {}\n").unwrap();
    git(root, &["add", "b.rs"]);
    git(root, &["commit", "-qm", "init-b"]);

    // 5 commits touching BOTH a.rs and b.rs together → cochange(a,b) = 5.
    for i in 0..5u32 {
        std::fs::write(root.join("a.rs"), format!("pub fn a() {{ /* both {i} */ }}\n")).unwrap();
        std::fs::write(root.join("b.rs"), format!("pub fn b() {{ /* both {i} */ }}\n")).unwrap();
        git(root, &["add", "a.rs", "b.rs"]);
        git(root, &["commit", "-qm", &format!("both {i}")]);
    }

    // 5 commits touching ONLY a.rs → freq[a] += 5, cochange unchanged.
    for i in 0..5u32 {
        std::fs::write(root.join("a.rs"), format!("pub fn a() {{ /* only-a {i} */ }}\n")).unwrap();
        git(root, &["add", "a.rs"]);
        git(root, &["commit", "-qm", &format!("only-a {i}")]);
    }

    // Final counts (window covers all commits, all within max_files_per_commit=10):
    //   freq[a] = 1(init-a) + 5(both) + 5(only) = 11
    //   freq[b] = 1(init-b) + 5(both) = 6
    //   cochange(a,b) = 5
    //   anchor = a (freq 11 >= 6), confidence = 5/11 ≈ 0.454

    dir
}

// ─── Bulk-skip fixture ────────────────────────────────────────────────────────
//
// One bulk commit with 6 files (p.rs, q.rs, r.rs, s.rs, t.rs, u.rs)
// plus several "normal" commits touching only p.rs + q.rs together.
// With max_files_per_commit=3, the bulk commit is skipped and p.rs+q.rs
// co-change only from the small commits.
fn build_bulk_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    // Create all 6 files.
    for f in &["p.rs", "q.rs", "r.rs", "s.rs", "t.rs", "u.rs"] {
        std::fs::write(root.join(f), format!("pub fn {f}() {{}}\n")).unwrap();
    }
    // Bulk commit: all 6 files.
    git(root, &["add", "p.rs", "q.rs", "r.rs", "s.rs", "t.rs", "u.rs"]);
    git(root, &["commit", "-qm", "bulk init"]);

    // 3 small commits touching ONLY p.rs + q.rs.
    for i in 0..3u32 {
        std::fs::write(root.join("p.rs"), format!("pub fn p() {{ /* {i} */ }}\n")).unwrap();
        std::fs::write(root.join("q.rs"), format!("pub fn q() {{ /* {i} */ }}\n")).unwrap();
        git(root, &["add", "p.rs", "q.rs"]);
        git(root, &["commit", "-qm", &format!("pq {i}")]);
    }

    dir
}

// ─── Two-cluster fixture (pagination test) ───────────────────────────────────
//
// Two disjoint co-change clusters:
//   cluster 1: a.rs + b.rs  (co-change N times, support >= min_support)
//   cluster 2: x.rs + y.rs  (co-change N times, support >= min_support)
// Mining with min_support=3 should yield >= 2 proposals.
fn build_two_cluster_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    git(root, &["init", "-q"]);
    git(root, &["config", "commit.gpgsign", "false"]);

    // Create all files.
    for f in &["a.rs", "b.rs", "x.rs", "y.rs"] {
        std::fs::write(root.join(f), "pub fn f() {}\n").unwrap();
    }
    git(root, &["add", "a.rs", "b.rs", "x.rs", "y.rs"]);
    git(root, &["commit", "-qm", "init"]);

    // 5 commits co-changing a.rs + b.rs.
    for i in 0..5u32 {
        std::fs::write(root.join("a.rs"), format!("pub fn a{i}() {{}}\n")).unwrap();
        std::fs::write(root.join("b.rs"), format!("pub fn b{i}() {{}}\n")).unwrap();
        git(root, &["add", "a.rs", "b.rs"]);
        git(root, &["commit", "-qm", &format!("ab {i}")]);
    }

    // 5 commits co-changing x.rs + y.rs.
    for i in 0..5u32 {
        std::fs::write(root.join("x.rs"), format!("pub fn x{i}() {{}}\n")).unwrap();
        std::fs::write(root.join("y.rs"), format!("pub fn y{i}() {{}}\n")).unwrap();
        git(root, &["add", "x.rs", "y.rs"]);
        git(root, &["commit", "-qm", &format!("xy {i}")]);
    }

    dir
}

/// Spawn a `basemind serve` subprocess and return the rmcp service handle.
async fn spawn_serve(root: &Path) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let bin = env!("CARGO_BIN_EXE_basemind");
    let cmd = AsyncCommand::new(bin).configure(|c| {
        c.arg("--root").arg(root).arg("serve").arg("--view").arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    ().serve(transport).await.expect("rmcp handshake")
}

// ─── Test 1: confidence boundary ─────────────────────────────────────────────
//
// Fixture: freq[a]=11, freq[b]=6, cochange=5 → confidence = 5/11 ≈ 0.454.
// - min_confidence=0.5 → mined==0 (0.454 < 0.5)
// - min_confidence=0.4 → mined>=1 (0.454 >= 0.4)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_reject_pair_below_confidence_threshold_and_emit_above() {
    let dir = build_confidence_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;

    // confidence ≈ 0.454 < 0.5 → rejected.
    let mine_reject = decode_text(
        &service
            .call_tool(call_params(
                "proposals_mine",
                json!({
                    "min_support": 5,
                    "min_confidence": 0.5,
                    "max_files_per_commit": 10,
                    "window": 50,
                }),
            ))
            .await
            .expect("proposals_mine confidence=0.5"),
    );
    let mined_reject = mine_reject
        .get("mined")
        .and_then(Value::as_u64)
        .expect("mined field must be present in proposals_mine response");
    assert_eq!(
        mined_reject, 0,
        "confidence≈0.454 must be REJECTED at min_confidence=0.5; got mined={mined_reject}: \
         {mine_reject}"
    );

    // confidence ≈ 0.454 >= 0.4 → accepted.
    let mine_accept = decode_text(
        &service
            .call_tool(call_params(
                "proposals_mine",
                json!({
                    "min_support": 5,
                    "min_confidence": 0.4,
                    "max_files_per_commit": 10,
                    "window": 50,
                }),
            ))
            .await
            .expect("proposals_mine confidence=0.4"),
    );
    let mined_accept = mine_accept
        .get("mined")
        .and_then(Value::as_u64)
        .expect("mined field must be present");
    assert!(
        mined_accept >= 1,
        "confidence≈0.454 must be EMITTED at min_confidence=0.4 \
         (freq[a]=11, freq[b]=6, cochange=5, anchor=a, conf=5/11≈0.454 >= 0.4); \
         got mined={mined_accept}: {mine_accept}"
    );
}

// ─── Test 2: support gating ───────────────────────────────────────────────────
//
// Same confidence fixture: cochange(a,b)=5.
// - min_support=6 → mined==0 (5 < 6)
// - min_support=5 → mined>=1 (5 >= 5)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_reject_pair_below_support_threshold_and_emit_above() {
    let dir = build_confidence_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;

    // Support=5 < 6 → no proposals.
    let mine_reject = decode_text(
        &service
            .call_tool(call_params(
                "proposals_mine",
                json!({
                    "min_support": 6,
                    "min_confidence": 0.1,
                    "max_files_per_commit": 10,
                    "window": 50,
                }),
            ))
            .await
            .expect("proposals_mine min_support=6"),
    );
    let mined_reject = mine_reject
        .get("mined")
        .and_then(Value::as_u64)
        .expect("mined field must be present");
    assert_eq!(
        mined_reject, 0,
        "cochange=5 must be REJECTED at min_support=6; got mined={mined_reject}: {mine_reject}"
    );

    // Support=5 >= 5 → at least one proposal.
    let mine_accept = decode_text(
        &service
            .call_tool(call_params(
                "proposals_mine",
                json!({
                    "min_support": 5,
                    "min_confidence": 0.1,
                    "max_files_per_commit": 10,
                    "window": 50,
                }),
            ))
            .await
            .expect("proposals_mine min_support=5"),
    );
    let mined_accept = mine_accept
        .get("mined")
        .and_then(Value::as_u64)
        .expect("mined field must be present");
    assert!(
        mined_accept >= 1,
        "cochange=5 must be EMITTED at min_support=5; got mined={mined_accept}: {mine_accept}"
    );
}

// ─── Test 3: skipped_bulk ────────────────────────────────────────────────────
//
// Fixture: 1 bulk commit (6 files) + 3 small commits (p.rs + q.rs).
// With max_files_per_commit=3:
//   - The bulk commit is skipped → skipped_bulk >= 1
//   - Pairs from the bulk commit (e.g. r.rs+s.rs) must NOT be in co-change counts
//   - p.rs+q.rs have support=3 from the small commits → mined >= 1
//   - r.rs+s.rs only appear together in the skipped bulk commit → support=0 → never mined
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_skip_bulk_commits_and_not_inflate_cochange() {
    let dir = build_bulk_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;

    let mine_body = decode_text(
        &service
            .call_tool(call_params(
                "proposals_mine",
                json!({
                    "min_support": 3,
                    "min_confidence": 0.1,
                    "max_files_per_commit": 3,
                    "window": 50,
                }),
            ))
            .await
            .expect("proposals_mine with max_files_per_commit=3"),
    );

    let skipped_bulk = mine_body
        .get("skipped_bulk")
        .and_then(Value::as_u64)
        .expect("skipped_bulk field must be present in proposals_mine response");
    assert!(
        skipped_bulk >= 1,
        "the 6-file bulk commit must be counted in skipped_bulk; \
         got skipped_bulk={skipped_bulk}: {mine_body}"
    );

    let mined = mine_body
        .get("mined")
        .and_then(Value::as_u64)
        .expect("mined field must be present");
    assert!(
        mined >= 1,
        "p.rs+q.rs co-changed in 3 small commits (support=3 >= min_support=3), \
         so at least one proposal must be emitted; got mined={mined}: {mine_body}"
    );

    // Verify that r.rs+s.rs are NOT in any mined proposal (they only co-change in the skipped
    // bulk commit). List proposals and assert none of them contain both r.rs and s.rs.
    let list_body = decode_text(
        &service
            .call_tool(call_params("proposals_list", json!({ "limit": 100, "kind": "skill" })))
            .await
            .expect("proposals_list after bulk mine"),
    );
    let proposals = list_body
        .get("proposals")
        .and_then(Value::as_array)
        .expect("proposals array must be present");

    for proposal in proposals {
        let files: Vec<&str> = proposal
            .get("files")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let has_r = files.contains(&"r.rs");
        let has_s = files.contains(&"s.rs");
        assert!(
            !(has_r && has_s),
            "r.rs+s.rs co-change only in the skipped bulk commit and must NOT appear \
             in any mined proposal; found proposal with files={files:?}: {proposal}"
        );
    }
}

// ─── Test 4: deterministic proposal_id ───────────────────────────────────────
//
// Mining the same fixture twice with identical params must produce proposals with
// the same id (content-addressed blake3 of the sorted file-set).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_produce_same_proposal_id_on_repeated_mine() {
    let dir = build_governance_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;

    let mine_params = json!({
        "min_support": 1,
        "min_confidence": 0.1,
        "max_files_per_commit": 10,
        "window": 50,
    });

    // First mine.
    let mine1 = decode_text(
        &service
            .call_tool(call_params("proposals_mine", mine_params.clone()))
            .await
            .expect("proposals_mine first"),
    );
    let mined1 = mine1.get("mined").and_then(Value::as_u64).unwrap_or(0);
    assert!(mined1 >= 1, "first mine must yield at least one proposal: {mine1}");

    let list1 = decode_text(
        &service
            .call_tool(call_params("proposals_list", json!({ "limit": 10 })))
            .await
            .expect("proposals_list after first mine"),
    );
    let ids1: Vec<String> = list1
        .get("proposals")
        .and_then(Value::as_array)
        .expect("proposals array in first list")
        .iter()
        .filter_map(|p| p.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    assert!(!ids1.is_empty(), "first proposals_list must return ids: {list1}");

    // Second mine (proposals are overwritten, not duplicated — same content-addressed ids).
    let mine2 = decode_text(
        &service
            .call_tool(call_params("proposals_mine", mine_params))
            .await
            .expect("proposals_mine second"),
    );
    let _mined2 = mine2.get("mined").and_then(Value::as_u64).unwrap_or(0);

    let list2 = decode_text(
        &service
            .call_tool(call_params("proposals_list", json!({ "limit": 10 })))
            .await
            .expect("proposals_list after second mine"),
    );
    let ids2: Vec<String> = list2
        .get("proposals")
        .and_then(Value::as_array)
        .expect("proposals array in second list")
        .iter()
        .filter_map(|p| p.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    assert!(!ids2.is_empty(), "second proposals_list must return ids: {list2}");

    // The first id from both mines must match (content-addressed over the same file-set).
    assert_eq!(
        ids1[0], ids2[0],
        "proposal ids must be deterministic across repeated mines of the same fixture: \
         first={} second={}",
        ids1[0], ids2[0]
    );
}

// ─── Test 5: proposals_list pagination + truncated + kind filter ──────────────
//
// Two disjoint clusters (a+b and x+y), each co-changing 5 times.
// Mining with min_support=5 → at least 2 proposals.
// proposals_list {limit:1} → truncated=true, next_cursor present.
// Follow cursor → second page.
// proposals_list {kind:"memory"} → empty (v1 only mines skill proposals).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_paginate_proposals_list_and_filter_by_kind() {
    let dir = build_two_cluster_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;

    // Mine both clusters.
    let mine_body = decode_text(
        &service
            .call_tool(call_params(
                "proposals_mine",
                json!({
                    "min_support": 5,
                    "min_confidence": 0.1,
                    "max_files_per_commit": 10,
                    "window": 50,
                }),
            ))
            .await
            .expect("proposals_mine two clusters"),
    );
    let mined = mine_body
        .get("mined")
        .and_then(Value::as_u64)
        .expect("mined field must be present");
    assert!(
        mined >= 2,
        "two independent co-change clusters (ab and xy) with support=5 must yield >= 2 proposals; \
         got mined={mined}: {mine_body}"
    );

    // Page 1: limit=1 → first proposal + truncated=true + next_cursor.
    let page1 = decode_text(
        &service
            .call_tool(call_params("proposals_list", json!({ "limit": 1, "kind": "skill" })))
            .await
            .expect("proposals_list page 1"),
    );
    assert_eq!(
        page1.get("truncated").and_then(Value::as_bool),
        Some(true),
        "proposals_list with limit=1 and >= 2 results must return truncated=true: {page1}"
    );
    let next_cursor = page1
        .get("next_cursor")
        .and_then(Value::as_str)
        .expect("proposals_list must return next_cursor when truncated=true")
        .to_string();
    assert!(
        !next_cursor.is_empty(),
        "next_cursor must be a non-empty string: {page1}"
    );
    let page1_ids: Vec<String> = page1
        .get("proposals")
        .and_then(Value::as_array)
        .expect("proposals array in page 1")
        .iter()
        .filter_map(|p| p.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    assert_eq!(page1_ids.len(), 1, "page 1 must contain exactly 1 proposal: {page1}");

    // Page 2: follow next_cursor → remaining proposals, truncated=false.
    let page2 = decode_text(
        &service
            .call_tool(call_params(
                "proposals_list",
                json!({ "limit": 100, "kind": "skill", "cursor": next_cursor }),
            ))
            .await
            .expect("proposals_list page 2"),
    );
    assert_eq!(
        page2.get("truncated").and_then(Value::as_bool),
        Some(false),
        "page 2 (limit=100, >= 2 total proposals) must have truncated=false: {page2}"
    );
    let page2_ids: Vec<String> = page2
        .get("proposals")
        .and_then(Value::as_array)
        .expect("proposals array in page 2")
        .iter()
        .filter_map(|p| p.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    assert!(
        !page2_ids.is_empty(),
        "page 2 must contain at least one more proposal: {page2}"
    );

    // The two pages must be disjoint (no overlap in ids).
    for id in &page2_ids {
        assert!(
            !page1_ids.contains(id),
            "proposal id {id} appeared in both page 1 and page 2 — cursor not advancing: \
             page1={page1_ids:?} page2={page2_ids:?}"
        );
    }

    // kind="memory" → v1 only mines skill proposals, so memory list is always empty.
    let memory_list = decode_text(
        &service
            .call_tool(call_params("proposals_list", json!({ "limit": 100, "kind": "memory" })))
            .await
            .expect("proposals_list kind=memory"),
    );
    let memory_proposals = memory_list
        .get("proposals")
        .and_then(Value::as_array)
        .expect("proposals array must be present even for empty kind=memory list");
    assert!(
        memory_proposals.is_empty(),
        "proposals_list kind=memory must return 0 proposals (v1 mines skills only): {memory_list}"
    );
    assert_eq!(
        memory_list.get("truncated").and_then(Value::as_bool),
        Some(false),
        "truncated must be false for empty kind=memory list: {memory_list}"
    );
}

// ─── Test 6: idempotent reject ────────────────────────────────────────────────
//
// Mine → list → reject twice (second must be a no-op, not an error).
// Re-mine → tombstone suppresses the same id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_idempotently_reject_and_tombstone_suppresses_remine() {
    let dir = build_governance_repo();
    let root = dir.path();
    run_scan(root);
    let service = spawn_serve(root).await;

    // Mine to get a candidate.
    let mine_body = decode_text(
        &service
            .call_tool(call_params(
                "proposals_mine",
                json!({
                    "min_support": 1,
                    "min_confidence": 0.1,
                    "max_files_per_commit": 10,
                    "window": 50,
                }),
            ))
            .await
            .expect("proposals_mine for reject test"),
    );
    let mined = mine_body.get("mined").and_then(Value::as_u64).unwrap_or(0);
    assert!(
        mined >= 1,
        "reject test requires at least one mined proposal: {mine_body}"
    );

    // List to get the id.
    let list_body = decode_text(
        &service
            .call_tool(call_params("proposals_list", json!({ "limit": 10 })))
            .await
            .expect("proposals_list for reject test"),
    );
    let reject_id = list_body["proposals"][0]
        .get("id")
        .and_then(Value::as_str)
        .expect("proposal id must be present")
        .to_string();

    // First reject — must return rejected=true.
    let reject1 = decode_text(
        &service
            .call_tool(call_params(
                "proposal_reject",
                json!({ "id": reject_id, "reason": "smoke-test first reject" }),
            ))
            .await
            .expect("proposal_reject first call"),
    );
    assert_eq!(
        reject1.get("rejected").and_then(Value::as_bool),
        Some(true),
        "first proposal_reject must return rejected=true: {reject1}"
    );

    // Second reject of the same id — must also return rejected=true (idempotent, no error).
    let reject2 = decode_text(
        &service
            .call_tool(call_params(
                "proposal_reject",
                json!({ "id": reject_id, "reason": "smoke-test second reject" }),
            ))
            .await
            .expect("proposal_reject second call (idempotent)"),
    );
    assert_eq!(
        reject2.get("rejected").and_then(Value::as_bool),
        Some(true),
        "second proposal_reject must also return rejected=true (idempotent): {reject2}"
    );

    // Re-mine — tombstone must suppress the rejected id.
    let mine_after = decode_text(
        &service
            .call_tool(call_params(
                "proposals_mine",
                json!({
                    "min_support": 1,
                    "min_confidence": 0.1,
                    "max_files_per_commit": 10,
                    "window": 50,
                }),
            ))
            .await
            .expect("proposals_mine after reject"),
    );

    let list_after = decode_text(
        &service
            .call_tool(call_params("proposals_list", json!({ "limit": 100 })))
            .await
            .expect("proposals_list after reject"),
    );
    let ids_after: Vec<String> = list_after
        .get("proposals")
        .and_then(Value::as_array)
        .expect("proposals array must be present after reject")
        .iter()
        .filter_map(|p| p.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    assert!(
        !ids_after.contains(&reject_id),
        "tombstoned id must NOT re-appear after a subsequent mine; \
         reject_id={reject_id} ids_after={ids_after:?}: {mine_after}"
    );
}
