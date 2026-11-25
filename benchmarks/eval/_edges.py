"""File-level import edges, the ground truth for the `impact` and `cycles` kinds (ADR-0047 section 4).

An edge `importer -> target` says that `importer` names `target` in an import. The gold is built
from the HEAD tree without basemind:

Rust (from `rust_gold` records, parsed with syn, and each package's `Cargo.toml`), runtime
imports only, as the graph answers by default (ADR-0047 decision 4):

- `mod name;` declares the module's file: `name.rs` or `name/mod.rs` in the declaring module's
  directory (the file's own directory for a crate root or `mod.rs`, else `<dir>/<stem>/`), under
  any inline modules it sits in; `#[path = "..."]` is followed. The declaration builds the module
  tree but is containment, not use: it is an edge only with `module_decls` (the CLI's
  `--include-module-decls`).
- `use crate::a::b::C` is an edge to the file of the longest module prefix (`a/b.rs`, else `a.rs`,
  else the crate root). `self::` and `super::` are resolved from the module the `use` sits in, and
  a path starting with a child module of that module (2018 uniform paths) likewise.
- A path starting at a crate name is classified by the importer's package manifest: `std`, `core`,
  `alloc`, `proc_macro`, `test` and registry or git dependencies are external (no edge). The
  package's own library (`use tokio::x` in `tokio/tests/*.rs`), a path dependency inside the
  repository, a `workspace = true` dependency whose `[workspace.dependencies]` entry has a path,
  and a dependency the workspace `[patch.*]`es to a path are crates of the repository: the path
  resolves like `crate::` in that crate's library (`[lib] path`, else `src/lib.rs`). Dependencies
  are keyed by the name code uses (the key, `-` read as `_`); the workspace is the nearest
  `Cargo.toml` at or above the package with a `[workspace]` table. Any other name is no edge.
- The module tree is walked from every crate root of every package (`src/lib.rs` or the
  `[lib] path`, `src/main.rs`, `src/bin/*`, `build.rs`, `tests/*`, `examples/*`, `benches/*`, and
  every target `path` the manifest declares); a file no root reaches has no module path and its
  `use` lines resolve to nothing.

Python (from `ast`):

- A file's module name is its path without `.py` (`__init__` names the package), from the repository
  root and from `src/`.
- `import a.b.c` is an edge to the deepest existing module of `a.b.c`; `from a.b import c` to `a.b.c`
  when that is a module, else to `a.b`. Relative imports resolve from the importer's package.
  `if TYPE_CHECKING:` imports count like any other.

TypeScript (from the source text, no compiler):

- Only relative specifiers (`./x`, `../x`) are edges; a bare specifier is a package, external to the
  graph. The specifier sits in `import ... from`, `export ... from` (a barrel's `export * from`
  and `export {a} from`), a side-effect `import '...'`, `import('...')` or `require('...')`; text
  in comments is ignored.
- A specifier resolves like the compiler's: the file itself, else the same path with `.js` /
  `.jsx` / `.mjs` / `.cjs` read as the `.ts` / `.tsx` / `.mts` / `.cts` source beside it, else the
  path plus an extension, else the directory's `index` file. An edge goes to the file the specifier
  names, so an importer of a barrel points at the barrel, and the barrel points at what it
  re-exports; no edge skips over the barrel.

Both: an edge to the importer itself is dropped, and each `(importer, target)` pair is kept once
at its first line. `#[cfg]` and other conditions are ignored.
"""

from __future__ import annotations

import ast
import bisect
import posixpath
import re
import tomllib
from collections import defaultdict, deque
from collections.abc import Callable, Iterable
from dataclasses import dataclass, field

import _common as c

Edges = dict[str, dict[str, int]]  # importer -> {target: first import line}

STD_CRATES = ("std", "core", "alloc", "proc_macro", "test")
DEP_SECTIONS = ("dependencies", "dev-dependencies", "build-dependencies")


@dataclass
class Dependency:
    package: str
    # `path`: the repo-relative package directory; `outside`: a path leaving the repository;
    # `workspace`: `workspace = true`; `registry`: anything else.
    source: str
    path: str | None = None
    key: str = ""


