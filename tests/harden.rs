//! Real-OSS hardening harness — Stage 1 of the hardening iteration.
//!
//! Drives an in-process `basemind` MCP server (`serve_in_memory`, over an in-memory duplex
//! transport — the local-writer topology, not a spawned child process) against a
//! previously-cloned repository (typically under `/tmp/basemind-harden/`), exercises every MCP
//! tool, asserts pass/fail criteria, and emits an NDJSON record per repo for the orchestrator.
//! HTTP-transport and `comms`-build `daemon_writer` coverage are NOT this harness's concern —
//! that lives in `tests/git_history_daemon.rs`.
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

#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::{ServiceExt, service::RoleClient, service::RunningService};
use serde_json::{Value, json};

#[path = "harden/worktrees.rs"]
mod worktrees;

/// Per-tool wall-clock ceiling. Any call exceeding this fails the harness.
const TOOL_TIMEOUT: Duration = Duration::from_secs(90);

/// Scan ceilings keyed by repo logical name. Defaults to 60s if missing.
fn scan_ceiling_secs(repo_name: &str) -> u64 {
    match repo_name {
        "typescript" | "TypeScript" => 600,
        "django" => 300,
        "react" => 300,
        "tokio" => 180,
        "ripgrep" | "ripgrep-shallow" => 120,
        "requests" | "gin" => 60,
        _ => 120,
    }
}

#[derive(Debug, serde::Serialize)]
struct ToolCallRecord {
    /// The tool name plus what selects the operation: see [`record_label`].
    tool: String,
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

type ServiceHandle = RunningService<RoleClient, ()>;

async fn connect(repo_root: &Path) -> ServiceHandle {
    let transport = basemind::mcp::serve_in_memory(repo_root, "working")
        .await
        .expect("in-memory serve");
    ().serve(transport).await.expect("rmcp handshake with basemind serve")
}

/// Every tool the harness may call: the verb tools plus the agent tools.
///
/// A fixed list rather than the server's advertised one, because the agent tools are cfg-gated:
/// under a default-feature build they are absent, and the harness calls them deliberately so it can
/// record them as skipped. In-process they answer that the daemon serves them, also recorded as
/// skipped: `tests/agent_tools_smoke.rs` covers them through the daemon. ~keep
const KNOWN_TOOLS: [&str; 17] = [
    "code_outline",
    "code_symbols",
    "code_read",
    "code_grep",
    "code_search",
    "code_refs",
    "find_files",
    "docs_search",
    "git_log",
    "git_blame",
    "git_diff",
    "memory_save",
    "memory_search",
    "index_status",
    "index_rescan",
    "agent_threads",
    "agent_inbox",
];

/// Tools whose retrieval stack is feature-gated. Gated off, they answer with an `isError` result
/// naming the Cargo feature, which the harness records as skipped rather than failed.
const FEATURE_GATED_TOOLS: [&str; 4] = ["code_search", "docs_search", "memory_save", "memory_search"];

/// Arguments that page or bound a response without selecting what the tool does.
const PAGING_ARGS: [&str; 3] = ["limit", "cursor", "max_tokens"];

/// Build a tool call, rejecting any name that is not in [`KNOWN_TOOLS`].
///
/// Every canary call site is wrapped in `if let Ok(out) = …`, so an unknown tool name does not fail
/// the call — it skips the block, leaves the canary unset, and `unwrap_or(0)` then reports zero
/// hits. That reads as a capability regression rather than a stale test: an earlier surface change
/// left `find_implementations` and `find_callers` behind here, and the next harden run blamed the
/// engine for returning no `Future` implementations in tokio. Panicking on an unknown name turns
/// that silent misattribution into an immediate, obvious failure. ~keep
fn call_params(name: &'static str, args: &Value) -> CallToolRequestParams {
    assert!(
        KNOWN_TOOLS.contains(&name),
        "harden calls `{name}`, which is not a basemind tool — a call site was missed when the \
         surface changed; an unknown tool would silently read back as a zero canary"
    );
    let mut params = CallToolRequestParams::new(name);
    if let Some(obj) = args.as_object() {
        params = params.with_arguments(obj.clone());
    }
    params
}

/// Decode a canary response, or `None` when the tool reported a failure.
///
/// A verb tool reports a failure as an `isError` result, not a protocol error, so `if let Ok(..)`
/// alone would read an error message as a body with zero hits.
fn canary_body(result: &CallToolResult) -> Option<Value> {
    (result.is_error != Some(true)).then(|| decode_text(result))
}

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
    if raw.is_empty() {
        return Value::Null;
    }
    serde_json::from_str(&raw).unwrap_or(Value::String(raw))
}

