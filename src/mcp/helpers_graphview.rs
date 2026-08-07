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
use super::file_url::file_url;
use super::graph_view::{self, GraphFormat, GraphView, GraphViewEdge, GraphViewNode};
use super::helpers::{elapsed_us, json_result};
use super::helpers_community::label_for;
use super::helpers_traverse::{describe, kinds_from};
use super::shared_state::SharedReadStack;
use super::traverse::Adjacency;
use super::types::LifecycleNotice;
use super::types_graphview::{
    DisplayParams, DisplayResponse, GraphExportParams, GraphExportResponse, UiParams, UiResponse,
};
use crate::index::IndexDb;
use crate::path::RelPath;

/// Sweep bound for community detection when tagging nodes; converges well inside this on real graphs.
const GRAPHVIEW_COMMUNITY_ITERS: u32 = 50;
const DEFAULT_MAX_NODES: u32 = 500;
const MAX_MAX_NODES: u32 = 2000;
pub(super) const DEFAULT_MAX_EXPORT_EDGES: u32 = 200;
const DEFAULT_MAX_VISUAL_EDGES: u32 = 2000;
pub(super) const MAX_MAX_EDGES: u32 = 2000;

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
    focus: Option<RelPath>,
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

/// Rank edges by importance before applying the render cap and return the pre-cap total.
fn cap_graph_edges(view: &mut GraphView, max_edges: usize) -> u32 {
    view.edges.sort_by(|left, right| {
        right
            .weight
            .cmp(&left.weight)
            .then(left.from.cmp(&right.from))
            .then(left.to.cmp(&right.to))
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.provenance.cmp(&right.provenance))
    });
    let edge_count_total = view.edges.len() as u32;
    if view.edges.len() > max_edges {
        view.edges.truncate(max_edges);
        view.truncated = true;
    }
    edge_count_total
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
///
/// Returns a `PathBuf`, never a `String`: `basemind_dir` comes from `BASEMIND_DATA_HOME` / the
/// platform cache dir, so its bytes are not guaranteed to be UTF-8, and a lossy stringification here
/// would hand the caller a path that no longer opens the file it just wrote.
fn write_export(
    basemind_dir: &std::path::Path,
    format: GraphFormat,
    content: &str,
) -> Result<std::path::PathBuf, McpError> {
    let dir = basemind_dir.join(EXPORTS_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| McpError::internal_error(format!("create exports dir: {e}"), None))?;
    let hash = crate::hashing::hex(&crate::hashing::hash_bytes(content.as_bytes()));
    let name = format!("graph-{}.{}", &hash[..EXPORT_HASH_PREFIX], format.extension());
    let path = dir.join(name);
    crate::store_blob::write_bytes_atomic(path.clone(), content.as_bytes())
        .map_err(|e| McpError::internal_error(format!("write export: {e}"), None))?;
    prune_exports(&dir, EXPORTS_BUDGET_BYTES);
    Ok(path)
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
    let max_edges = params.max_edges.unwrap_or(DEFAULT_MAX_EXPORT_EDGES).min(MAX_MAX_EDGES) as usize;

    let mut view = build_graph_view(shared, idx, cache, kinds, min_conf, algo, params.focus, max_nodes)?;
    let edge_count_total = cap_graph_edges(&mut view, max_edges);

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
        Some(RelPath::from(write_export(basemind_dir, format, &content)?.as_path()))
    } else {
        None
    };

    json_result(&GraphExportResponse {
        format: format.as_str().to_string(),
        content,
        node_count,
        edge_count,
        edge_count_total,
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

/// Map an [`OpenOutcome`] to the `display` response triple `(displayed, method, detail)`. Extracted
/// as a pure function so the headline mapping — a launch MUST report `displayed=true`/`"viewer"`, a
/// skip MUST report `displayed=false`/`"export"` with its reason — is unit-testable without spawning a
/// real viewer (the tool's `open:true` path otherwise never runs in CI).
fn report_for_outcome(outcome: OpenOutcome) -> (bool, &'static str, Option<String>) {
    match outcome {
        OpenOutcome::Launched => (true, "viewer", None),
        OpenOutcome::Skipped(why) => (false, "export", Some(why)),
    }
}

/// Attempt to open `target` in the user's default desktop viewer, best-effort. Never fails the tool:
/// any inability to launch degrades to [`OpenOutcome::Skipped`] with a reason, so the caller always
/// still has the written export path (or the served URL). `target` is either a filesystem path to
/// basemind's own export (the `display` tool and the `ui` file fallback) or a loopback
/// `http://127.0.0.1:<port>/ui…` URL (the `ui` served path) — the platform openers accept both.
///
/// Injection surface: a path's *parent directories* come from the workspace path /
/// `BASEMIND_DATA_HOME`, which are environment-controlled and may contain shell or `cmd`
/// metacharacters; a served URL is basemind's own loopback address. Either way every launcher here is
/// a **real program invoked with `target` as a discrete argument — never a shell string and never
/// `cmd /C start`**, so the OS argument parser (`execvp` on Unix, `CommandLineToArgvW` for
/// `explorer.exe`) treats it literally: no command-injection or path-traversal surface (CWE-22 /
/// CWE-78). `start` is deliberately avoided — it is a `cmd.exe` builtin that re-parses `& ^ % ( )`
/// even inside quotes.
fn open_in_viewer(target: &std::ffi::OsStr) -> OpenOutcome {
    #[cfg(target_os = "macos")]
    {
        spawn_opener("open", &[target])
    }
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return OpenOutcome::Skipped("no GUI session (DISPLAY/WAYLAND_DISPLAY unset)".to_string());
        }
        spawn_opener("xdg-open", &[target])
    }
    #[cfg(target_os = "windows")]
    {
        // `explorer.exe <target>` opens the default handler as a real PE program (args parsed by
        // CommandLineToArgvW), so `& ^ % ( )` in the export's parent directories stay literal —
        // unlike `cmd /C start`, which re-interprets them. explorer's exit code is unreliable (it
        // often returns nonzero even on success), so a clean *spawn* is the launch signal; a dropped
        // Child handle is reaped by the OS on Windows (no zombie), and spawn returns at once so the
        // caller's timeout never fires on this path.
        match std::process::Command::new("explorer.exe")
            .arg(target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_child) => OpenOutcome::Launched,
            Err(e) => OpenOutcome::Skipped(format!("could not launch explorer.exe: {e}")),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = target;
        OpenOutcome::Skipped("no desktop opener for this platform".to_string())
    }
}

