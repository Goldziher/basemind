//! Real-OSS hardening harness — Stage 1 of the hardening iteration.
//!
//! Drives `basemind serve` against a previously-cloned repository (typically under
//! `/tmp/basemind-harden/`), exercises every MCP tool, asserts pass/fail criteria,
//! and emits an NDJSON record per repo for the orchestrator.
//!
//! Invocation (orchestrated by `scripts/harden.sh`):
//!
//! ```sh
//! BASEMIND_HARDEN_REPO=/tmp/basemind-harden/react \
//! BASEMIND_HARDEN_REPO_NAME=react \
//! BASEMIND_HARDEN_RESULTS=/tmp/basemind-harden/results.ndjson \
//! cargo test --release --test harden -- --ignored --nocapture --exact harden_repo
//! ```
//!
//! The single `#[ignore]`d test reads env vars and runs the per-repo suite. The test
//! is `#[ignore]`d so default `cargo test` runs are unaffected — this is a gating
//! harness, run on demand and on a nightly CI schedule, not per-PR.

#![allow(clippy::expect_used)] // it's a test harness — explicit panics on bad env are fine

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{ServiceExt, service::RoleClient, service::RunningService};
use serde_json::{Value, json};
use tokio::process::Command;

// ─── Configuration ──────────────────────────────────────────────────────────

/// Per-tool wall-clock ceiling. Any call exceeding this fails the harness.
const TOOL_TIMEOUT: Duration = Duration::from_secs(90);

/// Scan ceilings keyed by repo logical name. Defaults to 60s if missing.
fn scan_ceiling_secs(repo_name: &str) -> u64 {
    match repo_name {
        "typescript" | "TypeScript" => 600, // huge repo + intentionally-broken fixtures
        "django" => 300,
        "react" => 300,
        "tokio" => 180,
        "ripgrep" | "ripgrep-shallow" => 120,
        "requests" | "gin" => 60,
        _ => 120,
    }
}

// ─── NDJSON record shape (also serialized to results file) ──────────────────

#[derive(Debug, serde::Serialize)]
struct ToolCallRecord {
    tool: &'static str,
    ok: bool,
    elapsed_ms: u128,
    /// Microsecond resolution — the indexed git tools are sub-millisecond, so `elapsed_ms` rounds
    /// many of them to 0. This is the end-to-end MCP round-trip (transport + query), not the pure
    /// query cost; the in-process [`GitOpsMetrics`] captures the latter.
    elapsed_us: u128,
    /// Brief one-liner; for errors, includes the error code/message.
    detail: String,
}

/// One warm indexed-vs-live-walk latency comparison for a single git read query, measured
/// **in-process** (no MCP transport) at warm steady state — the pure query cost. Times are the
/// median over many iterations, in microseconds.
#[derive(Debug, serde::Serialize)]
struct GitOpsQuery {
    /// The logical query: `commits_touching` / `recent_changes` / `window_commits`.
    name: &'static str,
    /// `hot` (most-changed path), `rare` (single-touch path), or `global` (whole-history scan).
    scope: &'static str,
    /// Median latency of the posting-list-backed indexed path, µs.
    indexed_us: f64,
    /// Median latency of the live `gix` walk it replaces, µs.
    live_us: f64,
    /// `live_us / indexed_us` — how many times faster the index is.
    speedup: f64,
}

/// In-process git-history measurement for one repo: how long the index took to build, what it costs
/// on disk, and warm indexed-vs-live latency for each git read query. Built deterministically
/// (synchronous `builder::sync`) before the MCP sweep so the timings are not racing a background
/// rebuild. `None` when the repo has no commits (unborn HEAD) or the index could not open.
#[derive(Debug, serde::Serialize)]
struct GitOpsMetrics {
    /// Wall-clock of the full `builder::sync` rebuild, ms.
    build_ms: u128,
    /// `RebuildOutcome` debug string (`FullRebuild { reason, commits }` on a fresh `.basemind/`).
    outcome: String,
    /// Commits indexed.
    commits: u32,
    /// On-disk size of `.basemind/git-history.fjall/`, bytes.
    index_bytes: u64,
    /// On-disk size of `.git/`, bytes — for the index-to-repo ratio.
    git_dir_bytes: u64,
    queries: Vec<GitOpsQuery>,
}

#[derive(Debug, serde::Serialize)]
struct RepoRecord {
    repo_name: String,
    repo_path: String,
    scan_elapsed_ms: u128,
    scan_files: usize,
    scan_skipped_too_large: usize,
    scan_skipped_non_utf8: usize,
    scan_read_failed: usize,
    scan_extract_failed: usize,
    server_boot_ms: u128,
    tools: Vec<ToolCallRecord>,
    /// In-process git-history metrics (build time, index size, indexed-vs-live latency). `None` for
    /// repos with no history. Additive — older readers ignore it.
    git_history: Option<GitOpsMetrics>,
    canaries: BTreeMap<String, Value>,
}

// ─── Helpers around the MCP service ─────────────────────────────────────────

type ServiceHandle = RunningService<RoleClient, ()>;

fn basemind_bin() -> &'static str {
    env!("CARGO_BIN_EXE_basemind")
}

async fn connect(repo_root: &Path) -> ServiceHandle {
    let bin = basemind_bin();
    let root = repo_root.to_path_buf();
    let cmd = Command::new(bin).configure(|c| {
        c.arg("--root")
            .arg(&root)
            .arg("serve")
            .arg("--view")
            .arg("working");
    });
    let transport = TokioChildProcess::new(cmd).expect("spawn basemind serve");
    ().serve(transport)
        .await
        .expect("rmcp handshake with basemind serve")
}

