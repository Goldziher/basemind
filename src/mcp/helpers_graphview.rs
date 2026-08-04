//! Body of the `graph_export` tool (ADR-0005).
//!
//! [`build_graph_view`] assembles the canonical [`GraphView`] from the shared code-graph: it builds
//! the graph over the requested lanes, filters to the confidence floor, detects communities
//! (ADR-0004) and labels them, ranks nodes by centrality, caps the view, and describes each kept
//! node via the L1 cache. [`run_graph_export`] renders that payload into the requested text format
//! ([`graph_view::render`]). The graph is built on demand and discarded — no persisted state.

use ahash::{AHashMap, AHashSet};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::MapCache;
use super::codegraph::{self, BuildOpts, CodeEdge, EdgeKindSet};
use super::community::{self, CommunityAlgo};
use super::graph_view::{self, GraphFormat, GraphView, GraphViewEdge, GraphViewNode};
use super::helpers::{elapsed_us, json_result};
use super::helpers_community::label_for;
use super::helpers_traverse::{describe, kinds_from};
use super::shared_state::SharedReadStack;
use super::traverse::Adjacency;
use super::types::LifecycleNotice;
use super::types_graphview::{DisplayParams, DisplayResponse, GraphExportParams, GraphExportResponse};
use crate::index::IndexDb;

/// Sweep bound for community detection when tagging nodes; converges well inside this on real graphs.
const GRAPHVIEW_COMMUNITY_ITERS: u32 = 50;
const DEFAULT_MAX_NODES: u32 = 500;
const MAX_MAX_NODES: u32 = 2000;

/// Label each detected community from its members (most central first): dominant path prefix + most
/// central member (ADR-0004). Returns a label per dense community id.
fn label_communities(adj: &Adjacency, cache: &MapCache, partition: &community::Partition) -> Vec<String> {
    let mut by_comm: Vec<Vec<u32>> = vec![Vec::new(); partition.num_communities as usize];
    for (id, &c) in partition.community_of.iter().enumerate() {
        by_comm[c as usize].push(id as u32);
    }
    by_comm
        .iter_mut()
        .map(|members| {
            members.sort_by(|&a, &b| {
                partition.centrality[b as usize]
                    .cmp(&partition.centrality[a as usize])
                    .then(a.cmp(&b))
            });
            label_for(adj, cache, members)
        })
        .collect()
}

/// Keep the most central `max_nodes` node ids (centrality desc, id asc), returned in ascending id
/// order for stable output. Returns the kept original ids, a map from original id → dense output
/// index, and whether the view was capped.
fn select_nodes(partition: &community::Partition, n: usize, max_nodes: usize) -> (Vec<u32>, AHashMap<u32, u32>, bool) {
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_by(|&a, &b| {
        partition.centrality[b as usize]
            .cmp(&partition.centrality[a as usize])
            .then(a.cmp(&b))
    });
    let capped = order.len() > max_nodes;
    order.truncate(max_nodes);
    order.sort_unstable();
    let mut remap: AHashMap<u32, u32> = AHashMap::with_capacity(order.len());
    for (new_id, &orig) in order.iter().enumerate() {
        remap.insert(orig, new_id as u32);
    }
    (order, remap, capped)
}

