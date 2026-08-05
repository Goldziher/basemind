//! The static SVG picture renderer (ADR-0005).
//!
//! [`to_svg`] turns a [`GraphView`] into a single self-contained SVG document — the one export the
//! rendering engine was missing. Unlike the interactive HTML page (`graph_html`), which ships the
//! graph data and lays it out in the browser, an SVG is *static*: the node positions must be baked
//! in. So this module runs the **same deterministic force layout the HTML engine runs**, but in Rust
//! at render time, and emits the resolved coordinates as `<line>`/`<circle>` geometry.
//!
//! The layout is a fixed-iteration spring simulation seeded from node index — identical constants to
//! the JS engine in `graph_html`, and no randomness anywhere — so the same view always yields
//! byte-identical output (snapshot-testable). Colors are the same community hues as the HTML/legend,
//! converted HSL→RGB here so the file renders correctly in plain SVG viewers (Inkscape, image
//! pipelines) that do not accept `hsl()` in presentation attributes.
//!
//! Cost: unlike the HTML format (whose layout runs client-side in the browser), the SVG layout runs
//! server-side and is `O(n²)` per iteration — like every other graph tool it builds synchronously in
//! the handler. The `max_nodes` cap (default 500, max 2000) bounds it; at the default it is tens of
//! ms, and the 2000-node ceiling is the worst case a caller opts into explicitly.
//!
//! Security: every name/label reaching the document goes through [`xml_escape`], which drops control
//! bytes (CWE-150) and escapes the XML metacharacters — the same guarantee the GraphML renderer has.

use std::fmt::Write as _;

use super::graph_view::{GraphView, xml_escape};

/// Force-layout constants — kept byte-identical to the JS engine in `graph_html` so the static
/// picture matches the interactive one. See that module for the derivation.
const SEED_RADIUS: f64 = 320.0;
const REPULSION: f64 = 1800.0;
const SPRING_REST: f64 = 90.0;
const SPRING_K: f64 = 0.05;
const CENTER_PULL: f64 = 0.002;
const DAMPING: f64 = 0.85;
/// Iteration count matches the HTML engine's `N > 1200 ? 60 : 150` split.
const ITERS_DENSE: u32 = 60;
const ITERS_SPARSE: u32 = 150;
const DENSE_THRESHOLD: usize = 1200;
/// Padding (SVG user units) around the laid-out graph's bounding box.
const PAD: f64 = 40.0;
/// Above this node count the static picture omits per-node labels: hundreds of overlapping text
/// runs are unreadable noise and bloat the file. The community legend still names every cluster, and
/// the interactive HTML export (`format: "html"`) is the answer when per-node labels are wanted.
const LABEL_LIMIT: usize = 60;

struct Body {
    x: f64,
    y: f64,
    vx: f64,
    vy: f64,
    r: f64,
}