/// Decode the first text-content item from a `CallToolResult` as JSON.
fn call_params(name: &'static str, args: &Value) -> CallToolRequestParams {
    let mut params = CallToolRequestParams::new(name);
    if let Some(obj) = args.as_object() {
        params = params.with_arguments(obj.clone());
    }
    params
}

fn decode_text(result: &CallToolResult) -> Value {
    use rmcp::model::RawContent;
    let raw = result
        .content
        .iter()
        .find_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    if raw.is_empty() {
        return Value::Null;
    }
    serde_json::from_str(&raw).unwrap_or(Value::String(raw))
}

/// Call a tool and record the result. Returns the decoded JSON body if successful
/// so per-tool drivers can chain assertions on it. Records the call either way.
async fn call(
    svc: &ServiceHandle,
    records: &mut Vec<ToolCallRecord>,
    tool: &'static str,
    args: Value,
) -> Option<Value> {
    let started = Instant::now();
    let outcome = tokio::time::timeout(TOOL_TIMEOUT, svc.call_tool(call_params(tool, &args))).await;
    let elapsed = started.elapsed();
    match outcome {
        Err(_) => {
            records.push(ToolCallRecord {
                tool,
                ok: false,
                elapsed_ms: elapsed.as_millis(),
                elapsed_us: elapsed.as_micros(),
                detail: format!("timeout after {:?}", TOOL_TIMEOUT),
            });
            None
        }
        Ok(Err(e)) => {
            records.push(ToolCallRecord {
                tool,
                ok: false,
                elapsed_ms: elapsed.as_millis(),
                elapsed_us: elapsed.as_micros(),
                detail: format!("rmcp error: {e}"),
            });
            None
        }
        Ok(Ok(result)) => {
            let body = decode_text(&result);
            // Some tools return a logically-empty body (e.g. `dependents` with no paths).
            // We still mark them ok unless `is_error` was set by the server.
            let is_error = result.is_error.unwrap_or(false);
            records.push(ToolCallRecord {
                tool,
                ok: !is_error,
                elapsed_ms: elapsed.as_millis(),
                elapsed_us: elapsed.as_micros(),
                detail: if is_error {
                    "is_error=true".to_string()
                } else {
                    "ok".to_string()
                },
            });
            Some(body)
        }
    }
}

// ─── In-process scan (avoids a second process round-trip) ───────────────────

struct ScanOutcome {
    elapsed: Duration,
    stats: basemind::scanner::ScanStats,
    sample_file: Option<SampleFile>,
}

struct SampleFile {
    /// repo-relative forward-slash path
    path: basemind::path::RelPath,
    /// non-empty when the file has at least one indexed symbol
    sample_symbol: Option<String>,
    /// non-empty when the file has at least one import with a resolved module
    sample_module: Option<String>,
}

fn run_scan(repo_root: &Path) -> ScanOutcome {
    // Ensure grammars are present before the scan — main.rs's `bootstrap_grammars`
    // wraps the same call with progress UI; tests don't need the UI.
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");

    let mut config = match basemind::config::load(repo_root) {
        Ok(c) => c,
        Err(_) => basemind::config::default_for_root(repo_root),
    };
    // The harness exercises the MCP surface; document-tier indexing adds
    // xberg + embedding cost that has nothing to do with the canaries.
    // Disable it so per-repo scan ceilings stay meaningful.
    config.documents.enabled = false;
    let mut store =
        basemind::store::Store::open(repo_root, basemind::store::VIEW_WORKING).expect("open store");
    let t0 = Instant::now();
    let report = basemind::scanner::scan(
        repo_root,
        &mut store,
        &config,
        basemind::scanner::ScanSource::WorkingTree,
    )
    .expect("scan");
    let elapsed = t0.elapsed();

    // Pick a non-empty, well-formed file to use as the per-tool argument.
    // Prefer something with imports + symbols so we can also exercise `dependents`.
    let sample_file = pick_sample(&store);

    ScanOutcome {
        elapsed,
        stats: report.stats,
        sample_file,
    }
}

fn pick_sample(store: &basemind::store::Store) -> Option<SampleFile> {
    // Iterate path → entry, read L1, pick the first file with ≥1 symbol; capture an
    // import module from anywhere in the index. Keeps the scan-time cost bounded.
    let mut sample: Option<SampleFile> = None;
    let mut fallback_module: Option<String> = None;
    for (path, entry) in &store.index.files {
        let l1 = match store.read_l1_by_hex(&entry.hash_hex) {
            Ok(Some(l1)) => l1,
            _ => continue,
        };
        if fallback_module.is_none() {
            for imp in &l1.imports {
                if let Some(m) = &imp.module {
                    fallback_module = Some(m.clone());
                    break;
                }
            }
        }
        if sample.is_none()
            && !l1.symbols.is_empty()
            && l1.symbols.iter().any(|s| !s.name.is_empty())
        {
            let sym = l1
                .symbols
                .iter()
                .find(|s| !s.name.is_empty())
                .map(|s| s.name.clone());
            let module = l1
                .imports
                .iter()
                .find_map(|i| i.module.clone())
                .or_else(|| fallback_module.clone());
            sample = Some(SampleFile {
                path: path.clone(),
                sample_symbol: sym,
                sample_module: module,
            });
        }
        if sample.is_some() && fallback_module.is_some() {
            break;
        }
    }
    if let Some(s) = sample.as_mut()
        && s.sample_module.is_none()
    {
        s.sample_module = fallback_module;
    }
    sample
}

// ─── Drive every tool against one repo ──────────────────────────────────────

