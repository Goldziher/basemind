//! The canonical graph-view payload and its pluggable text renderers (ADR-0005).
//!
//! One payload — [`GraphView`] — is a superset of the common node-link exchange shape: nodes carry
//! identity, label, location, kind, community + community label (ADR-0004), and centrality; edges
//! carry endpoints, kind, and provenance/confidence/weight (ADR-0002). Every renderer here, and the
//! future UI (ADR-0006), consumes this one payload.
//!
//! The renderers are **pure and deterministic** (they iterate the view in id order, so the same
//! view always yields byte-identical output — snapshot-testable) and **offline** (plain strings, no
//! assets, no network). This pass ships the machine/text formats; SVG and the self-contained
//! interactive HTML page are deferred to the UI ADRs (0006/0007). The view builder that assembles a
//! `GraphView` from the shared code-graph lives in `helpers_graphview` (it needs the L1 cache).

use std::fmt::Write as _;

use crate::path::RelPath;

/// One node in the canonical payload. `id` is a dense `0..node_count` index; edges reference it.
#[derive(Debug, Clone)]
pub(crate) struct GraphViewNode {
    pub(crate) id: u32,
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) path: Option<RelPath>,
    pub(crate) start_row: Option<u32>,
    pub(crate) start_col: Option<u32>,
    pub(crate) community: u32,
    pub(crate) community_label: String,
    pub(crate) centrality: u64,
}

/// One typed, provenance-tagged edge. `from`/`to` are node `id`s.
#[derive(Debug, Clone)]
pub(crate) struct GraphViewEdge {
    pub(crate) from: u32,
    pub(crate) to: u32,
    pub(crate) kind: String,
    pub(crate) provenance: String,
    pub(crate) confidence: f32,
    pub(crate) weight: u32,
}

/// The canonical graph-view payload consumed by every renderer and the UI.
#[derive(Debug, Default)]
pub(crate) struct GraphView {
    pub(crate) nodes: Vec<GraphViewNode>,
    pub(crate) edges: Vec<GraphViewEdge>,
    /// True when the underlying scan was truncated or the view was capped.
    pub(crate) truncated: bool,
}

/// Which text renderer to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphFormat {
    /// Node-link JSON — the common interchange shape (`{directed, multigraph, nodes, links}`).
    NodeLink,
    /// Graphviz DOT.
    Dot,
    /// Mermaid `graph` flowchart.
    Mermaid,
    /// GraphML (XML) for graph-database / tooling import.
    GraphMl,
    /// Cypher `CREATE` statements for Neo4j-style import.
    Cypher,
    /// A self-contained, offline interactive HTML page (zero dependencies).
    Html,
}

impl GraphFormat {
    /// Parse the tool `format` param. Accepts a few common spellings.
    pub(crate) fn parse(s: &str) -> Option<GraphFormat> {
        match s {
            "node_link" | "nodelink" | "json" => Some(GraphFormat::NodeLink),
            "dot" | "graphviz" => Some(GraphFormat::Dot),
            "mermaid" => Some(GraphFormat::Mermaid),
            "graphml" => Some(GraphFormat::GraphMl),
            "cypher" => Some(GraphFormat::Cypher),
            "html" | "interactive" => Some(GraphFormat::Html),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            GraphFormat::NodeLink => "node_link",
            GraphFormat::Dot => "dot",
            GraphFormat::Mermaid => "mermaid",
            GraphFormat::GraphMl => "graphml",
            GraphFormat::Cypher => "cypher",
            GraphFormat::Html => "html",
        }
    }
}

/// Render a view to the requested format.
pub(crate) fn render(view: &GraphView, format: GraphFormat) -> String {
    match format {
        GraphFormat::NodeLink => to_node_link(view),
        GraphFormat::Dot => to_dot(view),
        GraphFormat::Mermaid => to_mermaid(view),
        GraphFormat::GraphMl => to_graphml(view),
        GraphFormat::Cypher => to_cypher(view),
        GraphFormat::Html => super::graph_html::to_html(view),
    }
}

fn path_str(node: &GraphViewNode) -> Option<&str> {
    node.path.as_ref().and_then(|p| p.as_str())
}

