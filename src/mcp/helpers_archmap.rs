//! Body of the `architecture_map` MCP tool + the reusable in-memory `RepoGraph`.
//!
//! `RepoGraph` builds a whole-repo, file-level directed call graph once from the L1
//! outline cache + the call sites (Fjall `calls_by_path`, or the in-RAM call index on a
//! read-only session). Edges are name→definition: file A links to file B when A contains
//! a call whose callee name is defined (as a function-like symbol) in B. This inherits
//! `call_graph`'s name-based imprecision (overloaded names produce a few spurious edges);
//! edge `weight` lets consumers discount thin edges.
//!
//! On top of the graph the tool computes deterministic structure signals — degree, a
//! fixed-iteration PageRank, and Tarjan SCCs (cycle clusters) — blends them with an
//! optional git-churn overlay, ranks, knee-cuts, and budgets the result. Outputs are
//! paths/lines/signatures + edges, never prose.
//!
//! The graph is rebuilt per call (bounded by [`ARCHMAP_EDGE_SCAN_CAP`]); memoizing it
//! against `cache_generation` is a future optimization, not needed for an occasional tool.

use ahash::{AHashMap, AHashSet};
use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;

use super::MapCache;
use super::budget::apply_budget;
use super::helpers::{elapsed_us, json_result, kind_to_str};
use super::helpers_calls::for_each_call_in_file;
use super::helpers_graph::is_function_like;
use super::kneedle::knee_cutoff;
use super::types_archmap::{ArchEdge, ArchNode, ArchitectureMapParams, ArchitectureMapResponse, CycleCluster};
use crate::index::IndexDb;
use crate::path::RelPath;

/// Hard upper bound on call sites scanned while building the graph. Bounds work on huge
/// repos; when hit the response is marked `truncated` with reason `"scan_cap"`.
const ARCHMAP_EDGE_SCAN_CAP: usize = 4_000_000;
const PAGERANK_ITERS: usize = 20;
const PAGERANK_DAMPING: f64 = 0.85;

/// A directed weighted graph in adjacency form. `out[i]` / `in_[i]` are sorted for
/// deterministic iteration (PageRank / Tarjan discovery order).
struct Graph {
    n: usize,
    out: Vec<Vec<(u32, u32)>>,
    in_: Vec<Vec<(u32, u32)>>,
}

impl Graph {
    fn empty(n: usize) -> Self {
        Graph {
            n,
            out: vec![Vec::new(); n],
            in_: vec![Vec::new(); n],
        }
    }

    /// Sort every adjacency list so all downstream traversals are order-stable.
    fn sort(&mut self) {
        for v in &mut self.out {
            v.sort_unstable();
        }
        for v in &mut self.in_ {
            v.sort_unstable();
        }
    }

    fn fan_in(&self, i: usize) -> u32 {
        self.in_[i].len() as u32
    }

    fn fan_out(&self, i: usize) -> u32 {
        self.out[i].len() as u32
    }

    /// Fixed-iteration PageRank over incoming edges. Deterministic: `1/n` init,
    /// id-ordered accumulation, fixed iteration count (no float convergence test, no
    /// RNG). Dangling nodes (no out-edges) leak their mass — acceptable, since only the
    /// relative ranking matters here.
    fn pagerank(&self) -> Vec<f32> {
        let n = self.n;
        if n == 0 {
            return Vec::new();
        }
        let base = (1.0 - PAGERANK_DAMPING) / n as f64;
        let outdeg: Vec<usize> = (0..n).map(|i| self.out[i].len()).collect();
        let mut rank = vec![1.0 / n as f64; n];
        let mut next = vec![0.0f64; n];
        for _ in 0..PAGERANK_ITERS {
            for x in next.iter_mut() {
                *x = base;
            }
            for s in 0..n {
                if outdeg[s] == 0 {
                    continue;
                }
                let share = PAGERANK_DAMPING * rank[s] / outdeg[s] as f64;
                for &(dst, _w) in &self.out[s] {
                    next[dst as usize] += share;
                }
            }
            std::mem::swap(&mut rank, &mut next);
        }
        rank.into_iter().map(|r| r as f32).collect()
    }