async fn drive_tools(svc: &ServiceHandle, sample: Option<&SampleFile>) -> Vec<ToolCallRecord> {
    let mut records: Vec<ToolCallRecord> = Vec::with_capacity(20);

    let _ = svc.list_tools(None).await;

    call(svc, &mut records, "status", json!({})).await;
    call(svc, &mut records, "list_files", json!({ "limit": 50 })).await;
    call(
        svc,
        &mut records,
        "search_symbols",
        json!({ "needle": "test", "limit": 50 }),
    )
    .await;
    call(
        svc,
        &mut records,
        "workspace_grep",
        json!({ "pattern": "fn ", "limit": 50, "include_context": false }),
    )
    .await;

    if let Some(sample) = sample {
        call(
            svc,
            &mut records,
            "outline",
            json!({ "path": &sample.path, "l2": false }),
        )
        .await;

        if let Some(module) = &sample.sample_module {
            call(svc, &mut records, "dependents", json!({ "module": module })).await;
        }

        // Git tools — these short-circuit cleanly when no repo is present, so we
        // don't need a separate non-git branch.
        call(svc, &mut records, "working_tree_status", json!({})).await;
        call(svc, &mut records, "repo_info", json!({})).await;
        call(
            svc,
            &mut records,
            "recent_changes",
            json!({ "limit": 20, "include_files": true }),
        )
        .await;
        call(
            svc,
            &mut records,
            "commits_touching",
            json!({ "path": &sample.path, "limit": 10 }),
        )
        .await;
        call(
            svc,
            &mut records,
            "find_commits_by_path",
            json!({ "pattern": "\\.md$", "window": 200, "limit": 20 }),
        )
        .await;
        call(
            svc,
            &mut records,
            "search_git_history",
            json!({ "pattern": "fix", "field": "message", "limit": 20 }),
        )
        .await;
        call(
            svc,
            &mut records,
            "hot_files",
            json!({ "window": 200, "top_k": 20 }),
        )
        .await;
        call(
            svc,
            &mut records,
            "diff_outline",
            json!({ "path": &sample.path, "rev": "HEAD" }),
        )
        .await;
        call(
            svc,
            &mut records,
            "diff_file",
            json!({ "path": &sample.path, "rev_old": "HEAD~1", "rev_new": "HEAD" }),
        )
        .await;
        call(
            svc,
            &mut records,
            "blame_file",
            json!({ "path": &sample.path }),
        )
        .await;

        if let Some(sym) = &sample.sample_symbol {
            call(
                svc,
                &mut records,
                "blame_symbol",
                json!({ "path": &sample.path, "name": sym }),
            )
            .await;
            call(
                svc,
                &mut records,
                "symbol_history",
                json!({ "path": &sample.path, "name": sym, "limit": 20 }),
            )
            .await;
            // Stage 3 canary: reference search on the sampled symbol. Just confirm the
            // call succeeds — hit count varies wildly per repo (a `pub fn` in Rust vs an
            // `export const` in TS), so the bare-success is the only stable assertion.
            call(
                svc,
                &mut records,
                "find_references",
                json!({ "name": sym, "limit": 100 }),
            )
            .await;
            // Iteration-4 sweep: shallow call_graph walk on the same sampled symbol.
            // Bare-success assertion only; node count varies per repo.
            call(
                svc,
                &mut records,
                "call_graph",
                json!({ "name": sym, "direction": "callers", "max_depth": 2 }),
            )
            .await;
        }
    }

    // find_implementations: sweep with a common trait name. Use "Future" as a universally
    // present trait in Rust repos; falls back gracefully to 0 hits for non-Rust repos.
    call(
        svc,
        &mut records,
        "find_implementations",
        json!({ "trait_name": "Future", "limit": 100 }),
    )
    .await;

    // compress (structural): sweep with the sample file path when available.
    if let Some(sample) = sample {
        call(
            svc,
            &mut records,
            "compress",
            json!({ "path": &sample.path }),
        )
        .await;

        // expand: pull the first symbol's body from the same sample file.
        // We only call expand when a sample_symbol is available; errors on
        // languages without indexed symbols are expected and tolerated.
        if let Some(sym) = &sample.sample_symbol {
            call(
                svc,
                &mut records,
                "expand",
                json!({ "path": &sample.path, "name": sym }),
            )
            .await;
        }
    }

    // compress (prose): always available, no feature gate.
    call(
        svc,
        &mut records,
        "compress",
        json!({ "text": "It is worth noting that basemind provides code-aware compression. The index is fast." }),
    )
    .await;

    // Memory + document tools: sweep unconditionally (MCP error when features off is ok).
    call(
        svc,
        &mut records,
        "memory_put",
        json!({ "key": "harden_probe", "value": "basemind harden probe", "embed": false }),
    )
    .await;
    call(
        svc,
        &mut records,
        "memory_get",
        json!({ "key": "harden_probe" }),
    )
    .await;
    call(svc, &mut records, "memory_list", json!({})).await;
    call(
        svc,
        &mut records,
        "memory_delete",
        json!({ "key": "harden_probe" }),
    )
    .await;
    // memory_audit: write a probe key then sweep; MCP error when memory feature is off is ok.
    call(
        svc,
        &mut records,
        "memory_put",
        json!({ "key": "harden_audit_probe", "value": "audit probe", "embed": false }),
    )
    .await;
    call(
        svc,
        &mut records,
        "memory_audit",
        json!({ "key": "harden_audit_probe", "dry_run": true }),
    )
    .await;
    call(
        svc,
        &mut records,
        "memory_delete",
        json!({ "key": "harden_audit_probe" }),
    )
    .await;
    call(
        svc,
        &mut records,
        "search_documents",
        json!({ "query": "code map scanner" }),
    )
    .await;

    // proposals_mine: co-change mining over recent history. MCP error when memory feature is
    // off is ok (same gate as memory_audit). On success we just verify the call completes
    // without error — candidate count varies wildly per repo and mining threshold.
    call(
        svc,
        &mut records,
        "proposals_mine",
        json!({ "window": 100, "min_support": 5, "min_confidence": 0.6 }),
    )
    .await;
    call(
        svc,
        &mut records,
        "proposals_list",
        json!({ "kind": "skill", "limit": 20 }),
    )
    .await;

    // Cache admin tools: both must succeed on every repo. cache_stats is read-only;
    // cache_gc reclaims orphaned blobs (safe in-process under the server's lock).
    call(svc, &mut records, "cache_stats", json!({})).await;
    call(svc, &mut records, "cache_gc", json!({})).await;

    // Agent shells: spawn a trivial self-exiting session, capture it, then kill it — exercising the
    // embedded rmux daemon end-to-end. MCP error when the `shells` feature is off is ok (the tool is
    // simply unregistered), same as the memory/document sweep above. On success we chain the real
    // session id through capture + kill so the canary leaves no live session behind.
    if let Some(spawned) = call(
        svc,
        &mut records,
        "shell_spawn",
        json!({ "command": "echo basemind-harden-shell" }),
    )
    .await
        && let Some(session_id) = spawned.get("session_id").and_then(Value::as_str)
    {
        assert!(
            session_id.starts_with("bmsh-"),
            "shell_spawn session_id should be a minted bmsh- id, got {session_id:?}"
        );
        let session = json!({ "session_id": session_id });
        call(svc, &mut records, "shell_capture", session.clone()).await;
        call(svc, &mut records, "shell_kill", session).await;
    }
    call(svc, &mut records, "shell_list", json!({})).await;

    records
}