@dataclass
class Manifest:
    is_package: bool = False
    lib_name: str | None = None
    lib_path: str | None = None
    targets: list[str] = field(default_factory=list)
    dependencies: dict[str, Dependency] = field(default_factory=dict)
    is_workspace: bool = False
    workspace_dependencies: dict[str, Dependency] = field(default_factory=dict)
    patches: dict[str, str] = field(default_factory=dict)


def _normalize(path: str) -> str | None:
    out: list[str] = []
    for seg in path.split("/"):
        if seg in ("", "."):
            continue
        if seg == "..":
            if not out:
                return None
            out.pop()
        else:
            out.append(seg)
    return "/".join(out)


def _dependency(key: str, spec: object, pkg_dir: str) -> Dependency:
    spec = spec if isinstance(spec, dict) else {}
    package = spec.get("package", key)
    if isinstance(spec.get("path"), str):
        target = _normalize(posixpath.join(pkg_dir, spec["path"]))
        return Dependency(package, "outside" if target is None else "path", target, key)
    if spec.get("workspace") is True:
        return Dependency(package, "workspace", None, key)
    return Dependency(package, "registry", None, key)


def parse_manifest(text: str, pkg_dir: str) -> Manifest:
    """What the gold reads from one `Cargo.toml` in `pkg_dir`; one that does not parse declares nothing."""
    try:
        doc = tomllib.loads(text)
    except tomllib.TOMLDecodeError:
        return Manifest()

    def resolve(path: object) -> str | None:
        return _normalize(posixpath.join(pkg_dir, path)) if isinstance(path, str) else None

    lib = doc.get("lib") if isinstance(doc.get("lib"), dict) else {}
    package = doc.get("package") if isinstance(doc.get("package"), dict) else None
    m = Manifest(is_package=package is not None, lib_path=resolve(lib.get("path")))
    name = lib.get("name") or (package or {}).get("name")
    m.lib_name = name.replace("-", "_") if isinstance(name, str) else None
    m.targets = [t for t in [m.lib_path, resolve((package or {}).get("build"))] if t]
    for kind in ("bin", "test", "example", "bench"):
        for target in doc.get(kind) or []:
            if isinstance(target, dict) and (t := resolve(target.get("path"))):
                m.targets.append(t)
    tables = [doc.get(s) for s in DEP_SECTIONS]
    for platform in (doc.get("target") or {}).values():
        if isinstance(platform, dict):
            tables.extend(platform.get(s) for s in DEP_SECTIONS)
    for table in tables:
        for key, spec in (table if isinstance(table, dict) else {}).items():
            m.dependencies[key.replace("-", "_")] = _dependency(key, spec, pkg_dir)
    workspace = doc.get("workspace")
    if isinstance(workspace, dict):
        m.is_workspace = True
        for key, spec in (workspace.get("dependencies") or {}).items():
            m.workspace_dependencies[key] = _dependency(key, spec, pkg_dir)
        for registry in (doc.get("patch") or {}).values():
            for key, spec in (registry if isinstance(registry, dict) else {}).items():
                dep = _dependency(key, spec, pkg_dir)
                if dep.source == "path" and dep.path is not None:
                    m.patches[dep.package] = dep.path
    return m


def cargo_manifests(repo: c.Repo) -> dict[str, str]:
    """Every tracked `Cargo.toml` at HEAD: path -> text."""
    return dict(repo.blobs([f for f in repo.all_files() if posixpath.basename(f) == "Cargo.toml"]))


def lib_names(manifests: dict[str, str]) -> dict[str, str]:
    """The library root file of every package with a library name -> that name as code uses it."""
    parsed = {posixpath.dirname(p): parse_manifest(text, posixpath.dirname(p)) for p, text in manifests.items()}
    return {_lib_root(d, parsed): m.lib_name for d, m in sorted(parsed.items()) if m.lib_name}


def _local_crate(name: str, pkg_dir: str, manifests: dict[str, Manifest]) -> str | None:
    """The package directory of the repository crate `name` names for code of `pkg_dir`, or None
    when it is the standard library, an external dependency or no declared crate."""
    m = manifests.get(pkg_dir)
    if m is None or name in STD_CRATES:
        return None
    if m.lib_name == name:
        return pkg_dir
    dep = m.dependencies.get(name)
    if dep is None:
        return None
    workspace = _workspace(pkg_dir, manifests)
    source, path = dep.source, dep.path
    declared = workspace.workspace_dependencies.get(dep.key) if workspace and source == "workspace" else None
    if declared is not None:
        source, path = declared.source, declared.path
    if source == "path":
        return path
    if source in ("registry", "workspace") and workspace is not None:
        return workspace.patches.get(dep.package)
    return None