/// Run a Unix viewer-launcher (`open` / `xdg-open`) with every stdio stream detached to null.
/// Detaching is mandatory: the stdio MCP transport speaks the protocol over this process's stdout, so
/// a child inheriting stdout would corrupt the wire. `.status()` waits for and reaps the launcher — no
/// zombie is left in the long-lived server. The launcher usually hands off to the GUI app and exits
/// promptly, but `xdg-open`'s generic-desktop fallback can block until the viewer itself exits; the
/// caller's [`OPENER_LAUNCH_TIMEOUT`] frees the response in that case (the abandoned wait finishes on
/// the blocking pool). A non-success exit (e.g. `xdg-open` found no handler) degrades to Skipped.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
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
    let max_edges = params.max_edges.unwrap_or(DEFAULT_MAX_VISUAL_EDGES).min(MAX_MAX_EDGES) as usize;

    let mut view = build_graph_view(shared, idx, cache, kinds, min_conf, algo, params.focus, max_nodes)?;
    let edge_count_total = cap_graph_edges(&mut view, max_edges);

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
        let path = output_path.clone().into_os_string();
        let launch = tokio::task::spawn_blocking(move || open_in_viewer(&path));
        match tokio::time::timeout(OPENER_LAUNCH_TIMEOUT, launch).await {
            Ok(Ok(outcome)) => report_for_outcome(outcome),
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
        output_path: RelPath::from(output_path.as_path()),
        displayed,
        method: method.to_string(),
        detail,
        node_count,
        edge_count,
        edge_count_total,
        community_count,
        truncated,
        notice,
        elapsed_us: elapsed_us(started),
    })
}

/// The rendered UI payload plus the counts and format the `ui` surfaces report. Produced by
/// [`render_ui_parts`] and consumed by both [`run_ui`] (the tool) and the `/ui` HTTP route, so the two
/// surfaces render byte-identically from one code path.
pub(super) struct UiParts {
    pub content: String,
    pub format: GraphFormat,
    pub node_count: u32,
    pub edge_count: u32,
    pub edge_count_total: u32,
    pub community_count: u32,
    pub truncated: bool,
}