// ─── Per-repo assertions ────────────────────────────────────────────────────

/// Returns the human-readable failure summary if anything tripped; None on pass.
fn assert_passing(
    repo_name: &str,
    scan: &ScanOutcome,
    repo_record: &mut RepoRecord,
) -> Vec<String> {
    let mut failures: Vec<String> = Vec::new();
    let ceiling = Duration::from_secs(scan_ceiling_secs(repo_name));
    if scan.elapsed > ceiling {
        failures.push(format!(
            "scan elapsed {:.1}s > ceiling {:.1}s",
            scan.elapsed.as_secs_f32(),
            ceiling.as_secs_f32()
        ));
    }
    if scan.stats.scanned == 0 {
        failures.push("scan touched zero files".to_string());
    }

    // Generic per-tool: any !ok or timeout fails the harness — except tolerated responses that are
    // not malfunctions:
    //   * "requires the X feature" / "tool not found" — the tool isn't compiled into this binary. The
    //     published release includes every feature, but a reduced build (e.g. on a machine where the
    //     documents/memory/intelligence stack can't compile) leaves those tools unregistered. The
    //     harness still measures scan + git-ops on whatever binary it's given.
    //   * "disambiguate" — `expand` was fed a symbol name the generic sweep sampled that happens to
    //     match several symbols; returning a disambiguation error is the tool behaving correctly, not
    //     failing. (The expand contract is covered by its own smoke test, not this sweep.)
    for r in &repo_record.tools {
        let tolerated = !r.ok
            && (r.detail.contains("requires the")
                || r.detail.contains("tool not found")
                || r.detail.contains("disambiguate"));
        if !r.ok && !tolerated {
            failures.push(format!("{} failed: {}", r.tool, r.detail));
        }
        if r.elapsed_ms > TOOL_TIMEOUT.as_millis() {
            failures.push(format!(
                "{} ran {}ms > timeout {}ms",
                r.tool,
                r.elapsed_ms,
                TOOL_TIMEOUT.as_millis()
            ));
        }
    }

    // Global git-ops canaries (every git repo): the index must build, and on a repo with real
    // history the indexed hot-path query must not be slower than the live walk it replaces. The
    // depth gate keeps tiny repos (where the live walk is itself sub-microsecond) from flaking on
    // fixed Fjall overhead — there the numbers are still recorded, just not asserted.
    if let Some(m) = &repo_record.git_history {
        if m.commits == 0 {
            failures.push("git-history index built zero commits".to_string());
        }
        if let Some(ct) = m
            .queries
            .iter()
            .find(|q| q.name == "commits_touching" && q.scope == "hot")
            && m.commits >= 1000
            && ct.indexed_us > ct.live_us
        {
            failures.push(format!(
                "indexed commits_touching ({:.2}µs) slower than live walk ({:.2}µs) on {} commits",
                ct.indexed_us, ct.live_us, m.commits
            ));
        }
    }

    // Repo-specific canaries. These are the gating bits the iteration is meant
    // to flip green — they will fail on the baseline run, by design.
    match repo_name {
        "react" => {
            // After Stage 2, search_symbols("useState") returns ≥ 1 hit.
            // The canary records what we actually got so the orchestrator can diff
            // baselines across runs.
            let hit_count = repo_record
                .canaries
                .get("useState_hits")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if hit_count == 0 {
                failures.push("react canary: search_symbols(\"useState\") returned 0 hits".into());
            }
        }
        name if name.ends_with("-shallow") => {
            // After Stage 4, history-walking responses surface `truncated: true`.
            let truncated = repo_record
                .canaries
                .get("any_truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !truncated {
                failures
                    .push("shallow canary: no history-walking tool reported truncated=true".into());
            }
        }
        "tokio" => {
            // Iteration-3 canary: tokio is the canonical async-call corpus. `spawn` is
            // called in dozens of places throughout the runtime; ≥ 50 is conservative.
            let hits = repo_record
                .canaries
                .get("spawn_hits")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if hits < 50 {
                failures.push(format!(
                    "tokio canary: find_references(\"spawn\") returned {hits} hits (expected ≥ 50)"
                ));
            }
            // workspace_grep canary: `fn spawn` appears in many source files in tokio.
            // The pattern matches both function definitions and call-like patterns — at
            // least 20 hits is a very conservative lower bound for a repo this size.
            let grep_hits = repo_record
                .canaries
                .get("grep_fn_spawn_hits")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if grep_hits < 20 {
                failures.push(format!(
                    "tokio canary: workspace_grep(\"fn spawn\") returned {grep_hits} hits (expected ≥ 20)"
                ));
            }
            // find_implementations canary: tokio's `Future` trait has many implementors.
            let future_hits = repo_record
                .canaries
                .get("future_impl_hits")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if future_hits < 20 {
                failures.push(format!(
                    "tokio canary: find_implementations(\"Future\") returned {future_hits} hits (expected ≥ 20)"
                ));
            }
            // Iteration-4 canary: call_graph upward from `spawn` (depth=2) must surface
            // at least 5 nodes. Conservative lower bound — `spawn` is invoked from dozens
            // of helpers/spawners.
            let cg_nodes = repo_record
                .canaries
                .get("spawn_call_graph_nodes")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if cg_nodes < 5 {
                failures.push(format!(
                    "tokio canary: call_graph(\"spawn\", callers, depth=2) returned {cg_nodes} nodes (expected ≥ 5)"
                ));
            }
        }
        "django" => {
            // Iteration-3 canary: `get` is overloaded in Django (ORM queryset method, view
            // dispatch, dict access). Should saturate the limit easily.
            let hits = repo_record
                .canaries
                .get("get_hits")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if hits < 50 {
                failures.push(format!(
                    "django canary: find_references(\"get\") returned {hits} hits (expected ≥ 50)"
                ));
            }
            // git-history canary: `django/db/models/query.py` has been edited across many releases.
            // ≥ 10 commits is a conservative, churn-stable lower bound (it has hundreds in reality).
            // search_git_history canary: "fixed" is in a large share of Django commit messages;
            // ≥ 20 is a very conservative lower bound (limit=100 caps the page, so the hard floor
            // is really "the page filled well past 20").
            let search_fixed = repo_record
                .canaries
                .get("search_fixed_commits")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if search_fixed < 20 {
                failures.push(format!(
                    "django canary: search_git_history(\"fixed\", message) returned {search_fixed} commits (expected ≥ 20)"
                ));
            }
            let query_commits = repo_record
                .canaries
                .get("query_py_commits")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if query_commits < 10 {
                failures.push(format!(
                    "django canary: commits_touching(\"django/db/models/query.py\") returned {query_commits} commits (expected ≥ 10)"
                ));
            }
            // Governance canary — enforced only when mining actually ran (the `proposals_mined`
            // value is present, i.e. the harness was built with `--features memory`). Django
            // yields several co-change candidates at the default thresholds; ≥ 1 is a
            // conservative, churn-stable lower bound.
            if let Some(mined) = repo_record
                .canaries
                .get("proposals_mined")
                .and_then(Value::as_u64)
                && mined < 1
            {
                failures.push(format!(
                    "django canary: proposals_mine (default thresholds) returned {mined} candidates (expected ≥ 1)"
                ));
            }
        }
        _ => {}
    }

    failures
}