/// Run the deterministic spring layout and return one [`Body`] per node, index-aligned with
/// `view.nodes`. Mirrors `graph_html`'s in-browser simulation exactly (seed, forces, integration).
fn layout(view: &GraphView) -> Vec<Body> {
    let n = view.nodes.len();
    let mut bodies: Vec<Body> = view
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let a = if n > 1 {
                (2.0 * std::f64::consts::PI * i as f64) / n as f64
            } else {
                0.0
            };
            Body {
                x: a.cos() * SEED_RADIUS,
                y: a.sin() * SEED_RADIUS,
                vx: 0.0,
                vy: 0.0,
                r: 4.0 + (node.centrality as f64 + 1.0).sqrt().min(8.0),
            }
        })
        .collect();

    let iters = if n > DENSE_THRESHOLD { ITERS_DENSE } else { ITERS_SPARSE };
    for _ in 0..iters {
        // Pairwise repulsion (O(n^2), bounded by the `max_nodes` cap the view builder applies).
        for i in 0..n {
            for j in (i + 1)..n {
                let mut dx = bodies[i].x - bodies[j].x;
                let mut dy = bodies[i].y - bodies[j].y;
                let d2 = dx * dx + dy * dy + 0.01;
                let d = d2.sqrt();
                let f = REPULSION / d2;
                dx /= d;
                dy /= d;
                bodies[i].vx += dx * f;
                bodies[i].vy += dy * f;
                bodies[j].vx -= dx * f;
                bodies[j].vy -= dy * f;
            }
        }
        // Spring attraction along edges, softened by edge confidence.
        for e in &view.edges {
            let (a, b) = (e.from as usize, e.to as usize);
            if a >= n || b >= n {
                continue;
            }
            let mut dx = bodies[b].x - bodies[a].x;
            let mut dy = bodies[b].y - bodies[a].y;
            let d = (dx * dx + dy * dy).sqrt() + 0.01;
            let conf = if e.confidence > 0.0 { e.confidence as f64 } else { 1.0 };
            let f = (d - SPRING_REST) * SPRING_K * conf;
            dx /= d;
            dy /= d;
            bodies[a].vx += dx * f;
            bodies[a].vy += dy * f;
            bodies[b].vx -= dx * f;
            bodies[b].vy -= dy * f;
        }
        // Centering pull + velocity integration with damping.
        for body in bodies.iter_mut() {
            body.vx -= body.x * CENTER_PULL;
            body.vy -= body.y * CENTER_PULL;
            body.x += body.vx * DAMPING;
            body.y += body.vy * DAMPING;
            body.vx *= DAMPING;
            body.vy *= DAMPING;
        }
    }
    bodies
}

/// Community hue, matching the HTML engine + legend: golden-angle spacing keeps adjacent community
/// ids visually distinct.
fn hue(community: u32) -> f64 {
    (community as f64 * 137.508) % 360.0
}

/// Convert an HSL triple (h in degrees, s/l in 0..=1) to a `#rrggbb` hex string. SVG presentation
/// attributes don't portably accept `hsl()`, so the community colors are baked to RGB here.
fn hsl_to_hex(h: f64, s: f64, l: f64) -> String {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp.floor() as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to_byte = |v: f64| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", to_byte(r1), to_byte(g1), to_byte(b1))
}

fn node_color(community: u32) -> String {
    hsl_to_hex(hue(community), 0.62, 0.58)
}

/// Bounding box of the laid-out bodies, or a unit box when empty.
fn bounds(bodies: &[Body]) -> (f64, f64, f64, f64) {
    let mut minx = f64::INFINITY;
    let mut miny = f64::INFINITY;
    let mut maxx = f64::NEG_INFINITY;
    let mut maxy = f64::NEG_INFINITY;
    for b in bodies {
        minx = minx.min(b.x - b.r);
        miny = miny.min(b.y - b.r);
        maxx = maxx.max(b.x + b.r);
        maxy = maxy.max(b.y + b.r);
    }
    if !minx.is_finite() {
        return (0.0, 0.0, 1.0, 1.0);
    }
    (minx, miny, maxx, maxy)
}