/// Assemble the canonical graph-view payload from the shared code-graph. `max_nodes` keeps the most
/// central nodes (id-ascending among the kept set for readable output) and the edges induced among
/// them; `truncated` flags a capped or scan-truncated view.
// The build inputs (graph handle + lane/confidence/algo/focus/cap) are all independent scalars a
// single caller derives from params; bundling them into a struct would only add indirection.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_graph_view(
    shared: &SharedReadStack,
    idx: Option<&IndexDb>,
    cache: &MapCache,
    kinds: EdgeKindSet,
    min_conf: f32,
    algo: CommunityAlgo,
    focus: Option<String>,
    max_nodes: usize,
) -> Result<GraphView, McpError> {
    let built = shared.graph(
        idx,
        cache,
        &BuildOpts {
            kinds,
            focus,
            scan_cap: codegraph::CODEGRAPH_SCAN_CAP,
        },
    )?;
    let scan_truncated = built.truncated;
    // Only materialize a filtered edge set when a confidence floor is set; otherwise borrow the
    // memoized graph's edges directly so a cache hit stays a pure `Arc` clone.
    let filtered: Vec<CodeEdge>;
    let edges: &[CodeEdge] = if min_conf > 0.0 {
        filtered = built
            .edges
            .iter()
            .filter(|e| e.provenance.confidence() >= min_conf)
            .cloned()
            .collect();
        &filtered
    } else {
        &built.edges
    };
    let adj = Adjacency::build_from_edges(edges);
    // Community detection runs over the full graph before the `max_nodes` cut below (intentional):
    // dominant-cluster labels must reflect the whole partition, not an arbitrary pre-truncated slice.
    let partition = community::detect(&adj, algo, GRAPHVIEW_COMMUNITY_ITERS);
    let comm_label = label_communities(&adj, cache, &partition);
    let (order, remap, capped) = select_nodes(&partition, adj.node_count(), max_nodes);

    let nodes: Vec<GraphViewNode> = order
        .iter()
        .enumerate()
        .map(|(new_id, &orig)| {
            let described = describe(cache, adj.node(orig));
            let community = partition.community_of[orig as usize];
            GraphViewNode {
                id: new_id as u32,
                name: if described.name.is_empty() {
                    described.kind.clone()
                } else {
                    described.name
                },
                kind: described.kind,
                path: described.path,
                start_row: described.start_row,
                start_col: described.start_col,
                community,
                community_label: comm_label[community as usize].clone(),
                centrality: partition.centrality[orig as usize],
            }
        })
        .collect();

    let mut view_edges: Vec<GraphViewEdge> = Vec::new();
    for e in edges {
        let from = adj.id(&e.from).and_then(|i| remap.get(&i).copied());
        let to = adj.id(&e.to).and_then(|i| remap.get(&i).copied());
        let (Some(from), Some(to)) = (from, to) else {
            continue;
        };
        view_edges.push(GraphViewEdge {
            from,
            to,
            kind: e.kind.as_str().to_string(),
            provenance: e.provenance.as_str().to_string(),
            confidence: e.provenance.confidence(),
            weight: e.weight,
        });
    }

    Ok(GraphView {
        nodes,
        edges: view_edges,
        truncated: scan_truncated || capped,
    })
}

/// Sub-directory of the per-workspace cache that holds written exports (ADR-0005).
const EXPORTS_DIR: &str = "exports";
/// Hex prefix length of the content hash used in an export filename — 16 hex chars (64 bits) is
/// collision-safe for a per-workspace export directory while keeping the name short.
const EXPORT_HASH_PREFIX: usize = 16;
/// Soft byte budget for the `exports/` directory. Each distinct render (varying focus / max_nodes /
/// format) is a new content-addressed file that would otherwise accumulate forever; after writing,
/// the oldest files are evicted until the directory is back under this budget. `html`/`svg` at the
/// `max_nodes` cap can be hundreds of KB, so 64 MiB holds a generous working set without unbounded
/// growth. A self-contained bound, independent of the blob-store GC (which never sees this dir).
const EXPORTS_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// Write a rendered export to `<basemind_dir>/exports/graph-<content-hash>.<ext>` and return its
/// absolute path. The filename is content-addressed (a blake3 of the rendered bytes), so it is
/// deterministic, dedups identical renders, and carries no caller-supplied path component — there is
/// no traversal surface (CWE-22). An I/O failure is surfaced as an MCP internal error, not swallowed.
fn write_export(basemind_dir: &std::path::Path, format: GraphFormat, content: &str) -> Result<String, McpError> {
    let dir = basemind_dir.join(EXPORTS_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| McpError::internal_error(format!("create exports dir: {e}"), None))?;
    let hash = crate::hashing::hex(&crate::hashing::hash_bytes(content.as_bytes()));
    let name = format!("graph-{}.{}", &hash[..EXPORT_HASH_PREFIX], format.extension());
    let path = dir.join(name);
    crate::store_blob::write_bytes_atomic(path.clone(), content.as_bytes())
        .map_err(|e| McpError::internal_error(format!("write export: {e}"), None))?;
    prune_exports(&dir, EXPORTS_BUDGET_BYTES);
    Ok(path.to_string_lossy().into_owned())
}