async fn capture_canaries(svc: &ServiceHandle, repo_name: &str, record: &mut RepoRecord) {
    // Always evaluate canaries on a best-effort basis — failures here don't block
    // the rest of the suite. We feed the results back into assert_passing.
    match repo_name {
        "react" => {
            let res = svc
                .call_tool(call_params(
                    "search_symbols",
                    &json!({ "needle": "useState", "limit": 20 }),
                ))
                .await;
            if let Ok(out) = res {
                let body = decode_text(&out);
                let hits = body
                    .get("results")
                    .and_then(Value::as_array)
                    .map(|a| a.len() as u64)
                    .unwrap_or(0);
                record.canaries.insert("useState_hits".into(), json!(hits));
            }
        }
        name if name.ends_with("-shallow") => {
            // Ask a recent-changes call; after Stage 4 we expect `truncated` to bubble up.
            let mut truncated = false;
            for tool in ["recent_changes", "blame_file"] {
                let args = if tool == "blame_file" {
                    json!({
                        "path": record
                            .tools
                            .iter()
                            .find(|t| t.tool == "blame_file")
                            .map(|_| "README.md")
                            .unwrap_or("README.md")
                    })
                } else {
                    json!({ "limit": 5, "include_files": false })
                };
                if let Ok(out) = svc.call_tool(call_params(tool, &args)).await {
                    let body = decode_text(&out);
                    if body.get("truncated").and_then(Value::as_bool) == Some(true) {
                        truncated = true;
                        break;
                    }
                }
            }
            record
                .canaries
                .insert("any_truncated".into(), json!(truncated));
        }
        "tokio" => {
            if let Ok(out) = svc
                .call_tool(call_params(
                    "find_references",
                    &json!({ "name": "spawn", "limit": 200 }),
                ))
                .await
            {
                let body = decode_text(&out);
                let hits = body
                    .get("hits")
                    .and_then(Value::as_array)
                    .map(|a| a.len() as u64)
                    .unwrap_or(0);
                record.canaries.insert("spawn_hits".into(), json!(hits));
            }
            // find_implementations canary: tokio implements `Future` in many places.
            if let Ok(out) = svc
                .call_tool(call_params(
                    "find_implementations",
                    &json!({ "trait_name": "Future", "limit": 200 }),
                ))
                .await
            {
                let body = decode_text(&out);
                let hits = body
                    .get("hits")
                    .and_then(Value::as_array)
                    .map(|a| a.len() as u64)
                    .unwrap_or(0);
                record
                    .canaries
                    .insert("future_impl_hits".into(), json!(hits));
            }
            // Iteration-4 canary: call_graph callers from `spawn`, max_depth=2. tokio has
            // dense indirection around its runtime spawn helpers, so the BFS should pull
            // in well more than a handful of nodes even at depth 2.
            if let Ok(out) = svc
                .call_tool(call_params(
                    "call_graph",
                    &json!({ "name": "spawn", "direction": "callers", "max_depth": 2, "max_nodes": 500 }),
                ))
                .await
            {
                let body = decode_text(&out);
                let nodes = body
                    .get("nodes")
                    .and_then(Value::as_array)
                    .map(|a| a.len() as u64)
                    .unwrap_or(0);
                record
                    .canaries
                    .insert("spawn_call_graph_nodes".into(), json!(nodes));
            }
            // workspace_grep canary for tokio: count "fn spawn" across source files.
            if let Ok(out) = svc
                .call_tool(call_params(
                    "workspace_grep",
                    &json!({ "pattern": "fn spawn", "limit": 200, "include_context": false }),
                ))
                .await
            {
                let body = decode_text(&out);
                let hits = body
                    .get("total_matches")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                record
                    .canaries
                    .insert("grep_fn_spawn_hits".into(), json!(hits));
            }
        }
        "django" => {
            if let Ok(out) = svc
                .call_tool(call_params(
                    "find_references",
                    &json!({ "name": "get", "limit": 200 }),
                ))
                .await
            {
                let body = decode_text(&out);
                let hits = body
                    .get("hits")
                    .and_then(Value::as_array)
                    .map(|a| a.len() as u64)
                    .unwrap_or(0);
                record.canaries.insert("get_hits".into(), json!(hits));
            }
            // git-history canary: `django/db/models/query.py` is a foundational, long-lived file
            // touched by many commits. Served by the precomputed git-history index when the serve's
            // background sync has caught up, else by the live walk — both must agree, so a stable
            // lower bound holds regardless. Validates the history index end-to-end on a deep repo.
            if let Ok(out) = svc
                .call_tool(call_params(
                    "commits_touching",
                    &json!({ "path": "django/db/models/query.py", "limit": 100 }),
                ))
                .await
            {
                let body = decode_text(&out);
                let hits = body
                    .get("commits")
                    .and_then(Value::as_array)
                    .map(|a| a.len() as u64)
                    .unwrap_or(0);
                record
                    .canaries
                    .insert("query_py_commits".into(), json!(hits));
            }
            // git-history FTS canary: Django's commit convention is "Fixed #NNNNN -- …", so the
            // message token "fixed" appears in a huge fraction of commits. Exercises
            // search_git_history end-to-end (indexed term postings, or the live fallback over the
            // recent window — both search summaries), a stable high lower bound either way.
            if let Ok(out) = svc
                .call_tool(call_params(
                    "search_git_history",
                    &json!({ "pattern": "fixed", "field": "message", "limit": 100 }),
                ))
                .await
            {
                let body = decode_text(&out);
                let hits = body
                    .get("commits")
                    .and_then(Value::as_array)
                    .map(|a| a.len() as u64)
                    .unwrap_or(0);
                record
                    .canaries
                    .insert("search_fixed_commits".into(), json!(hits));
            }
            // Governance canary — only populated under `--features memory`; with the feature
            // off, `proposals_mine` returns an MCP error and the canary stays absent (so the
            // assertion below is skipped on default-feature runs). Django's co-change history
            // yields several candidates at the harness default thresholds (measured ~4 at
            // window=100/support=5/conf=0.6).
            if let Ok(out) = svc
                .call_tool(call_params(
                    "proposals_mine",
                    &json!({ "window": 100, "min_support": 5, "min_confidence": 0.6 }),
                ))
                .await
            {
                let body = decode_text(&out);
                if let Some(mined) = body.get("mined").and_then(Value::as_u64) {
                    record
                        .canaries
                        .insert("proposals_mined".into(), json!(mined));
                }
            }
        }
        _ => {}
    }
}