/// Render a view as a self-contained static SVG document.
pub(super) fn to_svg(view: &GraphView) -> String {
    let bodies = layout(view);
    let (minx, miny, maxx, maxy) = bounds(&bodies);
    // Translate the layout so the padded bounding box starts at the origin, giving a tight viewBox.
    let tx = PAD - minx;
    let ty = PAD - miny;
    let width = (maxx - minx) + 2.0 * PAD;
    let height = (maxy - miny) + 2.0 * PAD;
    let px = |v: f64| v + tx;
    let py = |v: f64| v + ty;

    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(
        out,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {width:.1} {height:.1}\" \
         font-family=\"ui-monospace, SFMono-Regular, Menlo, Consolas, monospace\">"
    );
    let _ = writeln!(
        out,
        "  <rect width=\"{width:.1}\" height=\"{height:.1}\" fill=\"#14161a\"/>"
    );

    // Edges first so nodes paint on top. Stroke opacity tracks confidence (matches the HTML engine).
    out.push_str("  <g stroke-linecap=\"round\">\n");
    for e in &view.edges {
        let (a, b) = (e.from as usize, e.to as usize);
        if a >= bodies.len() || b >= bodies.len() {
            continue;
        }
        // zero-confidence edge (none exist today, but the provenance ladder may grow one) draws
        // identically in both renderers.
        let conf = if e.confidence > 0.0 { e.confidence as f64 } else { 0.3 };
        let opacity = 0.12 + 0.25 * conf.clamp(0.0, 1.0);
        let _ = writeln!(
            out,
            "    <line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" \
             stroke=\"#96a0af\" stroke-opacity=\"{:.3}\"/>",
            px(bodies[a].x),
            py(bodies[a].y),
            px(bodies[b].x),
            py(bodies[b].y),
            opacity,
        );
    }
    out.push_str("  </g>\n");

    // Nodes, colored by community.
    out.push_str("  <g>\n");
    for (node, body) in view.nodes.iter().zip(&bodies) {
        let _ = writeln!(
            out,
            "    <circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"{:.1}\" fill=\"{}\"><title>{}</title></circle>",
            px(body.x),
            py(body.y),
            body.r,
            node_color(node.community),
            xml_escape(&node.name),
        );
    }
    out.push_str("  </g>\n");

    // Per-node labels only for small graphs — a static picture of hundreds of labels is unreadable.
    if view.nodes.len() <= LABEL_LIMIT {
        out.push_str("  <g fill=\"#e6e6e6\" font-size=\"9\">\n");
        for (node, body) in view.nodes.iter().zip(&bodies) {
            let _ = writeln!(
                out,
                "    <text x=\"{:.1}\" y=\"{:.1}\">{}</text>",
                px(body.x) + body.r + 2.0,
                py(body.y) + 3.0,
                xml_escape(&node.name),
            );
        }
        out.push_str("  </g>\n");
    }

    render_legend(&mut out, view, height);
    out.push_str("</svg>\n");
    out
}