/// Parse the UI knobs (visual formats only, like `display`), build the canonical graph view, and
/// render it. The single producer shared by the `ui` tool and the `/ui` route; it neither awaits the
/// cache nor writes or opens anything — the caller owns those side effects.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_ui_parts(
    shared: &SharedReadStack,
    idx: Option<&IndexDb>,
    cache: &MapCache,
    format: &str,
    edges: &str,
    algorithm: &str,
    min_confidence: Option<f32>,
    max_nodes: Option<u32>,
    max_edges: Option<u32>,
    focus: Option<RelPath>,
) -> Result<UiParts, McpError> {
    let format = GraphFormat::parse(format)
        .filter(|f| matches!(f, GraphFormat::Html | GraphFormat::Svg))
        .ok_or_else(|| {
            McpError::invalid_params(
                format!("ui format must be html or svg — the graph data formats live on graph_export, got {format:?}"),
                None,
            )
        })?;
    let algo = CommunityAlgo::parse(algorithm).ok_or_else(|| {
        McpError::invalid_params(
            format!("algorithm must be label_propagation or louvain, got {algorithm:?}"),
            None,
        )
    })?;
    let kinds = kinds_from(edges, false)?;
    let min_conf = min_confidence.unwrap_or(0.0).clamp(0.0, 1.0);
    let max_nodes = max_nodes.unwrap_or(DEFAULT_MAX_NODES).min(MAX_MAX_NODES) as usize;
    let max_edges = max_edges.unwrap_or(DEFAULT_MAX_VISUAL_EDGES).min(MAX_MAX_EDGES) as usize;

    let mut view = build_graph_view(shared, idx, cache, kinds, min_conf, algo, focus, max_nodes)?;
    let edge_count_total = cap_graph_edges(&mut view, max_edges);
    let mut comms: AHashSet<u32> = AHashSet::new();
    for node in &view.nodes {
        comms.insert(node.community);
    }
    let content = graph_view::render(&view, format);
    Ok(UiParts {
        content,
        format,
        node_count: view.nodes.len() as u32,
        edge_count: view.edges.len() as u32,
        edge_count_total,
        community_count: comms.len() as u32,
        truncated: view.truncated,
    })
}

/// `ui` (ADR-0006) — open the interactive basemind UI for a human. Renders the graph, always writes
/// the self-contained export (so there is a durable `file://` artifact), and resolves a URL: a live
/// `http://<addr>/ui?root=…` page when a basemind daemon is serving HTTP for this machine, otherwise
/// the `file://` export. `open` (default) launches the URL in the human's default viewer, reusing the
/// same reactor-safe, best-effort launcher as `display`; `open:false` returns the URL without
/// launching (agents/tests). The durable product is the URL, so — unlike `display`, whose whole point
/// is the launch — the launch outcome is not reported.
pub(super) async fn run_ui(
    shared: &SharedReadStack,
    idx: Option<&IndexDb>,
    cache: &MapCache,
    basemind_dir: &std::path::Path,
    params: UiParams,
    notice: Option<LifecycleNotice>,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    let parts = render_ui_parts(
        shared,
        idx,
        cache,
        &params.format,
        &params.edges,
        &params.algorithm,
        params.min_confidence,
        params.max_nodes,
        params.max_edges,
        params.focus.clone(),
    )?;
    // Always persist so there is a stable artifact backing the `file://` fallback and any viewer open.
    let output_path = write_export(basemind_dir, parts.format, &parts.content)?;

    let (url, served, method, detail) = match resolve_served_ui_url(
        &shared.root,
        &params.format,
        &params.edges,
        &params.algorithm,
        params.min_confidence,
        params.max_nodes,
        params.max_edges,
        params.focus.as_ref(),
    )
    .await
    {
        Some(url) => (url, true, "http", None),
        None => (
            file_url(&output_path),
            false,
            "file",
            Some("no basemind daemon serving HTTP; using the written export file".to_string()),
        ),
    };

    if params.open {
        // Reactor-safe, best-effort launch (mirrors `run_display`): offload the external opener to the
        // blocking pool and bound it, so a launcher blocking on a chooser/portal never pins an async
        // worker or hangs the call. The URL is the durable product; the launch outcome is not reported.
        let target = url.clone();
        let launch = tokio::task::spawn_blocking(move || open_in_viewer(std::ffi::OsStr::new(&target)));
        let _ = tokio::time::timeout(OPENER_LAUNCH_TIMEOUT, launch).await;
    }

    json_result(&UiResponse {
        url,
        served,
        method: method.to_string(),
        output_path: RelPath::from(output_path.as_path()),
        detail,
        node_count: parts.node_count,
        edge_count: parts.edge_count,
        edge_count_total: parts.edge_count_total,
        community_count: parts.community_count,
        truncated: parts.truncated,
        notice,
        elapsed_us: elapsed_us(started),
    })
}