// ─── In-process git-ops measurement (warm, microsecond, indexed vs live) ─────

/// Warm-up iterations discarded before timing (let the block cache + branch predictor settle).
const GITOPS_WARMUP: usize = 8;
/// Timed iterations for the indexed (µs-scale) path.
const GITOPS_ITERS_INDEXED: usize = 300;
/// Timed iterations for the live walk — fewer, since each is far slower and we only need a median.
const GITOPS_ITERS_LIVE: usize = 25;

/// Build the git-history index for `repo_root` synchronously (so its state is deterministic, not
/// racing `serve`'s background sync), then measure warm indexed-vs-live latency for the git read
/// queries plus the build time and on-disk index size. Returns `None` for a repo with no history.
///
/// This is the in-process, pure-query measurement (no MCP transport) — the µs-scale numbers the
/// README's git-ops section reports. It reuses the exact public APIs `benches/git_history.rs` does.
fn measure_git_ops(repo_root: &Path) -> Option<GitOpsMetrics> {
    use basemind::git::Repo;
    use basemind::git_history::{GitHistoryIndex, builder};

    let repo = Repo::discover(repo_root).ok()?;
    let bdir = repo_root.join(".basemind");
    std::fs::create_dir_all(&bdir).ok()?;
    let index = GitHistoryIndex::open(&bdir).ok()?;

    // Deterministic full build, timed. On a fresh `.basemind/` this is a from-scratch rebuild.
    let t0 = Instant::now();
    let outcome = builder::sync(&index, &repo, &bdir).ok()?;
    let build_ms = t0.elapsed().as_millis();
    let commits = index.commit_count();
    if commits == 0 {
        return None; // unborn / empty repo — nothing to measure
    }

    let index_bytes = dir_size(&bdir.join("git-history.fjall"));
    let git_dir_bytes = dir_size(&repo_root.join(".git"));
    let (hot, rare) = sample_paths(&index)?;

    let queries = vec![
        bench_query(
            "commits_touching",
            "hot",
            || index.commits_touching(&hot, 0, 50).len(),
            || repo.log_for_path(&hot, 50).map(|v| v.len()).unwrap_or(0),
        ),
        bench_query(
            "commits_touching",
            "rare",
            || index.commits_touching(&rare, 0, 50).len(),
            || repo.log_for_path(&rare, 50).map(|v| v.len()).unwrap_or(0),
        ),
        bench_query(
            "recent_changes",
            "global",
            || index.recent_commits(0, 50, false).len(),
            || repo.log_paths(50, false).map(|v| v.len()).unwrap_or(0),
        ),
        bench_query(
            "window_commits",
            "global",
            || index.window_commits(300).len(),
            || repo.log_paths(300, true).map(|v| v.len()).unwrap_or(0),
        ),
    ];

    // Drop the index handle (releasing the Fjall lock) before `serve` opens it for the MCP sweep.
    drop(index);
    Some(GitOpsMetrics {
        build_ms,
        outcome: format!("{outcome:?}"),
        commits,
        index_bytes,
        git_dir_bytes,
        queries,
    })
}