def _workspace(pkg_dir: str, manifests: dict[str, Manifest]) -> Manifest | None:
    d = pkg_dir
    while True:
        m = manifests.get(d)
        if m is not None and m.is_workspace:
            return m
        if not d:
            return None
        d = posixpath.dirname(d)


def _add(edges: Edges, src: str, dst: str | None, line: int) -> None:
    if dst is None or dst == src:
        return
    targets = edges.setdefault(src, {})
    if dst not in targets or line < targets[dst]:
        targets[dst] = line


def _lib_root(pkg: str, manifests: dict[str, Manifest]) -> str:
    m = manifests.get(pkg)
    return (m.lib_path if m and m.lib_path else None) or posixpath.join(pkg, "src", "lib.rs")


def _crate_roots(files: set[str], manifests: dict[str, Manifest]) -> dict[str, str]:
    """Every crate root file of every package with a tracked `Cargo.toml` -> its package directory,
    the nearest one when packages nest. A virtual manifest (no `[package]`) has no targets."""
    packages = sorted(d for d, m in manifests.items() if m.is_package)
    owner: dict[str, str] = {}
    for pkg in packages:
        roots: set[str] = set()
        main_files = (_lib_root(pkg, manifests), posixpath.join(pkg, "src", "main.rs"))
        roots.update(f for f in (*main_files, posixpath.join(pkg, "build.rs")) if f in files)
        m = manifests.get(pkg)
        roots.update(t for t in (m.targets if m else []) if t in files)
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
        for root in roots:
            if len(pkg) >= len(owner.get(root, "")):
                owner[root] = pkg
    return dict(sorted(owner.items()))


def _mod_dir(path: str, is_root: bool) -> str:
    base = posixpath.basename(path)
    if is_root or base == "mod.rs":
        return posixpath.dirname(path)
    return posixpath.join(posixpath.dirname(path), base[: -len(".rs")])


def rust_edges(
    files: Iterable[str], recs: list[dict], manifests: dict[str, str], *, module_decls: bool = False
) -> Edges:
    """Edges among the Rust files of `files` (every tracked path, `Cargo.toml` included);
    `manifests` maps each `Cargo.toml` path to its text."""
    return rust_graph(files, recs, manifests, module_decls=module_decls)[0]


def rust_graph(
    files: Iterable[str], recs: list[dict], manifests: dict[str, str], *, module_decls: bool = False
) -> tuple[Edges, dict[str, dict[tuple[str, ...], str]]]:
    """The edges, and per crate root the file of every module path its module tree reaches."""
    files = set(files)
    parsed = {posixpath.dirname(p): parse_manifest(text, posixpath.dirname(p)) for p, text in manifests.items()}
    roots = _crate_roots(files, parsed)
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
    for root in roots:
        table = {(): root}
        module_file[root] = table
        owners[root].append((root, ()))
        queue = deque([(root, ())])
        # A module path can have one file per `#[cfg]` (`#[cfg(unix)] #[path = "unix.rs"] mod imp;`
        # beside a windows one): conditions are ignored, so every such file is walked; paths
        # resolve to the first.
        walked = {(root, ())}
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
                if module_decls:
                    _add(edges, path, child, m["line"])
                child_path = (*modpath, *scope, m["name"])
                table.setdefault(child_path, child)
                if (child, child_path) not in walked:
                    walked.add((child, child_path))
                    owners[child].append((root, child_path))
                    queue.append((child, child_path))

    for path, recs_here in uses.items():
        for root, modpath in owners.get(path, []):
            table = module_file[root]
            pkg = roots[root]

            def crate_table(name: str, pkg: str = pkg) -> dict[tuple[str, ...], str] | None:
                local = _local_crate(name, pkg, parsed)
                return None if local is None else module_file.get(_lib_root(local, parsed))

            for u in recs_here:
                target = _resolve_use(u, modpath, table, crate_table)
                _add(edges, path, target, u["line"])
    return edges, module_file