/// Node-link JSON — a superset of the NetworkX/D3 shape so it interoperates with existing graph
/// consumers. Built via `serde_json` so escaping is handled correctly.
pub(super) fn to_node_link(view: &GraphView) -> String {
    let nodes: Vec<serde_json::Value> = view
        .nodes
        .iter()
        .map(|n| {
            serde_json::json!({
                "id": n.id,
                "label": n.name,
                "kind": n.kind,
                "path": path_str(n),
                "start_row": n.start_row,
                "start_col": n.start_col,
                "community": n.community,
                "community_label": n.community_label,
                "centrality": n.centrality,
            })
        })
        .collect();
    let links: Vec<serde_json::Value> = view
        .edges
        .iter()
        .map(|e| {
            serde_json::json!({
                "source": e.from,
                "target": e.to,
                "kind": e.kind,
                "provenance": e.provenance,
                "confidence": e.confidence,
                "weight": e.weight,
            })
        })
        .collect();
    let doc = serde_json::json!({
        "directed": true,
        "multigraph": true,
        "truncated": view.truncated,
        "nodes": nodes,
        "links": links,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}

/// Escape a string for a DOT double-quoted id/label: backslash and double-quote, and collapse raw
/// newlines to spaces so a name can't spread the label across lines.
fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace(['\n', '\r'], " ")
}

fn to_dot(view: &GraphView) -> String {
    let mut out = String::new();
    out.push_str("digraph basemind {\n  rankdir=LR;\n  node [shape=box];\n");
    for n in &view.nodes {
        // Escape name and path independently, then join with the DOT line-break `\n`, so the
        // escaper never turns the separator itself into a literal backslash-n.
        let label = match path_str(n) {
            Some(p) => format!("{}\\n{}", dot_escape(&n.name), dot_escape(p)),
            None => dot_escape(&n.name),
        };
        let _ = writeln!(
            out,
            "  n{} [label=\"{}\", tooltip=\"{}\"];",
            n.id,
            label,
            dot_escape(&n.community_label)
        );
    }
    for e in &view.edges {
        let _ = writeln!(
            out,
            "  n{} -> n{} [label=\"{}\", penwidth={:.2}];",
            e.from,
            e.to,
            dot_escape(&e.kind),
            1.0 + e.confidence,
        );
    }
    out.push_str("}\n");
    out
}

/// Escape a label for a Mermaid node text wrapped in double quotes.
fn mermaid_escape(s: &str) -> String {
    s.replace('"', "&quot;").replace(['\n', '\r'], " ")
}

fn to_mermaid(view: &GraphView) -> String {
    let mut out = String::from("graph LR\n");
    for n in &view.nodes {
        let _ = writeln!(out, "  n{}[\"{}\"]", n.id, mermaid_escape(&n.name));
    }
    for e in &view.edges {
        let _ = writeln!(out, "  n{} -->|{}| n{}", e.from, mermaid_escape(&e.kind), e.to);
    }
    out
}

/// Escape text for an XML attribute/body. Drops C0 control characters XML 1.0 forbids (everything
/// below U+0020 except tab/newline/carriage-return) — they have no legal escape and would make the
/// document non-well-formed.
fn xml_escape(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|&c| c == '\t' || c == '\n' || c == '\r' || c >= ' ')
        .collect();
    cleaned
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn to_graphml(view: &GraphView) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\">\n");
    for (id, ty, name) in [
        ("d_label", "string", "label"),
        ("d_kind", "string", "kind"),
        ("d_path", "string", "path"),
        ("d_community", "long", "community"),
        ("d_community_label", "string", "community_label"),
        ("d_centrality", "long", "centrality"),
    ] {
        let _ = writeln!(
            out,
            "  <key id=\"{id}\" for=\"node\" attr.name=\"{name}\" attr.type=\"{ty}\"/>"
        );
    }
    for (id, ty, name) in [
        ("e_kind", "string", "kind"),
        ("e_provenance", "string", "provenance"),
        ("e_confidence", "double", "confidence"),
        ("e_weight", "long", "weight"),
    ] {
        let _ = writeln!(
            out,
            "  <key id=\"{id}\" for=\"edge\" attr.name=\"{name}\" attr.type=\"{ty}\"/>"
        );
    }
    out.push_str("  <graph edgedefault=\"directed\">\n");
    for n in &view.nodes {
        let _ = writeln!(out, "    <node id=\"n{}\">", n.id);
        let _ = writeln!(out, "      <data key=\"d_label\">{}</data>", xml_escape(&n.name));
        let _ = writeln!(out, "      <data key=\"d_kind\">{}</data>", xml_escape(&n.kind));
        if let Some(p) = path_str(n) {
            let _ = writeln!(out, "      <data key=\"d_path\">{}</data>", xml_escape(p));
        }
        let _ = writeln!(out, "      <data key=\"d_community\">{}</data>", n.community);
        let _ = writeln!(
            out,
            "      <data key=\"d_community_label\">{}</data>",
            xml_escape(&n.community_label)
        );
        let _ = writeln!(out, "      <data key=\"d_centrality\">{}</data>", n.centrality);
        out.push_str("    </node>\n");
    }
    for (i, e) in view.edges.iter().enumerate() {
        let _ = writeln!(
            out,
            "    <edge id=\"e{i}\" source=\"n{}\" target=\"n{}\">",
            e.from, e.to
        );
        let _ = writeln!(out, "      <data key=\"e_kind\">{}</data>", xml_escape(&e.kind));
        let _ = writeln!(
            out,
            "      <data key=\"e_provenance\">{}</data>",
            xml_escape(&e.provenance)
        );
        let _ = writeln!(out, "      <data key=\"e_confidence\">{:.3}</data>", e.confidence);
        let _ = writeln!(out, "      <data key=\"e_weight\">{}</data>", e.weight);
        out.push_str("    </edge>\n");
    }
    out.push_str("  </graph>\n</graphml>\n");
    out
}

