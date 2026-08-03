//! The self-contained, offline interactive HTML renderer (ADR-0005 / ADR-0007).
//!
//! [`to_html`] turns a [`GraphView`] into a single HTML document with **zero external dependencies**:
//! the graph data is embedded as JSON in a `<script type="application/json">` block, and a compact
//! vanilla-JS canvas engine (inlined below) lays it out and draws it — pan, zoom, hover tooltips,
//! live name search, and a community legend. No CDN, no vendored library, no network at view time;
//! the file works opened straight off disk.
//!
//! The emitted **string is deterministic** (a static template with the deterministic node-link JSON
//! spliced in), so it is snapshot-testable. The in-browser layout is also deterministic — initial
//! positions are seeded from node index and the force simulation uses no randomness — so the same
//! view renders the same picture every time.
//!
//! Security: the embedded JSON has `<`, `>`, and `&` replaced with their `\uXXXX` escapes before
//! splicing, so a hostile symbol name containing `</script>` cannot break out of the data block
//! (JSON.parse decodes the escapes back to the original text).

use super::graph_view::{GraphView, to_node_link};

/// Render a view as a self-contained interactive HTML page.
pub(super) fn to_html(view: &GraphView) -> String {
    let json = to_node_link(view);
    // Neutralize the only sequences that could break out of the <script type="application/json">
    // block. These characters only ever appear inside JSON string values (names/paths/labels), so
    // escaping them keeps the document valid and JSON.parse restores the originals.
    let safe = json
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    TEMPLATE.replace(DATA_PLACEHOLDER, &safe)
}

const DATA_PLACEHOLDER: &str = "__BASEMIND_GRAPH_DATA__";

const TEMPLATE: &str = r####"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>basemind graph</title>
<style>
  :root { color-scheme: dark; }
  html, body { margin: 0; height: 100%; overflow: hidden; background: #14161a; color: #e6e6e6;
    font: 13px/1.4 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
  #toolbar { position: fixed; top: 8px; left: 8px; z-index: 3; display: flex; gap: 8px; align-items: center; }
  #search { background: #22262d; border: 1px solid #333a44; color: #e6e6e6; padding: 5px 8px;
    border-radius: 6px; width: 220px; outline: none; }
  #stats { color: #8a93a0; }
  #legend { position: fixed; top: 44px; left: 8px; z-index: 3; max-height: 60vh; overflow: auto;
    background: rgba(20,22,26,.8); border: 1px solid #262b33; border-radius: 6px; padding: 6px 8px; }
  #legend div { display: flex; align-items: center; gap: 6px; margin: 2px 0; cursor: default; }
  #legend .sw { width: 11px; height: 11px; border-radius: 2px; flex: 0 0 auto; }
  #tooltip { position: fixed; z-index: 4; pointer-events: none; display: none; max-width: 320px;
    background: #0f1114; border: 1px solid #333a44; border-radius: 6px; padding: 6px 8px; }
  #tooltip b { color: #fff; } #tooltip .m { color: #8a93a0; }
  canvas { position: fixed; inset: 0; display: block; }
</style>
</head>
<body>
<div id="toolbar">
  <input id="search" type="text" placeholder="search nodes…" autocomplete="off" spellcheck="false">
  <span id="stats"></span>