/// Sample a `(hot, rare)` path pair from the index's recent history: the most-changed path in the
/// newest window is "hot", a single-touch path is "rare". Mirrors `benches/git_history.rs`.
fn sample_paths(
    index: &basemind::git_history::GitHistoryIndex,
) -> Option<(basemind::path::RelPath, basemind::path::RelPath)> {
    use basemind::path::RelPath;
    let window = index.window_commits(2000);
    let mut counts: ahash::AHashMap<RelPath, usize> = ahash::AHashMap::new();
    for commit in &window {
        for (rel, _) in &commit.files {
            *counts.entry(rel.clone()).or_default() += 1;
        }
    }
    let hot = counts
        .iter()
        .max_by_key(|(_, n)| **n)
        .map(|(p, _)| p.clone())?;
    let rare = counts
        .iter()
        .find(|(_, n)| **n == 1)
        .map(|(p, _)| p.clone())
        .unwrap_or_else(|| hot.clone());
    Some((hot, rare))
}

/// Warm A/B: time the indexed and live closures back-to-back (shared thermal/cache conditions) and
/// return their median latencies in µs plus the speedup.
fn bench_query(
    name: &'static str,
    scope: &'static str,
    mut indexed: impl FnMut() -> usize,
    mut live: impl FnMut() -> usize,
) -> GitOpsQuery {
    let indexed_ns = median_ns(GITOPS_ITERS_INDEXED, &mut indexed);
    let live_ns = median_ns(GITOPS_ITERS_LIVE, &mut live);
    let indexed_us = indexed_ns as f64 / 1000.0;
    let live_us = live_ns as f64 / 1000.0;
    let speedup = if indexed_us > 0.0 {
        live_us / indexed_us
    } else {
        0.0
    };
    GitOpsQuery {
        name,
        scope,
        indexed_us,
        live_us,
        speedup,
    }
}

/// Median per-call latency in nanoseconds over `iters` timed iterations (after a warm-up). Nanosecond
/// resolution so sub-microsecond indexed calls don't round to zero.
fn median_ns(iters: usize, f: &mut impl FnMut() -> usize) -> u128 {
    for _ in 0..GITOPS_WARMUP {
        std::hint::black_box(f());
    }
    let mut samples: Vec<u128> = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = Instant::now();
        std::hint::black_box(f());
        samples.push(start.elapsed().as_nanos());
    }
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// Recursively sum **actual on-disk usage** under `dir` (0 if absent). Uses allocated 512-byte
/// blocks, not logical length — Fjall preallocates its journal as a sparse file whose `len()` is
/// far larger than the bytes really on disk, so `len()` would wildly over-report the index size
/// (e.g. report 64 MB for a 680 KB index). This matches what `du` shows.
fn dir_size(dir: &Path) -> u64 {
    let mut acc = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        match entry.metadata() {
            Ok(md) if md.is_dir() => acc += dir_size(&entry.path()),
            Ok(md) => acc += on_disk_size(&md),
            Err(_) => {}
        }
    }
    acc
}

/// Allocated on-disk size of a file. Unix exposes 512-byte block counts, which correctly
/// account for Fjall's sparse journal; other platforms fall back to logical length.
#[cfg(unix)]
fn on_disk_size(md: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.blocks() * 512
}

#[cfg(not(unix))]
fn on_disk_size(md: &std::fs::Metadata) -> u64 {
    md.len()
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Append a paste-ready markdown git-ops table for one repo to `<results_dir>/gitops.md`, so the
/// README author (and `harness-interpreter`) can read the numbers without parsing NDJSON.
fn append_gitops_md(repo_name: &str, m: &GitOpsMetrics) {
    let Ok(results) = std::env::var("BASEMIND_HARDEN_RESULTS") else {
        return;
    };
    let md = Path::new(&results).with_file_name("gitops.md");
    let mut out = String::new();
    out.push_str(&format!(
        "### {repo_name} — {} commits, index {} ({:.1}% of .git), full build {} ms\n\n",
        m.commits,
        human_bytes(m.index_bytes),
        if m.git_dir_bytes > 0 {
            100.0 * m.index_bytes as f64 / m.git_dir_bytes as f64
        } else {
            0.0
        },
        m.build_ms,
    ));
    out.push_str("| query | scope | indexed µs | live-walk µs | speedup |\n");
    out.push_str("|---|---|---|---|---|\n");
    for q in &m.queries {
        out.push_str(&format!(
            "| {} | {} | {:.2} | {:.2} | {:.0}× |\n",
            q.name, q.scope, q.indexed_us, q.live_us, q.speedup
        ));
    }
    out.push('\n');
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&md) {
        let _ = write!(f, "{out}");
    }
}