    /// Iterative Tarjan SCC. Returns a component id per node. Iterative (explicit work
    /// stack) so deep graphs don't blow the call stack. Discovery order is deterministic
    /// given the sorted adjacency + ascending node iteration.
    fn tarjan_scc(&self) -> Vec<u32> {
        let n = self.n;
        let mut index = vec![u32::MAX; n];
        let mut low = vec![0u32; n];
        let mut on_stack = vec![false; n];
        let mut comp = vec![u32::MAX; n];
        let mut stack: Vec<u32> = Vec::new();
        let mut idx_counter: u32 = 0;
        let mut comp_counter: u32 = 0;

        for start in 0..n {
            if index[start] != u32::MAX {
                continue;
            }
            let mut work: Vec<(u32, usize)> = vec![(start as u32, 0)];
            while let Some(&(v, pi)) = work.last() {
                let vu = v as usize;
                if pi == 0 {
                    index[vu] = idx_counter;
                    low[vu] = idx_counter;
                    idx_counter += 1;
                    stack.push(v);
                    on_stack[vu] = true;
                }
                if pi < self.out[vu].len() {
                    work.last_mut().unwrap().1 += 1;
                    let w = self.out[vu][pi].0;
                    let wu = w as usize;
                    if index[wu] == u32::MAX {
                        work.push((w, 0));
                    } else if on_stack[wu] {
                        low[vu] = low[vu].min(index[wu]);
                    }
                } else {
                    if low[vu] == index[vu] {
                        loop {
                            let w = stack.pop().unwrap();
                            on_stack[w as usize] = false;
                            comp[w as usize] = comp_counter;
                            if w == v {
                                break;
                            }
                        }
                        comp_counter += 1;
                    }
                    work.pop();
                    if let Some(&(parent, _)) = work.last() {
                        low[parent as usize] = low[parent as usize].min(low[vu]);
                    }
                }
            }
        }
        comp
    }
}

/// Whole-repo file-level call graph plus a per-callee-name fan-in table (reused by the
/// symbol tier, and by the Session-2 coverage tool).
pub(crate) struct RepoGraph {
    /// node id → file path (ascending — mirrors `MapCache::by_path` iteration).
    files: Vec<RelPath>,
    graph: Graph,
    /// Callee name → total call-site count across the repo (name-based fan-in).
    callee_counts: AHashMap<String, u32>,
    /// Callee name → number of files that define a function-like symbol with that name.
    /// The specificity denominator for the symbol tier: a name defined in 200 files is not
    /// one hub, so raw name-based fan-in is divided by this to demote ubiquitous names
    /// (`new` / `from` / `default`) that would otherwise dominate a repo-wide ranking.
    def_counts: AHashMap<String, u32>,
    truncated: bool,
    truncation_reason: Option<&'static str>,
}

impl RepoGraph {
    /// Build the graph from the L1 cache + call sites. `idx = Some` prefix-scans the
    /// Fjall `calls_by_path` keyspace per file; `idx = None` reads the in-RAM call index
    /// (read-only session). Bounded by `edge_scan_cap` total call sites.
    pub(crate) fn build(idx: Option<&IndexDb>, cache: &MapCache, edge_scan_cap: usize) -> Result<Self, McpError> {
        let files: Vec<RelPath> = cache.by_path.keys().cloned().collect();
        let mut id_of: AHashMap<RelPath, u32> = AHashMap::with_capacity(files.len());
        for (i, p) in files.iter().enumerate() {
            id_of.insert(p.clone(), i as u32);
        }

        let mut def_files_by_name: AHashMap<String, Vec<u32>> = AHashMap::new();
        for (path, l1) in &cache.by_path {
            let fid = id_of[path];
            for sym in &l1.symbols {
                if is_function_like(sym.kind) {
                    def_files_by_name.entry(sym.name.clone()).or_default().push(fid);
                }
            }
        }

        let mut edges: AHashMap<(u32, u32), u32> = AHashMap::new();
        let mut callee_counts: AHashMap<String, u32> = AHashMap::new();
        let mut scanned = 0usize;
        let mut truncated = false;
        let mut truncation_reason: Option<&'static str> = None;

        for path in cache.by_path.keys() {
            let src = id_of[path];
            let mut cap_hit = false;
            for_each_call_in_file(idx, cache, path, |callee, _start_byte| {
                scanned += 1;
                if scanned > edge_scan_cap {
                    cap_hit = true;
                    return false;
                }
                if let Some(count) = callee_counts.get_mut(callee) {
                    *count += 1;
                } else {
                    callee_counts.insert(callee.to_string(), 1);
                }
                if let Some(defs) = def_files_by_name.get(callee) {
                    for &dst in defs {
                        if dst != src {
                            *edges.entry((src, dst)).or_default() += 1;
                        }
                    }
                }
                true
            })?;
            if cap_hit {
                truncated = true;
                truncation_reason = Some("scan_cap");
                break;
            }
        }

        let def_counts: AHashMap<String, u32> = def_files_by_name
            .into_iter()
            .map(|(name, files)| (name, files.len() as u32))
            .collect();

        let mut graph = Graph::empty(files.len());
        for (&(s, d), &w) in &edges {
            graph.out[s as usize].push((d, w));
            graph.in_[d as usize].push((s, w));
        }
        graph.sort();

        Ok(RepoGraph {
            files,
            graph,
            callee_counts,
            def_counts,
            truncated,
            truncation_reason,
        })
    }
}

