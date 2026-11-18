"""File-level import edges, the ground truth for the `impact` and `cycles` kinds (ADR-0047 section 4).

An edge `importer -> target` says that `importer` names `target` in an import. The gold is built
from the HEAD tree without basemind:

Rust (from `rust_gold` records, parsed with syn):

- `mod name;` is an edge to the module's file: `name.rs` or `name/mod.rs` in the declaring
  module's directory (the file's own directory for a crate root or `mod.rs`, else `<dir>/<stem>/`),
  under any inline modules it sits in. `#[path = "..."]` is followed.
- `use crate::a::b::C` is an edge to the file of the longest module prefix (`a/b.rs`, else `a.rs`,
  else the crate root). `self::` and `super::` are resolved from the module the `use` sits in, and
  a path starting with a child module of that module (2018 uniform paths) likewise. Every other
  path names another crate and is no edge, workspace siblings included.
- The module tree is walked from every crate root of every package (`src/lib.rs`, `src/main.rs`,
  `src/bin/*`, `build.rs`, `tests/*`, `examples/*`, `benches/*`); a file no root reaches has no
  module path and its `use` lines resolve to nothing. A `[lib] path` override is not read.

Python (from `ast`):

- A file's module name is its path without `.py` (`__init__` names the package), from the repository
  root and from `src/`.
- `import a.b.c` is an edge to the deepest existing module of `a.b.c`; `from a.b import c` to `a.b.c`
  when that is a module, else to `a.b`. Relative imports resolve from the importer's package.
  `if TYPE_CHECKING:` imports count like any other.

Both: an edge to the importer itself is dropped, and each `(importer, target)` pair is kept once
at its first line. `#[cfg]` and other conditions are ignored.
"""

from __future__ import annotations

import ast
import posixpath
from collections import defaultdict, deque
from collections.abc import Iterable

import _common as c

Edges = dict[str, dict[str, int]]  # importer -> {target: first import line}


def _add(edges: Edges, src: str, dst: str | None, line: int) -> None:
    if dst is None or dst == src:
        return
    targets = edges.setdefault(src, {})
    if dst not in targets or line < targets[dst]:
        targets[dst] = line


def _crate_roots(files: set[str]) -> list[str]:
    """Every crate root file of every package with a tracked `Cargo.toml`, sorted."""
    packages = sorted({posixpath.dirname(f) for f in files if posixpath.basename(f) == "Cargo.toml"})
    roots: set[str] = set()
    for pkg in packages:
        main_files = (posixpath.join(pkg, "src", "lib.rs"), posixpath.join(pkg, "src", "main.rs"))
        roots.update(f for f in (*main_files, posixpath.join(pkg, "build.rs")) if f in files)
        for d in (
            posixpath.join(pkg, "src", "bin"),
            *(posixpath.join(pkg, t) for t in ("tests", "examples", "benches")),
        ):
            for f in files:
                rel = f[len(d) + 1 :] if f.startswith(d + "/") else None
                if (
                    rel
                    and rel.endswith(".rs")
                    and (rel.count("/") == 0 or (rel.endswith("/main.rs") and rel.count("/") == 1))
                ):
                    roots.add(f)
    return sorted(roots)


def _mod_dir(path: str, is_root: bool) -> str:
    base = posixpath.basename(path)
    if is_root or base == "mod.rs":
        return posixpath.dirname(path)
    return posixpath.join(posixpath.dirname(path), base[: -len(".rs")])


def rust_edges(files: Iterable[str], recs: list[dict]) -> Edges:
    """Edges among the Rust files of `files` (every tracked path, `Cargo.toml` included)."""
    return rust_graph(files, recs)[0]


def rust_graph(files: Iterable[str], recs: list[dict]) -> tuple[Edges, dict[str, dict[tuple[str, ...], str]]]:
    """The edges, and per crate root the file of every module path its module tree reaches."""
    files = set(files)
    mods: dict[str, list[dict]] = defaultdict(list)
    uses: dict[str, list[dict]] = defaultdict(list)
    for rec in recs:
        if rec["t"] == "mod_file":
            mods[rec["path"]].append(rec)
        elif rec["t"] == "use":
            uses[rec["path"]].append(rec)

    edges: Edges = {}
    # Per crate root: module path -> file, and file -> its module paths.
    module_file: dict[str, dict[tuple[str, ...], str]] = {}
    owners: dict[str, list[tuple[str, tuple[str, ...]]]] = defaultdict(list)
    for root in _crate_roots(files):
        table = {(): root}
        module_file[root] = table
        owners[root].append((root, ()))
        queue = deque([(root, ())])
        while queue:
            path, modpath = queue.popleft()
            for m in mods.get(path, []):
                scope = tuple(m["scope"].split("::")) if m["scope"] else ()
                base = posixpath.join(_mod_dir(path, path == root), *scope)
                if m["path_attr"]:
                    attr_base = posixpath.dirname(path) if not scope else base
                    candidates = [posixpath.normpath(posixpath.join(attr_base, m["path_attr"]))]
                else:
                    candidates = [posixpath.join(base, m["name"] + ".rs"), posixpath.join(base, m["name"], "mod.rs")]
                child = next((f for f in candidates if f in files), None)
                if child is None:
                    continue
                _add(edges, path, child, m["line"])
                child_path = (*modpath, *scope, m["name"])
                if child_path not in table:
                    table[child_path] = child
                    owners[child].append((root, child_path))
                    queue.append((child, child_path))

    for path, recs_here in uses.items():
        for root, modpath in owners.get(path, []):
            table = module_file[root]
            for u in recs_here:
                target = _resolve_use(u, modpath, table)
                _add(edges, path, target, u["line"])
    return edges, module_file