/// The label a call is recorded under: `tool:mode` for a mode-dispatched tool, `tool:relation` for
/// `code_refs`, else the tool name plus the arguments that select its operation
/// (`git_log:path+symbol`). Without this every `git_log` call would collapse into one row and the
/// harness's per-operation latency table — the thing it exists to produce — would report a single
/// blended number for five different queries.
fn record_label(tool: &'static str, args: &Value) -> String {
    for selector in ["mode", "relation"] {
        if let Some(value) = args.get(selector).and_then(Value::as_str) {
            return format!("{tool}:{value}");
        }
    }
    let shape: Vec<&str> = args
        .as_object()
        .map(|object| {
            object
                .keys()
                .map(String::as_str)
                .filter(|key| !PAGING_ARGS.contains(key))
                .collect()
        })
        .unwrap_or_default();
    if shape.is_empty() {
        tool.to_string()
    } else {
        format!("{tool}:{}", shape.join("+"))
    }
}

/// Call a tool and record the result. Returns the decoded JSON body if successful
/// so per-tool drivers can chain assertions on it. Records the call either way.
async fn call(
    svc: &ServiceHandle,
    records: &mut Vec<ToolCallRecord>,
    tool: &'static str,
    args: Value,
) -> Option<Value> {
    let label = record_label(tool, &args);
    let started = Instant::now();
    let outcome = tokio::time::timeout(TOOL_TIMEOUT, svc.call_tool(call_params(tool, &args))).await;
    let elapsed = started.elapsed();
    match outcome {
        Err(_) => {
            records.push(ToolCallRecord {
                tool: label,
                ok: false,
                elapsed_ms: elapsed.as_millis(),
                elapsed_us: elapsed.as_micros(),
                detail: format!("timeout after {:?}", TOOL_TIMEOUT),
            });
            None
        }
        Ok(Err(e)) => {
            records.push(ToolCallRecord {
                tool: label,
                ok: false,
                elapsed_ms: elapsed.as_millis(),
                elapsed_us: elapsed.as_micros(),
                detail: format!("rmcp error: {e}"),
            });
            None
        }
        Ok(Ok(result)) => {
            let body = decode_text(&result);
            let is_error = result.is_error.unwrap_or(false);
            records.push(ToolCallRecord {
                tool: label,
                ok: !is_error,
                elapsed_ms: elapsed.as_millis(),
                elapsed_us: elapsed.as_micros(),
                detail: if is_error {
                    match &body {
                        Value::String(message) => format!("is_error: {message}"),
                        other => format!("is_error: {other}"),
                    }
                } else {
                    "ok".to_string()
                },
            });
            Some(body)
        }
    }
}

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
    /// 1-based line of `sample_symbol`'s definition: a name an impl block shares with its type
    /// needs it to pick one definition.
    sample_line: Option<u32>,
    /// non-empty when the file has at least one import with a resolved module
    sample_module: Option<String>,
}

fn run_scan(repo_root: &Path) -> ScanOutcome {
    let _ = basemind::lang::ensure_grammars().expect("grammar bootstrap");

    let mut config = match basemind::config::load(repo_root) {
        Ok(c) => c,
        Err(_) => basemind::config::default_for_root(repo_root),
    };
    config.documents.enabled = false;
    let mut store = basemind::store::Store::open(repo_root, basemind::store::VIEW_WORKING).expect("open store");
    let t0 = Instant::now();
    let report = basemind::scanner::scan(
        repo_root,
        &mut store,
        &config,
        basemind::scanner::ScanSource::WorkingTree,
        basemind::scanner::EmbedMode::Inline,
        basemind::retrieval_wiring::backend(),
    )
    .expect("scan");
    let elapsed = t0.elapsed();

    let sample_file = pick_sample(&store);

    ScanOutcome {
        elapsed,
        stats: report.stats,
        sample_file,
    }
}