/// Evict the oldest exports (by modified time) until the directory is at or under `budget` bytes,
/// always keeping the most recently written file. Best-effort: any metadata / remove error is
/// ignored — an over-budget cache is a soft concern, never a reason to fail the export the caller
/// just asked for.
fn prune_exports(dir: &std::path::Path, budget: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, u64, std::path::PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((meta.modified().ok()?, meta.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    if total <= budget {
        return;
    }
    // Oldest first; stop before the last (newest) entry so the just-written file always survives.
    files.sort_by_key(|(mtime, _, _)| *mtime);
    for (_, len, path) in files.iter().take(files.len().saturating_sub(1)) {
        if total <= budget {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(*len);
        }
    }
}

/// `graph_export` — render the code-graph into a chosen text format.
pub(super) fn run_graph_export(
    shared: &SharedReadStack,
    idx: Option<&IndexDb>,
    cache: &MapCache,
    basemind_dir: &std::path::Path,
    params: GraphExportParams,
    notice: Option<LifecycleNotice>,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    let format = GraphFormat::parse(&params.format).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "format must be node_link/dot/mermaid/graphml/cypher/html/svg, got {:?}",
                params.format
            ),
            None,
        )
    })?;
    let algo = CommunityAlgo::parse(&params.algorithm).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "algorithm must be label_propagation or louvain, got {:?}",
                params.algorithm
            ),
            None,
        )
    })?;
    let kinds = kinds_from(&params.edges, false)?;
    let min_conf = params.min_confidence.unwrap_or(0.0).clamp(0.0, 1.0);
    let max_nodes = params.max_nodes.unwrap_or(DEFAULT_MAX_NODES).min(MAX_MAX_NODES) as usize;

    let view = build_graph_view(shared, idx, cache, kinds, min_conf, algo, params.focus, max_nodes)?;

    let mut comms: AHashSet<u32> = AHashSet::new();
    for node in &view.nodes {
        comms.insert(node.community);
    }
    let node_count = view.nodes.len() as u32;
    let edge_count = view.edges.len() as u32;
    let community_count = comms.len() as u32;
    let truncated = view.truncated;
    let content = graph_view::render(&view, format);

    let output_path = if params.write {
        Some(write_export(basemind_dir, format, &content)?)
    } else {
        None
    };

    json_result(&GraphExportResponse {
        format: format.as_str().to_string(),
        content,
        node_count,
        edge_count,
        community_count,
        truncated,
        output_path,
        notice,
        elapsed_us: elapsed_us(started),
    })
}

/// Bound on how long `display` waits for the desktop opener to hand off before degrading to
/// export-only. A well-behaved launcher (`open`/`xdg-open`) returns in well under a second; a longer
/// wait means it is blocking (a MIME chooser, a slow desktop-portal round-trip), and the tool should
/// answer the caller promptly rather than hold the response open. The spawned blocking task is left
/// to finish on its own — a truly hung external process cannot be cancelled — but the caller is freed.
const OPENER_LAUNCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Outcome of attempting to open a rendered view in the human's default desktop viewer.
enum OpenOutcome {
    /// A viewer launcher was dispatched successfully.
    Launched,
    /// No viewer was launched; the string is a human-readable reason (surfaced as `detail`).
    Skipped(String),
}