/// Resolve the live daemon-served UI URL for `root`, or `None` when no reachable HTTP daemon is
/// serving it (no comms build, no daemon running, or the port is not answering). On `None` the `ui`
/// tool falls back to the written `file://` export. The comms build delegates to
/// [`crate::comms::http_frontend::served_ui_url`], which reads the portfile and probes the port.
///
/// A non-UTF-8 `focus` also resolves to `None`: the served URL carries the prefix in a query string,
/// which cannot express those bytes, and serving a *wider* graph than the caller asked for would be
/// a silent wrong answer. The file export honours the raw bytes, so degrade to it.
#[allow(clippy::too_many_arguments)]
async fn resolve_served_ui_url(
    root: &std::path::Path,
    format: &str,
    edges: &str,
    algorithm: &str,
    min_confidence: Option<f32>,
    max_nodes: Option<u32>,
    max_edges: Option<u32>,
    focus: Option<&RelPath>,
) -> Option<String> {
    let focus: Option<&str> = match focus {
        Some(prefix) => Some(prefix.as_str()?),
        None => None,
    };
    #[cfg(all(feature = "comms", any(unix, windows)))]
    {
        crate::comms::http_frontend::served_ui_url(
            root,
            format,
            edges,
            algorithm,
            min_confidence,
            max_nodes,
            max_edges,
            focus,
        )
        .await
    }
    #[cfg(not(all(feature = "comms", any(unix, windows))))]
    {
        let _ = (
            root,
            format,
            edges,
            algorithm,
            min_confidence,
            max_nodes,
            max_edges,
            focus,
        );
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_cap_keeps_highest_weight_and_reports_pre_cap_total() {
        let edge = |from, weight| GraphViewEdge {
            from,
            to: from + 1,
            kind: "calls".to_string(),
            provenance: "inferred".to_string(),
            confidence: 0.7,
            weight,
        };
        let mut view = GraphView {
            nodes: Vec::new(),
            edges: vec![edge(0, 1), edge(1, 9), edge(2, 5)],
            truncated: false,
        };

        assert_eq!(cap_graph_edges(&mut view, 2), 3);
        assert_eq!(
            view.edges.iter().map(|edge| edge.weight).collect::<Vec<_>>(),
            vec![9, 5]
        );
        assert!(view.truncated);
    }

    #[test]
    fn report_for_outcome_maps_launch_and_skip_distinctly() {
        // The headline contract of `display{open:true}`: a launch is displayed=true/"viewer", a skip
        // is displayed=false/"export" carrying the reason. This guards against a swapped mapping that
        // the open:false smoke test (which never reaches this branch) cannot catch.
        let (displayed, method, detail) = report_for_outcome(OpenOutcome::Launched);
        assert!(displayed, "a launch reports displayed=true");
        assert_eq!(method, "viewer");
        assert_eq!(detail, None, "a launch carries no degrade reason");

        let (displayed, method, detail) = report_for_outcome(OpenOutcome::Skipped("no GUI session".to_string()));
        assert!(!displayed, "a skip reports displayed=false");
        assert_eq!(method, "export");
        assert_eq!(detail.as_deref(), Some("no GUI session"), "a skip surfaces its reason");
    }

    #[cfg(unix)]
    #[test]
    fn write_export_returns_a_path_that_opens_under_a_non_utf8_cache_dir() {
        use std::os::unix::ffi::OsStrExt;

        // `BASEMIND_DATA_HOME` (and the platform cache dir it falls back to) is not guaranteed to be
        // UTF-8. The caller opens the file by the path this returns, so a lossy conversion would hand
        // back a path naming nothing on disk.
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache_dir = tmp.path().join(std::ffi::OsStr::from_bytes(b"data-\xff"));
        if std::fs::create_dir(&cache_dir).is_err() {
            // APFS / HFS+ reject non-UTF-8 filenames outright, so on macOS this input cannot be
            // built at all; the assertion still runs on the filesystems that allow it (ext4, xfs).
            return;
        }

        let content = r#"{"nodes":[]}"#;
        let path = write_export(&cache_dir, GraphFormat::NodeLink, content).expect("write export");
        assert!(
            path.as_os_str().as_bytes().contains(&0xff),
            "the raw directory byte survives: {path:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("the returned path opens the file just written"),
            content
        );
    }

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