</div>
<div id="legend"></div>
<div id="tooltip"></div>
<canvas id="c"></canvas>
<script id="graph-data" type="application/json">__BASEMIND_GRAPH_DATA__</script>
<script>
(function () {
  "use strict";
  var data = JSON.parse(document.getElementById("graph-data").textContent);
  var nodes = data.nodes || [], links = data.links || [];
  var N = nodes.length;
  var byId = new Map();
  nodes.forEach(function (n) { byId.set(n.id, n); });

  function hue(c) { return (c * 137.508) % 360; }
  nodes.forEach(function (n) { n.color = "hsl(" + hue(n.community || 0).toFixed(1) + ",62%,58%)"; });

  // Deterministic seed positions on a circle by index — no randomness anywhere.
  nodes.forEach(function (n, i) {
    var a = N > 1 ? (2 * Math.PI * i) / N : 0;
    n.x = Math.cos(a) * 320; n.y = Math.sin(a) * 320; n.vx = 0; n.vy = 0;
    n.r = 4 + Math.min(8, Math.sqrt((n.centrality || 0) + 1));
  });

  // Fixed-iteration force layout (deterministic). O(n^2) repulsion is fine for the capped view.
  var ITER = N > 1200 ? 60 : 150;
  for (var it = 0; it < ITER; it++) {
    for (var i = 0; i < N; i++) {
      for (var j = i + 1; j < N; j++) {
        var a = nodes[i], b = nodes[j];
        var dx = a.x - b.x, dy = a.y - b.y, d2 = dx * dx + dy * dy + 0.01, d = Math.sqrt(d2);
        var f = 1800 / d2; dx /= d; dy /= d;
        a.vx += dx * f; a.vy += dy * f; b.vx -= dx * f; b.vy -= dy * f;
      }
    }
    links.forEach(function (l) {
      var a = byId.get(l.source), b = byId.get(l.target); if (!a || !b) return;
      var dx = b.x - a.x, dy = b.y - a.y, d = Math.sqrt(dx * dx + dy * dy) + 0.01;
      var f = (d - 90) * 0.05 * (l.confidence || 1); dx /= d; dy /= d;
      a.vx += dx * f; a.vy += dy * f; b.vx -= dx * f; b.vy -= dy * f;
    });
    for (var k = 0; k < N; k++) {
      var n = nodes[k];
      n.vx -= n.x * 0.002; n.vy -= n.y * 0.002;
      n.x += n.vx * 0.85; n.y += n.vy * 0.85; n.vx *= 0.85; n.vy *= 0.85;
    }
  }

  var cv = document.getElementById("c"), ctx = cv.getContext("2d");
  var tip = document.getElementById("tooltip");
  var scale = 1, ox = 0, oy = 0, query = "";

  function fit() {
    if (!N) return;
    var minx = Infinity, miny = Infinity, maxx = -Infinity, maxy = -Infinity;
    nodes.forEach(function (n) {
      minx = Math.min(minx, n.x); miny = Math.min(miny, n.y);
      maxx = Math.max(maxx, n.x); maxy = Math.max(maxy, n.y);
    });
    var w = innerWidth, h = innerHeight, gw = maxx - minx || 1, gh = maxy - miny || 1;
    scale = Math.min(w / (gw + 120), h / (gh + 120), 2);
    ox = w / 2 - (minx + maxx) / 2 * scale; oy = h / 2 - (miny + maxy) / 2 * scale;
  }

  function matches(n) { return query && (n.label || "").toLowerCase().indexOf(query) >= 0; }

  function draw() {
    var dpr = window.devicePixelRatio || 1;
    cv.width = innerWidth * dpr; cv.height = innerHeight * dpr;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, innerWidth, innerHeight);
    ctx.lineWidth = 1;
    links.forEach(function (l) {
      var a = byId.get(l.source), b = byId.get(l.target); if (!a || !b) return;
      var dim = query && !(matches(a) || matches(b));
      ctx.strokeStyle = dim ? "rgba(120,130,140,.06)" : "rgba(150,160,175," + (0.12 + 0.25 * (l.confidence || 0.3)) + ")";
      ctx.beginPath();
      ctx.moveTo(a.x * scale + ox, a.y * scale + oy);
      ctx.lineTo(b.x * scale + ox, b.y * scale + oy);
      ctx.stroke();
    });
    nodes.forEach(function (n) {
      var dim = query && !matches(n);
      ctx.globalAlpha = dim ? 0.15 : 1;
      ctx.fillStyle = n.color;
      ctx.beginPath();
      ctx.arc(n.x * scale + ox, n.y * scale + oy, n.r, 0, 6.2832);
      ctx.fill();
      if (matches(n)) { ctx.strokeStyle = "#fff"; ctx.lineWidth = 2; ctx.stroke(); ctx.lineWidth = 1; }
    });
    ctx.globalAlpha = 1;
    document.getElementById("stats").textContent =
      N + " nodes · " + links.length + " edges" + (data.truncated ? " · truncated" : "");
  }

  function pick(px, py) {
    var best = null, bd = 12 * 12;
    for (var i = 0; i < N; i++) {
      var n = nodes[i], sx = n.x * scale + ox, sy = n.y * scale + oy;
      var dx = px - sx, dy = py - sy, d = dx * dx + dy * dy;
      if (d < bd) { bd = d; best = n; }
    }
    return best;
  }

  cv.addEventListener("mousemove", function (e) {
    if (e.buttons === 1) { ox += e.movementX; oy += e.movementY; draw(); tip.style.display = "none"; return; }
    var n = pick(e.clientX, e.clientY);
    if (n) {
      tip.style.display = "block"; tip.style.left = (e.clientX + 12) + "px"; tip.style.top = (e.clientY + 12) + "px";
      tip.innerHTML = "<b></b><div class=m></div><div class=m></div>";
      tip.children[0].textContent = n.label || "";
      tip.children[1].textContent = (n.kind || "") + (n.path ? " · " + n.path : "");
      tip.children[2].textContent = n.community_label || "";
    } else { tip.style.display = "none"; }
  });
  cv.addEventListener("mouseleave", function () { tip.style.display = "none"; });
  cv.addEventListener("wheel", function (e) {
    e.preventDefault();
    var f = Math.exp(-e.deltaY * 0.001), mx = e.clientX, my = e.clientY;
    ox = mx - (mx - ox) * f; oy = my - (my - oy) * f; scale *= f; draw();
  }, { passive: false });

  var legend = document.getElementById("legend"), seen = new Map();
  nodes.forEach(function (n) { if (!seen.has(n.community)) seen.set(n.community, n.community_label || ("community " + n.community)); });
  Array.from(seen.entries()).sort(function (a, b) { return a[0] - b[0]; }).forEach(function (e) {
    var row = document.createElement("div");
    var sw = document.createElement("span"); sw.className = "sw"; sw.style.background = "hsl(" + hue(e[0]).toFixed(1) + ",62%,58%)";
    var lb = document.createElement("span"); lb.textContent = e[1];
    row.appendChild(sw); row.appendChild(lb); legend.appendChild(row);
  });

  document.getElementById("search").addEventListener("input", function (e) {
    query = e.target.value.trim().toLowerCase(); draw();
  });
  window.addEventListener("resize", draw);
  fit(); draw();
})();
</script>
</body>
</html>
"####;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::graph_view::{GraphView, GraphViewEdge, GraphViewNode};
    use crate::path::RelPath;

    fn view() -> GraphView {
        GraphView {
            nodes: vec![GraphViewNode {
                id: 0,
                name: "engine</script><script>alert(1)".into(), // breakout attempt
                kind: "function".into(),
                path: Some(RelPath::from("src/core.rs")),
                start_row: Some(1),
                start_col: Some(0),
                community: 0,
                community_label: "src · engine".into(),
                centrality: 10,
            }],
            edges: vec![GraphViewEdge {
                from: 0,
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
    fn html_is_a_self_contained_document() {
        let out = to_html(&view());
        assert!(out.starts_with("<!doctype html>"), "is an HTML document");
        assert!(out.contains("<canvas id=\"c\">"), "has the canvas");
        // Fully offline: no external resource references.
        assert!(
            !out.contains("http://") && !out.contains("https://"),
            "no network references: {out:.0}"
        );
        assert!(!out.contains("src=\"//"), "no protocol-relative src");
        // The placeholder was replaced with real data.
        assert!(!out.contains(DATA_PLACEHOLDER), "data placeholder spliced");
    }

    #[test]
    fn embedded_data_cannot_break_out_of_the_script_block() {
        let out = to_html(&view());
        // The literal `</script>` from the hostile name must NOT appear inside the data block; it is
        // escaped to </script>, so the data <script> stays open until its real close tag.
        // Exactly two real </script> tags exist (the data block's and the engine's).
        assert_eq!(out.matches("</script>").count(), 2, "no injected closing tag");
        assert!(
            out.contains("\\u003c/script\\u003e") || out.contains("\\u003cscript"),
            "name was escaped"
        );
    }

    #[test]
    fn html_render_is_deterministic() {
        assert_eq!(to_html(&view()), to_html(&view()));
    }
}