/// Attempt to open `path` in the user's default desktop viewer, best-effort. Never fails the tool:
/// any inability to launch degrades to [`OpenOutcome::Skipped`] with a reason, so the caller always
/// still has the written export path. `path` is basemind's own content-addressed export file — no
/// caller-supplied component — passed as a single argument (never through a shell), so there is no
/// command-injection or path-traversal surface (CWE-22 / CWE-78).
fn open_in_viewer(path: &std::path::Path) -> OpenOutcome {
    #[cfg(target_os = "macos")]
    {
        spawn_opener("open", &[path.as_os_str()])
    }
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return OpenOutcome::Skipped("no GUI session (DISPLAY/WAYLAND_DISPLAY unset)".to_string());
        }
        spawn_opener("xdg-open", &[path.as_os_str()])
    }
    #[cfg(target_os = "windows")]
    {
        use std::ffi::OsStr;
        // `cmd /C start "" <path>`: the empty "" is start's window-title argument, so a quoted path
        // is never misparsed as the title.
        spawn_opener(
            "cmd",
            &[OsStr::new("/C"), OsStr::new("start"), OsStr::new(""), path.as_os_str()],
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = path;
        OpenOutcome::Skipped("no desktop opener for this platform".to_string())
    }
}

/// Run a viewer-launcher (`open` / `xdg-open` / `cmd start`) with every stdio stream detached to
/// null. Detaching is mandatory: the stdio MCP transport speaks the protocol over this process's
/// stdout, so a child inheriting stdout would corrupt the wire. Waiting reaps the launcher (which
/// exits promptly after handing the file to the GUI app — the app itself is reparented, not waited
/// on) and yields a real launch signal without accumulating zombies in the long-lived server.
fn spawn_opener(program: &str, args: &[&std::ffi::OsStr]) -> OpenOutcome {
    match std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) if status.success() => OpenOutcome::Launched,
        Ok(status) => OpenOutcome::Skipped(format!("{program} exited with {status}")),
        Err(e) => OpenOutcome::Skipped(format!("could not launch {program}: {e}")),
    }
}