/// Body of the `architecture_map` tool. `churn` (commits-touching per file) is `None`
/// when the overlay is disabled or there's no git repo.
pub(crate) fn run_architecture_map(
    idx: Option<&IndexDb>,
    cache: &MapCache,
    churn: Option<&AHashMap<RelPath, u32>>,
    params: ArchitectureMapParams,
    notice: Option<super::types::LifecycleNotice>,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    let max_nodes = params.max_nodes.unwrap_or(60).min(300) as usize;
    let max_edges = params.max_edges.unwrap_or(200).min(2000) as usize;
    let depth = params.depth.unwrap_or(2).max(1) as usize;
    let focus = params.focus.as_deref();

    let rg = RepoGraph::build(idx, cache, ARCHMAP_EDGE_SCAN_CAP)?;

    match params.granularity.as_str() {
        "module" => run_tier_grouped(
            &rg,
            churn,
            focus,
            Some(depth),
            &params,
            max_nodes,
            max_edges,
            notice,
            started,
        ),
        "file" => run_tier_grouped(&rg, churn, focus, None, &params, max_nodes, max_edges, notice, started),
        "symbol" => run_tier_symbol(
            &rg, cache, idx, churn, focus, &params, max_nodes, max_edges, notice, started,
        ),
        other => Err(McpError::invalid_params(
            format!("granularity must be \"module\", \"file\", or \"symbol\", got {other:?}"),
            None,
        )),
    }
}

/// Min-max normalize to `[0, 1]`; a flat input maps to all-zero (that signal doesn't
/// discriminate, so it contributes nothing to the blend).
fn minmax_norm(vals: &[f64]) -> Vec<f64> {
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for &v in vals {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    let span = hi - lo;
    if span <= f64::EPSILON {
        return vec![0.0; vals.len()];
    }
    vals.iter().map(|&v| (v - lo) / span).collect()
}

/// Directory label = the first `depth` path components (the file name dropped). A
/// top-level file with no directory maps to `"."`.
fn dir_label(path: &str, depth: usize) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 1 {
        return ".".to_string();
    }
    let dirs = &parts[..parts.len() - 1];
    let take = dirs.len().min(depth);
    dirs[..take].join("/")
}