/// Draw the community legend (swatch + label per community present), top-left, matching the HTML
/// page's legend ordering (ascending community id).
fn render_legend(out: &mut String, view: &GraphView, height: f64) {
    let mut seen: Vec<(u32, &str)> = Vec::new();
    for node in &view.nodes {
        if !seen.iter().any(|(c, _)| *c == node.community) {
            seen.push((node.community, node.community_label.as_str()));
        }
    }
    if seen.is_empty() {
        return;
    }
    seen.sort_by_key(|(c, _)| *c);
    // Keep the legend within the picture; it is decorative, so cap rows to avoid an overflowing column.
    let max_rows = ((height - 2.0 * PAD) / 16.0).floor().max(1.0) as usize;
    let _ = writeln!(out, "  <g font-size=\"11\">");
    for (row, (community, label)) in seen.iter().take(max_rows).enumerate() {
        let y = PAD + row as f64 * 16.0;
        let _ = writeln!(
            out,
            "    <rect x=\"{:.1}\" y=\"{:.1}\" width=\"11\" height=\"11\" fill=\"{}\"/>",
            PAD,
            y,
            node_color(*community),
        );
        let _ = writeln!(
            out,
            "    <text x=\"{:.1}\" y=\"{:.1}\" fill=\"#e6e6e6\">{}</text>",
            PAD + 16.0,
            y + 10.0,
            xml_escape(label),
        );
    }
    out.push_str("  </g>\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::graph_view::{GraphView, GraphViewEdge, GraphViewNode};
    use crate::path::RelPath;

    fn node(id: u32, name: &str, community: u32, centrality: u64) -> GraphViewNode {
        GraphViewNode {
            id,
            name: name.into(),
            kind: "function".into(),
            path: Some(RelPath::from("src/core.rs")),
            start_row: Some(1),
            start_col: Some(0),
            community,
            community_label: format!("cluster {community}"),
            centrality,
        }
    }

    fn sample() -> GraphView {
        GraphView {
            nodes: vec![node(0, "engine", 0, 20), node(1, "helper", 0, 10), node(2, "run", 1, 5)],
            edges: vec![
                GraphViewEdge {
                    from: 1,
                    to: 0,
                    kind: "calls".into(),
                    provenance: "extracted".into(),
                    confidence: 1.0,
                    weight: 1,
                },
                GraphViewEdge {
                    from: 2,
                    to: 1,
                    kind: "calls".into(),
                    provenance: "inferred".into(),
                    confidence: 0.5,
                    weight: 1,
                },
            ],
            truncated: false,
        }
    }

    #[test]
    fn svg_is_a_self_contained_document_with_geometry() {
        let out = to_svg(&sample());
        assert!(out.starts_with("<?xml version=\"1.0\""), "xml prolog");
        assert!(
            out.contains("<svg xmlns=\"http://www.w3.org/2000/svg\""),
            "svg root: {out}"
        );
        assert!(out.trim_end().ends_with("</svg>"), "closed svg");
        // One circle per node, at least one edge line drawn.
        assert_eq!(out.matches("<circle").count(), 3, "one circle per node: {out}");
        assert!(out.matches("<line").count() >= 2, "edges drawn: {out}");
        // Fully offline: no external resource references. (The SVG `xmlns` is an http *identifier*,
        assert!(!out.contains("href"), "no linked/embedded resources: {out}");
        assert!(!out.contains("url("), "no CSS url() references: {out}");
    }

    #[test]
    fn svg_render_is_deterministic() {
        assert_eq!(to_svg(&sample()), to_svg(&sample()));
    }

    #[test]
    fn svg_labels_are_gated_by_node_count() {
        // A small graph carries per-node labels; the legend is always present.
        let small = to_svg(&sample());
        assert!(small.contains("<text"), "small graph labels nodes: {small}");

        // A graph above LABEL_LIMIT drops per-node labels but still draws every circle + legend.
        let mut big = GraphView::default();
        for i in 0..(LABEL_LIMIT as u32 + 5) {
            big.nodes.push(node(i, &format!("n{i}"), i % 3, 1));
        }
        let out = to_svg(&big);
        assert_eq!(out.matches("<circle").count(), LABEL_LIMIT + 5, "all nodes drawn");
        // The only <text> runs are the legend's 3 community labels — no per-node labels.
        assert_eq!(out.matches("<text").count(), 3, "labels suppressed above limit: {out}");
    }

    #[test]
    fn svg_escapes_hostile_names_and_strips_control_bytes() {
        let mut view = sample();
        // ESC/BEL terminal-injection payload plus XML metacharacters.
        view.nodes[0].name = "ev\u{1b}]0;x\u{07}il <b>&'\"</b>".into();
        let out = to_svg(&view);
        assert!(
            !out.contains('\u{1b}') && !out.contains('\u{07}'),
            "control bytes stripped: {out:?}"
        );
        // A raw `<b>` in a title/label would break the document; it must be entity-escaped.
        assert!(out.contains("&lt;b&gt;"), "angle brackets escaped: {out}");
        assert!(out.contains("&amp;"), "ampersand escaped: {out}");
    }

    #[test]
    fn empty_view_still_renders_a_valid_svg() {
        let out = to_svg(&GraphView::default());
        assert!(out.starts_with("<?xml"), "prolog present");
        assert!(out.contains("<svg"), "svg root present");
        assert_eq!(out.matches("<circle").count(), 0, "no nodes, no circles");
    }

    #[test]
    fn hsl_to_hex_matches_known_anchors() {
        // Pure hues at s=1,l=0.5 give the primary/secondary corners; sanity-checks the conversion.
        assert_eq!(hsl_to_hex(0.0, 1.0, 0.5), "#ff0000");
        assert_eq!(hsl_to_hex(120.0, 1.0, 0.5), "#00ff00");
        assert_eq!(hsl_to_hex(240.0, 1.0, 0.5), "#0000ff");
    }
}