/// `display` (ADR-0007) — the agent's human-facing output channel. Renders the code-graph to a
/// *visual* format, writes it to the export cache, and opens it in the human's default viewer,
/// degrading to export-only when no GUI session is available. The rendered bytes are not returned
/// inline; the product is the opened view plus the stable [`DisplayResponse::output_path`].
///
/// The rich native-window push (ADR-0006's basemind UI over the agent-layer IPC) is deliberately
/// deferred: with no UI window built yet there is no channel consumer, so `method` reserves
/// `"window"` for it and today lands on `"viewer"` or `"export"`.
///
/// `async` because the viewer launch is offloaded to the blocking pool: `open`/`xdg-open` can stall
/// unboundedly (a missing MIME association pops an interactive chooser, a desktop-portal round-trip
/// is slow), and that external-process wait must not pin an async worker thread of the shared daemon
/// runtime. The render/build portion stays synchronous inline, matching `run_graph_export`.
pub(super) async fn run_display(
    shared: &SharedReadStack,
    idx: Option<&IndexDb>,
    cache: &MapCache,
    basemind_dir: &std::path::Path,
    params: DisplayParams,
    notice: Option<LifecycleNotice>,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    let format = GraphFormat::parse(&params.format)
        .filter(|f| matches!(f, GraphFormat::Html | GraphFormat::Svg))
        .ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "display format must be html or svg — the graph data formats live on graph_export, got {:?}",
                    params.format
                ),
                None,
            )
        })?;
    let algo = CommunityAlgo::parse(&params.algorithm).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "algorithm must be label_propagation or louvain, got {:?}",
                params.algorithm
            ),
            None,
        )
    })?;
    let kinds = kinds_from(&params.edges, false)?;
    let min_conf = params.min_confidence.unwrap_or(0.0).clamp(0.0, 1.0);
    let max_nodes = params.max_nodes.unwrap_or(DEFAULT_MAX_NODES).min(MAX_MAX_NODES) as usize;

    let view = build_graph_view(shared, idx, cache, kinds, min_conf, algo, params.focus, max_nodes)?;

    let mut comms: AHashSet<u32> = AHashSet::new();
    for node in &view.nodes {
        comms.insert(node.community);
    }
    let node_count = view.nodes.len() as u32;
    let edge_count = view.edges.len() as u32;
    let community_count = comms.len() as u32;
    let truncated = view.truncated;
    let content = graph_view::render(&view, format);

    // Always persist: `display` needs a stable file to open (or hand back) whether or not a viewer
    // launches — the export cache is content-addressed and GC-bounded (see `write_export`).
    let output_path = write_export(basemind_dir, format, &content)?;

    let (displayed, method, detail) = if params.open {
        // Offload the external-process wait to Tokio's blocking pool (built for unbounded blocking
        // I/O) rather than the async worker pool the daemon runtime services other tool calls on, and
        // bound it: a launcher blocking on a chooser/portal degrades to export-only, not a hung call.
        let path = output_path.clone();
        let launch = tokio::task::spawn_blocking(move || open_in_viewer(std::path::Path::new(&path)));
        match tokio::time::timeout(OPENER_LAUNCH_TIMEOUT, launch).await {
            Ok(Ok(OpenOutcome::Launched)) => (true, "viewer", None),
            Ok(Ok(OpenOutcome::Skipped(why))) => (false, "export", Some(why)),
            Ok(Err(join_err)) => (false, "export", Some(format!("opener task failed: {join_err}"))),
            Err(_elapsed) => (
                false,
                "export",
                Some(format!(
                    "viewer did not hand off within {}s (opener may be blocking on a chooser or portal)",
                    OPENER_LAUNCH_TIMEOUT.as_secs()
                )),
            ),
        }
    } else {
        (false, "export", None)
    };

    json_result(&DisplayResponse {
        format: format.as_str().to_string(),
        output_path,
        displayed,
        method: method.to_string(),
        detail,
        node_count,
        edge_count,
        community_count,
        truncated,
        notice,
        elapsed_us: elapsed_us(started),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_opener_degrades_when_program_is_missing() {
        // The opener is best-effort and must never panic or fail the tool: a nonexistent launcher
        // binary degrades to Skipped with a reason, so `display` can still return the export path.
        match spawn_opener("basemind-no-such-opener-xyz", &[]) {
            OpenOutcome::Skipped(reason) => assert!(
                reason.contains("basemind-no-such-opener-xyz"),
                "reason names the missing program, got {reason:?}"
            ),
            OpenOutcome::Launched => panic!("a nonexistent opener must not report Launched"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn spawn_opener_reports_launched_on_a_clean_exit() {
        use std::ffi::OsStr;
        // ~keep A launcher that exits 0 is a successful hand-off. `sh -c 'echo ...'` also exercises
        // ~keep the stdio detachment: with stdout null'd the echo goes nowhere, so it cannot reach the
        // ~keep process stdout the stdio MCP transport speaks over. `.status()` waits, so the child is
        // ~keep reaped — no zombie is left behind.
        match spawn_opener("sh", &[OsStr::new("-c"), OsStr::new("echo detached-from-mcp-stdout")]) {
            OpenOutcome::Launched => {}
            OpenOutcome::Skipped(reason) => panic!("a clean exit must report Launched, got {reason:?}"),
        }
    }

    #[test]
    fn prune_exports_evicts_down_to_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Five equal-size files (1000 bytes each, total 5000). With a 2500-byte budget the eviction
        // count is deterministic regardless of mtime tie-breaking: 5000 → delete until ≤ 2500 leaves
        // exactly two files (2000 bytes). The just-written (last) file is always retained.
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("graph-{i}.svg")), vec![b'x'; 1000]).expect("write");
        }
        prune_exports(dir.path(), 2500);
        let remaining: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .collect();
        let total: u64 = remaining.iter().map(|e| e.metadata().unwrap().len()).sum();
        assert!(
            total <= 2500,
            "pruned under budget, got {total} bytes across {} files",
            remaining.len()
        );
        assert_eq!(remaining.len(), 2, "keeps exactly the files that fit the budget");
    }

    #[test]
    fn prune_exports_is_a_noop_under_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..3 {
            std::fs::write(dir.path().join(format!("graph-{i}.svg")), vec![b'x'; 100]).expect("write");
        }
        prune_exports(dir.path(), 64 * 1024);
        let count = std::fs::read_dir(dir.path()).expect("read_dir").count();
        assert_eq!(count, 3, "nothing evicted when the directory is under budget");
    }
}