fn pick_sample(store: &basemind::store::Store) -> Option<SampleFile> {
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
        if sample.is_none() && !l1.symbols.is_empty() && l1.symbols.iter().any(|s| !s.name.is_empty()) {
            let picked = l1.symbols.iter().find(|s| !s.name.is_empty());
            let sym = picked.map(|s| s.name.clone());
            let line = picked.map(|s| s.start_row + 1);
            let module = l1
                .imports
                .iter()
                .find_map(|i| i.module.clone())
                .or_else(|| fallback_module.clone());
            sample = Some(SampleFile {
                path: path.clone(),
                sample_symbol: sym,
                sample_line: line,
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

async fn drive_tools(svc: &ServiceHandle, sample: Option<&SampleFile>) -> Vec<ToolCallRecord> {
    let mut records: Vec<ToolCallRecord> = Vec::with_capacity(20);

    let _ = svc.list_tools(None).await;

    call(svc, &mut records, "index_status", json!({})).await;
    call(svc, &mut records, "agent_threads", json!({})).await;
    call(svc, &mut records, "agent_inbox", json!({ "peek": true })).await;
    call(svc, &mut records, "find_files", json!({ "limit": 50 })).await;
    call(svc, &mut records, "find_files", json!({ "query": "src", "limit": 50 })).await;
    call(
        svc,
        &mut records,
        "code_symbols",
        json!({ "name": "test", "limit": 50 }),
    )
    .await;
    call(svc, &mut records, "code_grep", json!({ "pattern": "fn ", "limit": 50 })).await;

    if let Some(sample) = sample {
        call(svc, &mut records, "code_outline", json!({ "path": &sample.path })).await;

        // With an explicit `column` the server never looks `name` up on the line, so any name does.
        call(
            svc,
            &mut records,
            "code_refs",
            json!({
                "name": sample.sample_symbol.as_deref().unwrap_or("_"),
                "relation": "definition",
                "path": &sample.path,
                "line": 1,
                "column": 1,
            }),
        )
        .await;

        if let Some(module) = &sample.sample_module {
            call(
                svc,
                &mut records,
                "code_refs",
                json!({ "name": module, "relation": "importers" }),
            )
            .await;
        }
        call(
            svc,
            &mut records,
            "code_refs",
            json!({ "relation": "importers", "path": &sample.path, "depth": 2 }),
        )
        .await;
        call(
            svc,
            &mut records,
            "code_refs",
            json!({ "relation": "cycles", "limit": 20 }),
        )
        .await;

        call(svc, &mut records, "git_diff", json!({})).await;
        call(svc, &mut records, "git_log", json!({ "limit": 20 })).await;
        call(
            svc,
            &mut records,
            "git_log",
            json!({ "path": &sample.path, "limit": 10 }),
        )
        .await;
        if let Some((directory, _)) = sample.path.to_str_lossy().rsplit_once('/') {
            call(
                svc,
                &mut records,
                "git_log",
                json!({ "path": format!("{directory}/"), "limit": 20 }),
            )
            .await;
        }
        call(svc, &mut records, "git_log", json!({ "query": "fix", "limit": 20 })).await;
        call(
            svc,
            &mut records,
            "git_diff",
            json!({ "path": &sample.path, "structural": true, "from": "HEAD" }),
        )
        .await;
        call(
            svc,
            &mut records,
            "git_diff",
            json!({ "path": &sample.path, "from": "HEAD~1", "to": "HEAD" }),
        )
        .await;
        call(svc, &mut records, "git_blame", json!({ "path": &sample.path })).await;

        if let Some(sym) = &sample.sample_symbol {
            call(
                svc,
                &mut records,
                "git_blame",
                json!({ "path": &sample.path, "symbol": sym, "line": sample.sample_line }),
            )
            .await;
            call(
                svc,
                &mut records,
                "git_log",
                json!({ "path": &sample.path, "symbol": sym, "line": sample.sample_line, "limit": 20 }),
            )
            .await;
            call(svc, &mut records, "code_refs", json!({ "name": sym, "limit": 100 })).await;
        }
    }

    call(
        svc,
        &mut records,
        "code_refs",
        json!({ "name": "Future", "relation": "implementations", "limit": 100 }),
    )
    .await;

    if let Some(sample) = sample
        && let Some(sym) = &sample.sample_symbol
    {
        call(
            svc,
            &mut records,
            "code_read",
            json!({ "path": &sample.path, "symbol": sym, "line": sample.sample_line }),
        )
        .await;
    }

    call(
        svc,
        &mut records,
        "memory_save",
        json!({ "key": "harden_probe", "value": "basemind harden probe" }),
    )
    .await;
    call(svc, &mut records, "memory_search", json!({ "key": "harden_probe" })).await;
    call(svc, &mut records, "memory_search", json!({})).await;
    call(svc, &mut records, "docs_search", json!({ "query": "code map scanner" })).await;
    call(
        svc,
        &mut records,
        "code_search",
        json!({ "query": "parse the file and extract symbols" }),
    )
    .await;
    call(
        svc,
        &mut records,
        "code_search",
        json!({ "query": "parse file extract symbols", "lane": "keyword" }),
    )
    .await;
    call(
        svc,
        &mut records,
        "code_search",
        json!({ "query": "spawn", "lane": "hybrid" }),
    )
    .await;

    records
}

/// Whether the spawned `basemind` binary is expected to have precise Python/Java resolution
/// (`code-intel-stack`) compiled in, derived from `BASEMIND_HARDEN_FEATURES` (which the harness
/// builds the binary with). Unset → the harness default is `full`, which includes it. Set to `""`
/// or a set without the code-intel stack → off, and the resolution canary is skipped rather than
/// false-failing. This keeps the canary a stable lower bound across the harness's feature matrix.
fn precise_resolution_expected() -> bool {
    match std::env::var("BASEMIND_HARDEN_FEATURES") {
        Err(_) => true,
        Ok(features) => {
            let features = features.trim();
            !features.is_empty()
                && features
                    .split([',', ' '])
                    .map(str::trim)
                    .any(|f| matches!(f, "full" | "code-intel" | "code-intel-stack"))
        }
    }
}

/// Whether the spawned binary resolves Rust imports into the file-level import graph: a
/// code-intel feature is compiled in (see [`precise_resolution_expected`] for the derivation).
fn import_graph_expected() -> bool {
    match std::env::var("BASEMIND_HARDEN_FEATURES") {
        Err(_) => true,
        Ok(features) => features
            .split([',', ' '])
            .map(str::trim)
            .any(|f| matches!(f, "full" | "code-intel" | "code-intel-js" | "code-intel-stack")),
    }
}

/// The agent tools, which only the daemon serves: see [`KNOWN_TOOLS`].
const DAEMON_SERVED_TOOLS: [&str; 2] = ["agent_threads", "agent_inbox"];

/// The refusal an agent tool answers on a server the daemon does not host (`agent_context`).
const DAEMON_REFUSAL: &str = "agent messaging is served by the basemind daemon";

/// Whether a failed call is an expected refusal rather than a defect, recorded as skipped: a tool
/// this build does not advertise, a feature-gated tool naming its Cargo feature, an agent tool
/// refused in-process, or an ambiguous name.
fn tolerated_failure(label: &str, detail: &str) -> bool {
    let tool = label.split(':').next().unwrap_or(label);
    let feature_gated = FEATURE_GATED_TOOLS.contains(&tool) && detail.contains("Cargo feature");
    let daemon_served = DAEMON_SERVED_TOOLS.contains(&tool) && detail.contains(DAEMON_REFUSAL);
    feature_gated || daemon_served || detail.contains("tool not found") || detail.contains("disambiguate")
}

/// Returns the human-readable failure summary if anything tripped; None on pass.
fn assert_passing(repo_name: &str, scan: &ScanOutcome, repo_record: &mut RepoRecord) -> Vec<String> {
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

    for r in &repo_record.tools {
        if !r.ok && !tolerated_failure(&r.tool, &r.detail) {
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

    if let Some(m) = &repo_record.git_history {
        if m.commits == 0 {
            failures.push("git-history index built zero commits".to_string());
        }
        if m.commits > 0
            && let Some(gh) = repo_record
                .canaries
                .get("stats_git_history_bytes")
                .and_then(Value::as_u64)
        {
            if gh == 0 {
                failures.push(format!(
                    "cache_stats: git_history_bytes is 0 despite a built git-history index ({} commits) — the index is uncounted",
                    m.commits
                ));
            }
            if let Some(total) = repo_record.canaries.get("stats_total_bytes").and_then(Value::as_u64)
                && total < gh
            {
                failures.push(format!(
                    "cache_stats: total_bytes ({total}) < git_history_bytes ({gh}) — git-history not rolled into the total"
                ));
            }
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

    match repo_name {
        "react" => {
            let hit_count = repo_record
                .canaries
                .get("useState_hits")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if hit_count == 0 {
                failures.push("react canary: code_symbols(\"useState\") returned 0 hits".into());
            }
        }
        name if name.ends_with("-shallow") => {
            let truncated = repo_record
                .canaries
                .get("any_truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !truncated {
                failures.push("shallow canary: no history-walking tool reported truncated=true".into());
            }
        }
        "tokio" => {
            let hits = repo_record
                .canaries
                .get("spawn_hits")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if hits < 50 {
                failures.push(format!(
                    "tokio canary: code_refs(\"spawn\") returned {hits} hits (expected ≥ 50)"
                ));
            }
            let find_files_hits = repo_record
                .canaries
                .get("find_files_src_hits")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if find_files_hits < 100 {
                failures.push(format!(
                    "tokio canary: find_files(\"src\") returned {find_files_hits} hits (expected ≥ 100)"
                ));
            }
            let grep_hits = repo_record
                .canaries
                .get("grep_fn_spawn_hits")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if grep_hits < 20 {
                failures.push(format!(
                    "tokio canary: code_grep(\"fn spawn\") returned {grep_hits} hits (expected ≥ 20)"
                ));
            }
            let future_hits = repo_record
                .canaries
                .get("future_impl_hits")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if future_hits < 20 {
                failures.push(format!(
                    "tokio canary: code_refs(\"Future\", implementations) returned {future_hits} hits (expected ≥ 20)"
                ));
            }
            let runtime_importers = repo_record
                .canaries
                .get("runtime_importers_depth2")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if import_graph_expected() && runtime_importers < 20 {
                failures.push(format!(
                    "tokio canary: code_refs(importers, path=tokio/src/runtime/mod.rs, depth=2) returned \
                     {runtime_importers} files (expected ≥ 20)"
                ));
            }
            if cfg!(feature = "code-search")
                && let Some(hits) = repo_record
                    .canaries
                    .get("search_code_spawn_hits")
                    .and_then(Value::as_u64)
                && hits < 1
            {
                failures.push(format!(
                    "tokio canary: code_search(\"spawn a task\") returned {hits} hits (expected ≥ 1)"
                ));
            }
        }
        "django" => {
            let hits = repo_record
                .canaries
                .get("get_hits")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if hits < 50 {
                failures.push(format!(
                    "django canary: code_refs(\"get\") returned {hits} hits (expected ≥ 50)"
                ));
            }
            let search_fixed = repo_record
                .canaries
                .get("search_fixed_commits")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if search_fixed < 20 {
                failures.push(format!(
                    "django canary: git_log(query=\"fixed\") returned {search_fixed} commits (expected ≥ 20)"
                ));
            }
            let query_commits = repo_record
                .canaries
                .get("query_py_commits")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if query_commits < 10 {
                failures.push(format!(
                    "django canary: git_log(path=\"django/db/models/query.py\") returned {query_commits} commits (expected ≥ 10)"
                ));
            }
            if let Some(author_hits) = repo_record.canaries.get("author_search_hits").and_then(Value::as_u64) {
                if author_hits < 1 {
                    let token = repo_record
                        .canaries
                        .get("author_search_token")
                        .and_then(Value::as_str)
                        .unwrap_or("?");
                    failures.push(format!(
                        "django canary: git_log(author={token:?}) found 0 commits for a deep-history author — full-depth author search regressed"
                    ));
                }
                if repo_record
                    .canaries
                    .get("author_search_consistent")
                    .and_then(Value::as_bool)
                    == Some(false)
                {
                    failures.push(
                        "django canary: git_log(author) returned a commit whose author does not match the query — author scope leaked".into(),
                    );
                }
            }
            if precise_resolution_expected() {
                let resolved_total = repo_record
                    .canaries
                    .get("force_str_resolved_total")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let cross_file = repo_record
                    .canaries
                    .get("force_str_cross_file_hits")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if resolved_total < 1 || cross_file < 1 {
                    failures.push(format!(
                        "django canary: code_refs(force_str, path) precise resolution regressed — \
                         resolved_total={resolved_total}, resolved cross-file hits={cross_file} \
                         (expected ≥ 1 of each)"
                    ));
                }
            }
        }
        _ => {}
    }

    failures
}

/// Run `git -C <repo> <args>` and return the first non-blank stdout line, trimmed. `None` on any
/// failure or empty output. Used by canaries that need a real-git oracle (e.g. sampling a
/// deep-history author). Best-effort: a canary that can't derive its input is simply not recorded,
/// so a git hiccup never turns into a spurious failure.
fn git_first_line(repo: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout)
        .ok()?
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
}

async fn capture_canaries(svc: &ServiceHandle, repo_name: &str, repo_root: &Path, record: &mut RepoRecord) {
    // Cache accounting has no MCP tool; this is the call `basemind cache stats` makes.
    let cache_dir = basemind::store::workspace_cache_dir(repo_root);
    if let Ok(Ok(stats)) = tokio::task::spawn_blocking(move || {
        basemind::store_gc::cache_stats(&basemind::data_root::DataRoot::current(), &cache_dir)
    })
    .await
    {
        record
            .canaries
            .insert("stats_git_history_bytes".into(), json!(stats.git_history_bytes));
        record
            .canaries
            .insert("stats_total_bytes".into(), json!(stats.total_bytes));
    }

    // Canaries count rows, so each asks for a budget far above its page instead of the default.
    const WHOLE_PAGE: u32 = 1_000_000;
    // A list (`code_symbols` results), or one grouped by file (`code_refs` hits:
    // `{"<path>": [row, ...]}`), or by file and enclosing symbol (`code_grep` hits:
    // `{"<path>": {"<symbol>": [row, ...]}}`), or by depth (`code_refs` importers' `by_depth`).
    fn rows_in(value: &Value) -> u64 {
        match value {
            Value::Array(rows) => rows.len() as u64,
            Value::Object(groups) => groups.values().map(rows_in).sum(),
            _ => 0,
        }
    }
    let count = |body: &Value, key: &str| body.get(key).map_or(0, rows_in);

    match repo_name {
        "react" => {
            let res = svc
                .call_tool(call_params(
                    "code_symbols",
                    &json!({ "name": "useState", "limit": 20, "max_tokens": WHOLE_PAGE }),
                ))
                .await;
            if let Ok(out) = res
                && let Some(body) = canary_body(&out)
            {
                record
                    .canaries
                    .insert("useState_hits".into(), json!(count(&body, "results")));
            }
        }
        name if name.ends_with("-shallow") => {
            let mut truncated = false;
            for (tool, args) in [
                ("git_log", json!({ "limit": 5 })),
                ("git_blame", json!({ "path": "README.md" })),
            ] {
                if let Ok(out) = svc.call_tool(call_params(tool, &args)).await
                    && let Some(body) = canary_body(&out)
                    && body.get("truncated").and_then(Value::as_bool) == Some(true)
                {
                    truncated = true;
                    break;
                }
            }
            record.canaries.insert("any_truncated".into(), json!(truncated));
        }
        "tokio" => {
            if let Ok(out) = svc
                .call_tool(call_params(
                    "code_refs",
                    &json!({ "name": "spawn", "limit": 200, "max_tokens": WHOLE_PAGE }),
                ))
                .await
                && let Some(body) = canary_body(&out)
            {
                record.canaries.insert("spawn_hits".into(), json!(count(&body, "hits")));
            }
            if let Ok(out) = svc
                .call_tool(call_params(
                    "find_files",
                    &json!({ "query": "src", "limit": 200, "max_tokens": WHOLE_PAGE }),
                ))
                .await
                && let Some(body) = canary_body(&out)
            {
                record
                    .canaries
                    .insert("find_files_src_hits".into(), json!(count(&body, "files")));
            }
            if let Ok(out) = svc
                .call_tool(call_params(
                    "code_refs",
                    &json!({
                        "name": "Future",
                        "relation": "implementations",
                        "limit": 200,
                        "max_tokens": WHOLE_PAGE,
                    }),
                ))
                .await
                && let Some(body) = canary_body(&out)
            {
                record
                    .canaries
                    .insert("future_impl_hits".into(), json!(count(&body, "hits")));
            }
            if let Ok(out) = svc
                .call_tool(call_params(
                    "code_grep",
                    &json!({ "pattern": "fn spawn", "limit": 200, "max_tokens": WHOLE_PAGE }),
                ))
                .await
                && let Some(body) = canary_body(&out)
            {
                // `total` is written only beside a `next_cursor`; a whole page is counted by its rows.
                let hits = body
                    .get("total")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| count(&body, "hits"));
                record.canaries.insert("grep_fn_spawn_hits".into(), json!(hits));
            }
            if let Ok(out) = svc
                .call_tool(call_params(
                    "code_refs",
                    &json!({ "relation": "importers", "path": "tokio/src/runtime/mod.rs", "depth": 2 }),
                ))
                .await
                && let Some(body) = canary_body(&out)
            {
                let files = body
                    .get("total")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| count(&body, "by_depth"));
                record.canaries.insert("runtime_importers_depth2".into(), json!(files));
            }
            #[cfg(feature = "code-search")]
            if let Ok(out) = svc
                .call_tool(call_params(
                    "code_search",
                    &json!({ "query": "spawn a task", "limit": 10, "max_tokens": WHOLE_PAGE }),
                ))
                .await
                && let Some(body) = canary_body(&out)
                && let Some(hits) = body.get("hits").and_then(Value::as_array)
                && !hits.is_empty()
            {
                record
                    .canaries
                    .insert("search_code_spawn_hits".into(), json!(hits.len() as u64));
            }
        }
        "django" => {
            if let Ok(out) = svc
                .call_tool(call_params(
                    "code_refs",
                    &json!({ "name": "get", "limit": 200, "max_tokens": WHOLE_PAGE }),
                ))
                .await
                && let Some(body) = canary_body(&out)
            {
                record.canaries.insert("get_hits".into(), json!(count(&body, "hits")));
            }
            if let Ok(out) = svc
                .call_tool(call_params(
                    "git_log",
                    &json!({ "path": "django/db/models/query.py", "limit": 100, "max_tokens": WHOLE_PAGE }),
                ))
                .await
                && let Some(body) = canary_body(&out)
            {
                record
                    .canaries
                    .insert("query_py_commits".into(), json!(count(&body, "commits")));
            }
            if let Ok(out) = svc
                .call_tool(call_params(
                    "git_log",
                    &json!({ "query": "fixed", "limit": 100, "max_tokens": WHOLE_PAGE }),
                ))
                .await
                && let Some(body) = canary_body(&out)
            {
                record
                    .canaries
                    .insert("search_fixed_commits".into(), json!(count(&body, "commits")));
            }
            if let Some(author) = git_first_line(repo_root, &["log", "--format=%an", "-1", "--skip=500"])
                && let Some(token) = author
                    .split_whitespace()
                    .find(|w| w.chars().all(char::is_alphabetic) && w.len() >= 3)
                && let Ok(out) = svc
                    .call_tool(call_params(
                        "git_log",
                        &json!({ "author": token, "limit": 100, "max_tokens": WHOLE_PAGE }),
                    ))
                    .await
                && let Some(body) = canary_body(&out)
            {
                let commits = body
                    .get("commits")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                // `author` matches the name or the email and a row names only the author, so the
                // check is git's: every commit whose author name or email holds the token, by full sha.
                let token_lc = token.to_lowercase();
                let by_token: Vec<String> = std::process::Command::new("git")
                    .arg("-C")
                    .arg(repo_root)
                    .args(["log", "--format=%H %an %ae"])
                    .output()
                    .ok()
                    .and_then(|out| String::from_utf8(out.stdout).ok())
                    .unwrap_or_default()
                    .lines()
                    .filter(|line| line.to_lowercase().contains(&token_lc))
                    .filter_map(|line| line.split_whitespace().next().map(str::to_string))
                    .collect();
                let consistent = commits.iter().all(|c| {
                    // A row starts with its sha.
                    let sha = c
                        .as_str()
                        .and_then(|row| row.split_once(' '))
                        .map_or("", |(sha, _)| sha);
                    !sha.is_empty() && by_token.iter().any(|full| full.starts_with(sha))
                });
                record
                    .canaries
                    .insert("author_search_hits".into(), json!(commits.len() as u64));
                record
                    .canaries
                    .insert("author_search_consistent".into(), json!(consistent));
                record.canaries.insert("author_search_token".into(), json!(token));
            }
            if let Ok(out) = svc
                .call_tool(call_params(
                    "code_refs",
                    &json!({
                        "name": "force_str",
                        "path": "django/utils/encoding.py",
                        "limit": 200,
                        "max_tokens": WHOLE_PAGE,
                    }),
                ))
                .await
                && let Some(body) = canary_body(&out)
            {
                let resolved_total = body.get("resolved_total").and_then(Value::as_u64).unwrap_or(0);
                // Cross-file AND proven: a hit is only evidence of precise resolution when the
                // resolver bound it to this definition (`imported`). Any other grade still names a
                // real call site of this name, so counting it here would let the name-only floor
                // alone keep the canary green after a resolution regression. A row leaves out the
                // page's `resolution_default`, itself unwritten when it is `name_only`.
                let default = body
                    .get("resolution_default")
                    .and_then(Value::as_str)
                    .or(Some("name_only"));
                let cross_file_hits = body
                    .get("hits")
                    .and_then(Value::as_object)
                    .map(|files| {
                        files
                            .iter()
                            .filter(|(path, _)| path.as_str() != "django/utils/encoding.py")
                            .flat_map(|(_, hits)| hits.as_array().into_iter().flatten())
                            .filter(|h| h.get("resolution").and_then(Value::as_str).or(default) == Some("imported"))
                            .count() as u64
                    })
                    .unwrap_or(0);
                record
                    .canaries
                    .insert("force_str_resolved_total".into(), json!(resolved_total));
                record
                    .canaries
                    .insert("force_str_cross_file_hits".into(), json!(cross_file_hits));
            }
        }
        _ => {}
    }
}

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

    let t0 = Instant::now();
    let outcome = builder::sync(&index, &repo, &bdir).ok()?;
    let build_ms = t0.elapsed().as_millis();
    let commits = index.commit_count();
    if commits == 0 {
        return None;
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
    let hot = counts.iter().max_by_key(|(_, n)| **n).map(|(p, _)| p.clone())?;
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
    let speedup = if indexed_us > 0.0 { live_us / indexed_us } else { 0.0 };
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
            // The entry vanished mid-walk (concurrent compaction/deletion) or is unreadable;
            // either way it is already gone, so it contributes no on-disk bytes.
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
    // Every index this run writes lands under the data home. Unset, that is the user's own cache;
    // set but not installed, the library would still use the platform default.
    assert!(
        std::env::var_os(basemind::store::DATA_HOME_ENV).is_some() && std::env::var_os("BASEMIND_COMMS_DIR").is_some(),
        "set BASEMIND_DATA_HOME and BASEMIND_COMMS_DIR to a throwaway directory (scripts/harden.sh does)"
    );
    basemind::cli::env_config::install_from_env().expect("install the engine configuration");

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

    let boot_start = Instant::now();
    let svc = connect(&repo).await;
    let server_boot_ms = boot_start.elapsed().as_millis();
    eprintln!("[harden] server boot: {}ms", server_boot_ms);

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

    if let Some(m) = &record.git_history {
        record.canaries.insert("gh_index_commits".to_string(), json!(m.commits));
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

    capture_canaries(&svc, &repo_name, &repo, &mut record).await;

    append_results(&record);

    let _ = svc.cancel().await;

    let failures = assert_passing(&repo_name, &scan, &mut record);
    if !failures.is_empty() {
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

#[test]
fn tolerates_only_the_expected_refusals() {
    let daemon_refusal = "is_error: comms backend: agent messaging is served by the basemind daemon, which hosts \
                          every session the `basemind serve` relay starts; this server runs without it";
    assert!(tolerated_failure("agent_threads", daemon_refusal));
    assert!(tolerated_failure("agent_inbox:peek", daemon_refusal));
    assert!(tolerated_failure("agent_inbox:peek", "rmcp error: tool not found"));
    assert!(tolerated_failure(
        "docs_search",
        "is_error: needs the `documents` Cargo feature"
    ));
    assert!(!tolerated_failure("code_grep", daemon_refusal));
    assert!(!tolerated_failure("agent_threads", "is_error: database is locked"));
    assert!(!tolerated_failure(
        "code_grep",
        "is_error: needs the `documents` Cargo feature"
    ));
}