def _resolve_use(
    u: dict,
    modpath: tuple[str, ...],
    table: dict[tuple[str, ...], str],
    crate_table: Callable[[str], dict[tuple[str, ...], str] | None],
) -> str | None:
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
    elif (other := crate_table(segs[0])) is not None:
        table, absolute = other, segs[1:]
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


TS_EXTENSIONS = (".ts", ".tsx", ".mts", ".cts")
TS_JS_TO_SOURCE = {".js": (".ts", ".tsx"), ".jsx": (".tsx",), ".mjs": (".mts",), ".cjs": (".cts",)}
TS_SPECIFIER = re.compile(r"""(?:\bfrom\s*|\bimport\s*\(?\s*|\brequire\s*\(\s*)(['"])(\.{1,2}(?:/[^'"\n]*)?)\1""")


def _ts_resolve(importer: str, specifier: str, files: set[str]) -> str | None:
    """The file `specifier` names from `importer`, by the compiler's rules for a relative path."""
    base = posixpath.normpath(posixpath.join(posixpath.dirname(importer), specifier))
    if base == ".." or base.startswith("../"):
        return None
    stem, ext = posixpath.splitext(base)
    candidates = [base]
    candidates += [stem + alt for alt in TS_JS_TO_SOURCE.get(ext, ())]
    candidates += [base + e for e in TS_EXTENSIONS]
    candidates += [f"{base}/index{e}" for e in TS_EXTENSIONS]
    return next((cand for cand in candidates if cand in files), None)


def _ts_code(src: str) -> tuple[str, list[int], list[int]]:
    """`src` with comments and template-literal text blanked (newlines kept, so a line number stays the
    file's), and the start and end offsets of each ordinary string literal, which stay in the text.

    A scanner, not a parser: a `/` that opens a regular-expression literal is read as a division, so a
    quote inside one is taken for a string."""
    out = list(src)
    starts: list[int] = []
    ends: list[int] = []
    n = len(src)
    stack: list[str] = []  # "tpl" for the `${` of a template, "brace" for any other `{`

    def blank(a: int, b: int) -> None:
        for k in range(a, min(b, n)):
            if out[k] != "\n":
                out[k] = " "

    def template(start: int) -> int:
        """Blank the template text from `start` (a backtick or the `}` closing a `${`); the offset after it."""
        i = start + 1
        while i < n:
            if src[i] == "\\":
                i += 2
            elif src[i] == "`":
                blank(start, i + 1)
                return i + 1
            elif src.startswith("${", i):
                blank(start, i + 2)
                stack.append("tpl")
                return i + 2
            else:
                i += 1
        blank(start, n)
        return n

    i = 0
    while i < n:
        ch = src[i]
        if src.startswith("//", i):
            end = src.find("\n", i)
            end = n if end < 0 else end
            blank(i, end)
            i = end
        elif src.startswith("/*", i):
            end = src.find("*/", i + 2)
            end = n if end < 0 else end + 2
            blank(i, end)
            i = end
        elif ch in "'\"":
            j = i + 1
            while j < n and src[j] not in (ch, "\n"):
                j += 2 if src[j] == "\\" else 1
            j = min(j + 1, n)
            starts.append(i)
            ends.append(j)
            i = j
        elif ch == "`":
            i = template(i)
        elif ch == "{":
            stack.append("brace")
            i += 1
        elif ch == "}" and stack:
            i = template(i) if stack.pop() == "tpl" else i + 1
        else:
            i += 1
    return "".join(out), starts, ends


def typescript_edges(blobs: Iterable[tuple[str, str]]) -> Edges:
    """Edges among the TypeScript files of `blobs` (`(path, source)` pairs)."""
    sources = {p: s for p, s in blobs if p.endswith(TS_EXTENSIONS)}
    files = set(sources)
    edges: Edges = {}
    for path, src in sorted(sources.items()):
        text, starts, ends = _ts_code(src)
        for m in TS_SPECIFIER.finditer(text):
            at = bisect.bisect_right(starts, m.start()) - 1
            if at >= 0 and m.start() < ends[at]:
                continue
            _add(edges, path, _ts_resolve(path, m.group(2), files), text.count("\n", 0, m.start()) + 1)
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