def _resolve_use(u: dict, modpath: tuple[str, ...], table: dict[tuple[str, ...], str]) -> str | None:
    base = (*modpath, *u["scope"].split("::")) if u["scope"] else modpath
    segs = [s for s in u["module"].split("::") if s]
    while segs and segs[-1] in ("self", "*"):
        segs.pop()
    if not segs:
        return None
    if segs[0] == "crate":
        absolute = segs[1:]
    elif segs[0] == "self":
        absolute = [*base, *segs[1:]]
    elif segs[0] == "super":
        ups = 0
        while ups < len(segs) and segs[ups] == "super":
            ups += 1
        if ups > len(base):
            return None
        absolute = [*base[: len(base) - ups], *segs[ups:]]
    elif (*base, segs[0]) in table:
        absolute = [*base, *segs]
    else:
        return None
    for n in range(len(absolute), -1, -1):
        hit = table.get(tuple(absolute[:n]))
        if hit is not None:
            return hit
    return None


def python_edges(blobs: Iterable[tuple[str, str]]) -> Edges:
    """Edges among the Python files of `blobs` (`(path, source)` pairs)."""
    sources = dict(blobs)
    modules: dict[str, str] = {}
    for path in sorted(sources):
        for prefix in ("", "src/"):
            if not path.startswith(prefix):
                continue
            parts = path[len(prefix) : -len(".py")].split("/")
            if parts[-1] == "__init__":
                parts.pop()
            if parts and all(p.isidentifier() for p in parts):
                modules.setdefault(".".join(parts), path)
    names = {path: name for name, path in modules.items()}

    def deepest(dotted: str) -> str | None:
        parts = dotted.split(".")
        for n in range(len(parts), 0, -1):
            hit = modules.get(".".join(parts[:n]))
            if hit:
                return hit
        return None

    edges: Edges = {}
    for path, src in sorted(sources.items()):
        tree = c.parse_python(src)
        if tree is None:
            continue
        own = names.get(path)
        package = None
        if own is not None:
            package = own if path.endswith("__init__.py") else own.rpartition(".")[0]
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                for alias in node.names:
                    _add(edges, path, deepest(alias.name), node.lineno)
            elif isinstance(node, ast.ImportFrom):
                if node.level:
                    if package is None:
                        continue
                    pkg = package.split(".") if package else []
                    if node.level - 1 > len(pkg):
                        continue
                    head = pkg[: len(pkg) - (node.level - 1)]
                    module = ".".join([*head, *(node.module.split(".") if node.module else [])])
                else:
                    module = node.module or ""
                for alias in node.names:
                    sub = f"{module}.{alias.name}" if module else alias.name
                    target = modules.get(sub) if alias.name != "*" else None
                    _add(edges, path, target or (deepest(module) if module else None), node.lineno)
    return edges


def reverse(edges: Edges) -> dict[str, set[str]]:
    rev: dict[str, set[str]] = defaultdict(set)
    for src, targets in edges.items():
        for dst in targets:
            rev[dst].add(src)
    return rev


def importers_within(rev: dict[str, set[str]], target: str, depth: int) -> dict[str, int]:
    """Every file reaching `target` in at most `depth` edges, with its hop count; not `target`."""
    seen = {target: 0}
    frontier = [target]
    for hop in range(1, depth + 1):
        nxt = []
        for f in frontier:
            for imp in sorted(rev.get(f, ())):
                if imp not in seen:
                    seen[imp] = hop
                    nxt.append(imp)
        frontier = nxt
    del seen[target]
    return seen


def cycles(edges: Edges) -> list[list[str]]:
    """Strongly connected components with two or more files (Tarjan, iterative), each sorted, the
    list sorted by size then first path."""
    nodes = sorted(set(edges) | {d for t in edges.values() for d in t})
    succ = {n: sorted(edges.get(n, {})) for n in nodes}
    index: dict[str, int] = {}
    low: dict[str, int] = {}
    on_stack: set[str] = set()
    stack: list[str] = []
    out: list[list[str]] = []
    counter = 0
    for start in nodes:
        if start in index:
            continue
        work = [(start, 0)]
        while work:
            node, i = work.pop()
            if i == 0:
                index[node] = low[node] = counter
                counter += 1
                stack.append(node)
                on_stack.add(node)
            recurse = False
            children = succ[node]
            while i < len(children):
                child = children[i]
                i += 1
                if child not in index:
                    work.append((node, i))
                    work.append((child, 0))
                    recurse = True
                    break
                if child in on_stack:
                    low[node] = min(low[node], index[child])
            if recurse:
                continue
            if low[node] == index[node]:
                comp = []
                while True:
                    w = stack.pop()
                    on_stack.discard(w)
                    comp.append(w)
                    if w == node:
                        break
                if len(comp) > 1:
                    out.append(sorted(comp))
            if work:
                parent = work[-1][0]
                low[parent] = min(low[parent], low[node])
    return sorted(out, key=lambda comp: (len(comp), comp[0]))