#[allow(clippy::too_many_arguments)]
fn run_tier_grouped(
    rg: &RepoGraph,
    churn: Option<&AHashMap<RelPath, u32>>,
    focus: Option<&str>,
    rollup: Option<usize>,
    params: &ArchitectureMapParams,
    max_nodes: usize,
    max_edges: usize,
    notice: Option<super::types::LifecycleNotice>,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    let mut group_of: Vec<Option<u32>> = vec![None; rg.files.len()];
    let mut label_to_gid: AHashMap<String, u32> = AHashMap::new();
    let mut labels: Vec<String> = Vec::new();
    let mut group_paths: Vec<Option<RelPath>> = Vec::new();
    let mut group_churn: Vec<u32> = Vec::new();

    for (fid, path) in rg.files.iter().enumerate() {
        let ps = path.as_str().unwrap_or("");
        if let Some(fx) = focus
            && !ps.starts_with(fx)
        {
            continue;
        }
        let (label, gpath) = match rollup {
            Some(d) => (dir_label(ps, d), None),
            None => (ps.to_string(), Some(path.clone())),
        };
        let gid = match label_to_gid.get(&label) {
            Some(&g) => g,
            None => {
                let g = labels.len() as u32;
                label_to_gid.insert(label.clone(), g);
                labels.push(label);
                group_paths.push(gpath);
                group_churn.push(0);
                g
            }
        };
        group_of[fid] = Some(gid);
        if let Some(ch) = churn {
            let c = ch.get(path).copied().unwrap_or(0);
            let slot = &mut group_churn[gid as usize];
            *slot = slot.saturating_add(c);
        }
    }

    let ngroups = labels.len();

    let mut gedges: AHashMap<(u32, u32), u32> = AHashMap::new();
    for s in 0..rg.files.len() {
        let Some(gs) = group_of[s] else { continue };
        for &(d, w) in &rg.graph.out[s] {
            let Some(gd) = group_of[d as usize] else { continue };
            if gs != gd {
                *gedges.entry((gs, gd)).or_default() += w;
            }
        }
    }

    let mut g = Graph::empty(ngroups);
    for (&(s, d), &w) in &gedges {
        g.out[s as usize].push((d, w));
        g.in_[d as usize].push((s, w));
    }
    g.sort();
    let pr = g.pagerank();
    let comp = g.tarjan_scc();

    let deg: Vec<f64> = (0..ngroups).map(|i| (g.fan_in(i) + g.fan_out(i)) as f64).collect();
    let prv: Vec<f64> = pr.iter().map(|&r| r as f64).collect();
    let chv: Vec<f64> = group_churn.iter().map(|&c| c as f64).collect();
    let degn = minmax_norm(&deg);
    let prn = minmax_norm(&prv);
    let chn = minmax_norm(&chv);
    let (w_pr, w_deg, w_churn) = weights(churn.is_some());
    let scores: Vec<f64> = (0..ngroups)
        .map(|i| w_pr * prn[i] + w_deg * degn[i] + w_churn * chn[i])
        .collect();

    let mut order: Vec<usize> = (0..ngroups).collect();
    order.sort_by(|&a, &b| {
        scores[b]
            .partial_cmp(&scores[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(labels[a].cmp(&labels[b]))
    });
    let ranked_scores: Vec<f64> = order.iter().map(|&i| scores[i]).collect();
    let cut = knee_cutoff(&ranked_scores).min(max_nodes).min(ngroups);
    let survivor_gids: Vec<u32> = order[..cut].iter().map(|&i| i as u32).collect();

    let prelim: Vec<ArchNode> = survivor_gids
        .iter()
        .enumerate()
        .map(|(local, &gid)| {
            let gi = gid as usize;
            ArchNode {
                id: local as u32,
                label: labels[gi].clone(),
                path: group_paths[gi].clone(),
                name: None,
                kind: None,
                start_row: None,
                signature: None,
                fan_in: g.fan_in(gi),
                fan_out: g.fan_out(gi),
                pagerank: Some(prn[gi] as f32),
                commits_touching: churn.map(|_| group_churn[gi]),
                score: scores[gi] as f32,
                scc_id: None,
            }
        })
        .collect();
    let budgeted = apply_budget(prelim, params.max_tokens);
    let mut nodes = budgeted.items;
    let kept = nodes.len();

    let mut local_of: AHashMap<u32, u32> = AHashMap::with_capacity(kept);
    for (local, &gid) in survivor_gids[..kept].iter().enumerate() {
        local_of.insert(gid, local as u32);
    }

    let (cycles, scc_of_local) = build_cycles(&comp, &survivor_gids[..kept], &gedges, &local_of);
    for node in &mut nodes {
        if let Some(&sid) = scc_of_local.get(&node.id) {
            node.scc_id = Some(sid);
        }
    }

    let edges = emit_edges(&gedges, &local_of, max_edges);

    json_result(&ArchitectureMapResponse {
        granularity: params.granularity.clone(),
        node_count_total: ngroups as u32,
        edge_count_total: gedges.len() as u32,
        nodes,
        edges,
        cycles,
        truncated: rg.truncated,
        truncation_reason: rg.truncation_reason,
        budgeted: budgeted.budgeted,
        notice,
        elapsed_us: elapsed_us(started),
    })
}

/// Ranking weights: (pagerank, degree, churn). Churn drops out (renormalized) when the
/// overlay is absent.
fn weights(has_churn: bool) -> (f64, f64, f64) {
    if has_churn {
        (0.5, 0.3, 0.2)
    } else {
        (0.625, 0.375, 0.0)
    }
}

/// Cluster kept nodes by shared SCC component; emit clusters with >1 member. Returns the
/// clusters plus a `local id → scc_id` map for stamping nodes.
fn build_cycles(
    comp: &[u32],
    kept_gids: &[u32],
    gedges: &AHashMap<(u32, u32), u32>,
    local_of: &AHashMap<u32, u32>,
) -> (Vec<CycleCluster>, AHashMap<u32, u32>) {
    let mut by_comp: AHashMap<u32, Vec<u32>> = AHashMap::new();
    for &gid in kept_gids {
        let c = comp[gid as usize];
        by_comp.entry(c).or_default().push(local_of[&gid]);
    }
    let mut clusters: Vec<(u32, Vec<u32>)> = by_comp.into_iter().filter(|(_, m)| m.len() > 1).collect();
    for (_, m) in &mut clusters {
        m.sort_unstable();
    }
    clusters.sort_by_key(|(_, m)| m[0]);

    let mut scc_of_local: AHashMap<u32, u32> = AHashMap::new();
    let mut out: Vec<CycleCluster> = Vec::with_capacity(clusters.len());
    for (scc_id, (comp_id, members)) in clusters.into_iter().enumerate() {
        for &loc in &members {
            scc_of_local.insert(loc, scc_id as u32);
        }
        let member_gids: AHashSet<u32> = kept_gids
            .iter()
            .copied()
            .filter(|g| comp[*g as usize] == comp_id)
            .collect();
        let internal = gedges
            .keys()
            .filter(|(s, d)| member_gids.contains(s) && member_gids.contains(d))
            .count() as u32;
        out.push(CycleCluster {
            scc_id: scc_id as u32,
            members,
            internal_edges: internal,
        });
    }
    (out, scc_of_local)
}

/// Emit inter-group edges whose endpoints both survived, heaviest first, capped.
fn emit_edges(gedges: &AHashMap<(u32, u32), u32>, local_of: &AHashMap<u32, u32>, max_edges: usize) -> Vec<ArchEdge> {
    let mut edges: Vec<ArchEdge> = gedges
        .iter()
        .filter_map(|(&(s, d), &w)| {
            let from = *local_of.get(&s)?;
            let to = *local_of.get(&d)?;
            Some(ArchEdge {
                from,
                to,
                weight: w,
                kind: "calls".to_string(),
            })
        })
        .collect();
    edges.sort_by(|a, b| b.weight.cmp(&a.weight).then(a.from.cmp(&b.from)).then(a.to.cmp(&b.to)));
    edges.truncate(max_edges);
    edges
}

struct SymCand {
    path: RelPath,
    name: String,
    kind: &'static str,
    start_row: u32,
    start_byte: u32,
    end_byte: u32,
    signature: Option<String>,
    /// Raw name-based call count (reported verbatim — the honest count).
    fan_in: u32,
    /// Specificity-weighted hub-ness = `fan_in / def_count`. The ranking signal: it demotes
    /// ubiquitous names whose fan-in is spread across many definitions.
    hub: f64,
    churn: u32,
}

#[allow(clippy::too_many_arguments)]
fn run_tier_symbol(
    rg: &RepoGraph,
    cache: &MapCache,
    idx: Option<&IndexDb>,
    churn: Option<&AHashMap<RelPath, u32>>,
    focus: Option<&str>,
    params: &ArchitectureMapParams,
    max_nodes: usize,
    max_edges: usize,
    notice: Option<super::types::LifecycleNotice>,
    started: std::time::Instant,
) -> Result<CallToolResult, McpError> {
    let mut cands: Vec<SymCand> = Vec::new();
    for (path, l1) in &cache.by_path {
        let ps = path.as_str().unwrap_or("");
        if let Some(fx) = focus
            && !ps.starts_with(fx)
        {
            continue;
        }
        let c = churn.and_then(|ch| ch.get(path)).copied().unwrap_or(0);
        for sym in &l1.symbols {
            if !is_function_like(sym.kind) {
                continue;
            }
            let fan_in = rg.callee_counts.get(&sym.name).copied().unwrap_or(0);
            let def_count = rg.def_counts.get(&sym.name).copied().unwrap_or(1).max(1);
            cands.push(SymCand {
                path: path.clone(),
                name: sym.name.clone(),
                kind: kind_to_str(sym.kind),
                start_row: sym.start_row,
                start_byte: sym.start_byte,
                end_byte: sym.end_byte,
                signature: sym.signature.clone(),
                fan_in,
                hub: fan_in as f64 / def_count as f64,
                churn: c,
            });
        }
    }
    let node_count_total = cands.len() as u32;

    cands.sort_by(|a, b| {
        b.hub
            .partial_cmp(&a.hub)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.path.cmp(&b.path))
            .then(a.name.cmp(&b.name))
            .then(a.start_row.cmp(&b.start_row))
    });
    let hub_curve: Vec<f64> = cands.iter().map(|c| c.hub).collect();
    let cut = knee_cutoff(&hub_curve).min(max_nodes).min(cands.len());
    cands.truncate(cut);
    let survivors = cands;

    let mut by_file: AHashMap<RelPath, Vec<u32>> = AHashMap::new();
    let mut name_to_locals: AHashMap<&str, Vec<u32>> = AHashMap::new();
    for (loc, s) in survivors.iter().enumerate() {
        by_file.entry(s.path.clone()).or_default().push(loc as u32);
        name_to_locals.entry(s.name.as_str()).or_default().push(loc as u32);
    }
    let mut fan_out_sets: Vec<AHashSet<String>> = vec![AHashSet::new(); survivors.len()];
    let mut edge_map: AHashMap<(u32, u32), u32> = AHashMap::new();
    for (file, locals) in &by_file {
        for_each_call_in_file(idx, cache, file, |callee, start_byte| {
            for &loc in locals {
                let s = &survivors[loc as usize];
                if s.start_byte <= start_byte && start_byte < s.end_byte {
                    let fo = &mut fan_out_sets[loc as usize];
                    if !fo.contains(callee) {
                        fo.insert(callee.to_string());
                    }
                    if let Some(targets) = name_to_locals.get(callee) {
                        for &t in targets {
                            if t != loc {
                                *edge_map.entry((loc, t)).or_default() += 1;
                            }
                        }
                    }
                }
            }
            true
        })?;
    }
    let fan_out: Vec<u32> = fan_out_sets.iter().map(|s| s.len() as u32).collect();

    let hubv: Vec<f64> = survivors.iter().map(|c| c.hub).collect();
    let hubn = minmax_norm(&hubv);

    let prelim: Vec<ArchNode> = survivors
        .iter()
        .enumerate()
        .map(|(loc, s)| ArchNode {
            id: loc as u32,
            label: s.path.as_str().unwrap_or("").to_string(),
            path: Some(s.path.clone()),
            name: Some(s.name.clone()),
            kind: Some(s.kind.to_string()),
            start_row: Some(s.start_row),
            signature: s.signature.clone(),
            fan_in: s.fan_in,
            fan_out: fan_out[loc],
            pagerank: None,
            commits_touching: churn.map(|_| s.churn),
            score: hubn[loc] as f32,
            scc_id: None,
        })
        .collect();
    let budgeted = apply_budget(prelim, params.max_tokens);
    let nodes = budgeted.items;
    let kept = nodes.len();

    let mut edges: Vec<ArchEdge> = edge_map
        .iter()
        .filter(|((from, to), _)| (*from as usize) < kept && (*to as usize) < kept)
        .map(|(&(from, to), &w)| ArchEdge {
            from,
            to,
            weight: w,
            kind: "calls".to_string(),
        })
        .collect();
    edges.sort_by(|a, b| b.weight.cmp(&a.weight).then(a.from.cmp(&b.from)).then(a.to.cmp(&b.to)));
    let edge_count_total = edges.len() as u32;
    edges.truncate(max_edges);

    json_result(&ArchitectureMapResponse {
        granularity: params.granularity.clone(),
        node_count_total,
        edge_count_total,
        nodes,
        edges,
        cycles: Vec::new(),
        truncated: rg.truncated,
        truncation_reason: rg.truncation_reason,
        budgeted: budgeted.budgeted,
        notice,
        elapsed_us: elapsed_us(started),
    })
}

/// Commits-touching per file over the last `window` commits — the churn overlay. Mirrors
/// the aggregation in `hot_files` (counts only). Returns `None`-worthy errors as `Err`;
/// the caller degrades to no overlay.
pub(crate) fn churn_commit_counts(state: &super::ServerState, window: u32) -> Result<AHashMap<RelPath, u32>, McpError> {
    let repo = super::helpers::require_git_repo(state)?;
    let head = super::helpers::head_sha(repo)?;
    let commits: Vec<crate::git::CommitInfo> = match super::helpers::git_history_if_fresh(state, &head) {
        Some(index) => index.window_commits(window as usize),
        None => state
            .git_cache
            .log(repo, &head, None, window, true)
            .map_err(|e| McpError::internal_error(format!("log: {e}"), None))?
            .as_ref()
            .clone(),
    };
    let mut counts: AHashMap<RelPath, u32> = AHashMap::new();
    for c in &commits {
        for (path, _kind) in &c.files {
            *counts.entry(path.clone()).or_default() += 1;
        }
    }
    Ok(counts)
}