// ─── NDJSON output ──────────────────────────────────────────────────────────

fn append_results(record: &RepoRecord) {
    let Ok(path) = std::env::var("BASEMIND_HARDEN_RESULTS") else {
        return;
    };
    let Ok(line) = serde_json::to_string(record) else {
        return;
    };
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

// ─── The harness entry point ────────────────────────────────────────────────

/// Single ignored test that exercises one repo per invocation. Spawn via the
/// orchestrator script — it iterates the configured repo set and runs `cargo
/// test` once per clone with a different `BASEMIND_HARDEN_REPO`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real-OSS hardening harness; invoke via scripts/harden.sh"]
async fn harden_repo() {
    let repo = std::env::var("BASEMIND_HARDEN_REPO")
        .map(PathBuf::from)
        .expect("BASEMIND_HARDEN_REPO must point at a cloned repository");
    assert!(
        repo.is_dir(),
        "BASEMIND_HARDEN_REPO does not exist or is not a directory: {}",
        repo.display()
    );
    let repo_name = std::env::var("BASEMIND_HARDEN_REPO_NAME").unwrap_or_else(|_| {
        repo.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string()
    });

    eprintln!("[harden] repo={} ({})", repo_name, repo.display());

    // 1. Scan in-process on a blocking thread — the scanner is sync and may
    // reach the doc tier, which opens a LanceStore that owns its own tokio
    // runtime. Running it under spawn_blocking strips the test's tokio TLS so
    // LanceStore's `block_on` is safe.
    let scan = {
        let repo = repo.clone();
        tokio::task::spawn_blocking(move || run_scan(&repo))
            .await
            .expect("scan join")
    };
    eprintln!(
        "[harden] scan: {} files in {:.1}s ({} updated, {} read_failed, {} extract_failed)",
        scan.stats.scanned,
        scan.elapsed.as_secs_f32(),
        scan.stats.updated,
        scan.stats.read_failed,
        scan.stats.extract_failed
    );

    // 1.5 Build the git-history index synchronously and measure warm indexed-vs-live latency BEFORE
    //     starting `serve`. Deterministic (no background-sync race), and it leaves the index fresh so
    //     the MCP git tools below hit the indexed path. Runs on a blocking thread (gix + Fjall).
    let git_history = {
        let repo = repo.clone();
        tokio::task::spawn_blocking(move || measure_git_ops(&repo))
            .await
            .expect("git-ops measure join")
    };
    if let Some(m) = &git_history {
        eprintln!(
            "[harden] git-history: {} commits, build {}ms, index {} ({:.1}% of .git)",
            m.commits,
            m.build_ms,
            human_bytes(m.index_bytes),
            if m.git_dir_bytes > 0 {
                100.0 * m.index_bytes as f64 / m.git_dir_bytes as f64
            } else {
                0.0
            },
        );
        for q in &m.queries {
            eprintln!(
                "[harden]   {} ({}): indexed {:.2}µs vs live {:.2}µs — {:.0}× faster",
                q.name, q.scope, q.indexed_us, q.live_us, q.speedup
            );
        }
        append_gitops_md(&repo_name, m);
    }

    // 2. Spawn `basemind serve` and connect via rmcp's child-process transport.
    let boot_start = Instant::now();
    let svc = connect(&repo).await;
    let server_boot_ms = boot_start.elapsed().as_millis();
    eprintln!("[harden] server boot: {}ms", server_boot_ms);

    // 3. Walk every MCP tool.
    let tools = drive_tools(&svc, scan.sample_file.as_ref()).await;

    let mut record = RepoRecord {
        repo_name: repo_name.clone(),
        repo_path: repo.display().to_string(),
        scan_elapsed_ms: scan.elapsed.as_millis(),
        scan_files: scan.stats.scanned,
        scan_skipped_too_large: scan.stats.skipped_too_large,
        scan_skipped_non_utf8: scan.stats.skipped_non_utf8,
        scan_read_failed: scan.stats.read_failed,
        scan_extract_failed: scan.stats.extract_failed,
        server_boot_ms,
        tools,
        git_history,
        canaries: BTreeMap::new(),
    };

    // 3.5 Git-ops canaries (every git repo): the index built, and the indexed hot-path query is no
    //     slower than the live walk it replaces. Recorded for all repos; asserted in `assert_passing`
    //     only where the history is deep enough for the comparison to be stable.
    if let Some(m) = &record.git_history {
        record
            .canaries
            .insert("gh_index_commits".to_string(), json!(m.commits));
        if let Some(ct) = m
            .queries
            .iter()
            .find(|q| q.name == "commits_touching" && q.scope == "hot")
        {
            record
                .canaries
                .insert("gh_ct_hot_indexed_us".to_string(), json!(ct.indexed_us));
            record
                .canaries
                .insert("gh_ct_hot_live_us".to_string(), json!(ct.live_us));
            record
                .canaries
                .insert("gh_ct_hot_speedup".to_string(), json!(ct.speedup));
        }
    }

    // 4. Per-repo canary captures (read-only; results go into record.canaries).
    capture_canaries(&svc, &repo_name, &mut record).await;

    // 5. Persist the per-repo record before assertions so we get partial data
    //    even when a later step panics.
    append_results(&record);

    // 6. Clean shutdown so the child exits before the test returns.
    let _ = svc.cancel().await;

    // 7. Assert pass/fail.
    let failures = assert_passing(&repo_name, &scan, &mut record);
    if !failures.is_empty() {
        // Re-append after canaries were materialized into the record so the
        // failures and final canary values stay in sync on disk.
        append_results(&record);
        panic!(
            "[harden] {} failed {} check(s):\n  - {}",
            repo_name,
            failures.len(),
            failures.join("\n  - ")
        );
    }

    eprintln!("[harden] {} clean", repo_name);
}