/// Escape a string for a single-quoted Cypher literal.
fn cypher_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Map an edge kind to a Cypher relationship type (uppercase, the community convention). The rel
/// type is emitted unquoted, so restrict it to a safe `[A-Z0-9_]` token defensively — `kind` is a
/// fixed enum (calls/imports/inherits/contains) today, but this keeps a future variant from ever
/// becoming an injection point.
fn cypher_rel(kind: &str) -> String {
    let rel: String = kind
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_ascii_uppercase();
    if rel.is_empty() { "REL".to_string() } else { rel }
}

fn to_cypher(view: &GraphView) -> String {
    let mut out = String::new();
    for n in &view.nodes {
        let path = path_str(n).unwrap_or("");
        let _ = writeln!(
            out,
            "CREATE (n{}:Symbol {{name:'{}', kind:'{}', path:'{}', community:{}, community_label:'{}', centrality:{}}})",
            n.id,
            cypher_escape(&n.name),
            cypher_escape(&n.kind),
            cypher_escape(path),
            n.community,
            cypher_escape(&n.community_label),
            n.centrality
        );
    }
    for e in &view.edges {
        let _ = writeln!(
            out,
            "CREATE (n{})-[:{} {{provenance:'{}', confidence:{:.3}, weight:{}}}]->(n{})",
            e.from,
            cypher_rel(&e.kind),
            cypher_escape(&e.provenance),
            e.confidence,
            e.weight,
            e.to
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> GraphView {
        GraphView {
            nodes: vec![
                GraphViewNode {
                    id: 0,
                    name: "wr<a>&p".into(), // angle brackets + ampersand exercise XML escaping
                    kind: "function".into(),
                    path: Some(RelPath::from("src/core.rs")),
                    start_row: Some(1),
                    start_col: Some(0),
                    community: 0,
                    community_label: "src · engine".into(),
                    centrality: 20,
                },
                GraphViewNode {
                    id: 1,
                    name: "he\"l'p\\er".into(), // quote + single-quote + backslash exercise DOT/Cypher
                    kind: "function".into(),
                    path: Some(RelPath::from("src/core.rs")),
                    start_row: Some(2),
                    start_col: Some(0),
                    community: 0,
                    community_label: "src · engine".into(),
                    centrality: 10,
                },
            ],
            edges: vec![GraphViewEdge {
                from: 1,
                to: 0,
                kind: "calls".into(),
                provenance: "extracted".into(),
                confidence: 1.0,
                weight: 1,
            }],
            truncated: false,
        }
    }

    #[test]
    fn format_parse_accepts_synonyms() {
        assert_eq!(GraphFormat::parse("json"), Some(GraphFormat::NodeLink));
        assert_eq!(GraphFormat::parse("graphviz"), Some(GraphFormat::Dot));
        assert_eq!(GraphFormat::parse("mermaid"), Some(GraphFormat::Mermaid));
        assert_eq!(GraphFormat::parse("graphml"), Some(GraphFormat::GraphMl));
        assert_eq!(GraphFormat::parse("cypher"), Some(GraphFormat::Cypher));
        assert_eq!(GraphFormat::parse("bogus"), None);
    }

    #[test]
    fn node_link_is_valid_json_with_nodes_and_links() {
        let out = render(&sample(), GraphFormat::NodeLink);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(v["directed"], serde_json::json!(true));
        assert_eq!(v["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(v["links"].as_array().unwrap().len(), 1);
        assert_eq!(v["links"][0]["source"], serde_json::json!(1));
        assert_eq!(v["links"][0]["target"], serde_json::json!(0));
        // Hostile characters round-trip through JSON exactly, without corrupting the document.
        assert_eq!(v["nodes"][0]["label"], serde_json::json!("wr<a>&p"));
        assert_eq!(v["nodes"][1]["label"], serde_json::json!("he\"l'p\\er"));
    }

    #[test]
    fn dot_escapes_quotes_and_wires_edges() {
        let out = render(&sample(), GraphFormat::Dot);
        assert!(out.starts_with("digraph basemind {"));
        assert!(out.contains("n1 -> n0"));
        // Backslash escaped to `\\`, then the double-quote to `\"` — `he"l'p\er` → `he\"l'p\\er`.
        assert!(out.contains("he\\\"l'p\\\\er"), "dot label escaping: {out}");
    }

    #[test]
    fn mermaid_escapes_quotes() {
        let out = render(&sample(), GraphFormat::Mermaid);
        assert!(out.starts_with("graph LR"));
        assert!(out.contains("n1 -->|calls| n0"));
        assert!(out.contains("he&quot;l'p\\er"), "mermaid quote escaping: {out}");
    }

    #[test]
    fn graphml_is_escaped_xml() {
        let out = render(&sample(), GraphFormat::GraphMl);
        assert!(out.contains("<graphml"));
        // Angle brackets and ampersand must all be entity-escaped in the element body.
        assert!(out.contains("wr&lt;a&gt;&amp;p"), "xml body escaping: {out}");
        assert!(out.contains("he&quot;l&apos;p\\er"), "xml quote/apos escaping: {out}");
        assert!(out.contains("source=\"n1\" target=\"n0\""));
    }

    #[test]
    fn cypher_escapes_and_maps_rel_type() {
        let out = render(&sample(), GraphFormat::Cypher);
        assert!(out.contains("CREATE (n0:Symbol"));
        assert!(out.contains("-[:CALLS "), "kind maps to uppercase rel type: {out}");
        // Backslash → `\\` and single-quote → `\'` keep the single-quoted literal closed:
        // `he"l'p\er` → `he"l\'p\\er`.
        assert!(out.contains("he\"l\\'p\\\\er"), "cypher literal escaping: {out}");
    }

    #[test]
    fn cypher_rel_rejects_unsafe_kind() {
        // Defense in depth: a non-enum kind is sanitized to a safe token, never injected raw.
        assert_eq!(cypher_rel("calls"), "CALLS");
        assert_eq!(cypher_rel("x]->(m) DETACH DELETE n //"), "XMDETACHDELETEN");
        assert_eq!(cypher_rel("!!!"), "REL");
    }

    #[test]
    fn xml_escape_drops_forbidden_control_chars() {
        // A raw C0 control char (U+0001) has no legal XML escape; it must be dropped, not emitted.
        let out = xml_escape("a\u{01}b");
        assert_eq!(out, "ab");
        // Tab/newline/return are legal and preserved.
        assert_eq!(xml_escape("a\tb\nc"), "a\tb\nc");
    }

    #[test]
    fn renderers_are_deterministic() {
        let v = sample();
        for f in [
            GraphFormat::NodeLink,
            GraphFormat::Dot,
            GraphFormat::Mermaid,
            GraphFormat::GraphMl,
            GraphFormat::Cypher,
        ] {
            assert_eq!(render(&v, f), render(&v, f), "{f:?}");
        }
    }
}
