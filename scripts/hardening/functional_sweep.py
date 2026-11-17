#!/usr/bin/env python3
"""One call per MCP verb tool and the matching CLI command against a generated fixture repository
with a planted document set, asserting structural fields and that document RAG returns the planted
fact for a planted question, with and without filters.

Skips are explicit (a tool answering that its Cargo feature is not compiled in) and counted; a run
that checked nothing fails.

Usage:
    functional_sweep.py --binary target/release/basemind [--out DIR]
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import (  # noqa: E402
    Isolation,
    McpError,
    McpSession,
    binary_version,
    default_out_root,
    install_cleanup,
    log,
    owned_processes,
    run_cli,
    terminate_owned,
)
from fixtures import FACTS, build_fixture  # noqa: E402

Check = Callable[[Any], str | None]


def items(body: Any, *keys: str) -> list[Any]:
    """The first list found under one of `keys`."""
    if not isinstance(body, dict):
        return []
    for k in keys:
        v = body.get(k)
        if isinstance(v, list):
            return v
    return []


def blob(x: Any) -> str:
    return json.dumps(x, ensure_ascii=False)


def need(cond: bool, msg: str) -> str | None:
    return None if cond else msg


@dataclass
class Case:
    name: str
    tool: str
    args: dict[str, Any]
    cli: list[str] | None
    check: Check
    feature: str | None = None


def file_listed(path: str) -> Check:
    return lambda b: need(any(f.get("path") == path for f in items(b, "files")), f"{path} not in files")


def outline_has(name: str) -> Check:
    def check(b: Any) -> str | None:
        syms = items(b, "symbols")
        hit = next((s for s in syms if s.get("name") == name), None)
        if hit is None:
            return f"{name} not in outline ({len(syms)} symbols)"
        return need(isinstance(hit.get("line"), int) and hit["line"] >= 1, f"{name} has no 1-based line: {hit}")

    return check


def any_row_mentions(keys: tuple[str, ...], text: str, minimum: int = 1) -> Check:
    def check(b: Any) -> str | None:
        rows = items(b, *keys)
        n = sum(1 for r in rows if text in blob(r))
        return need(n >= minimum, f"{n} rows under {keys} mention {text!r} (need >= {minimum}); body {blob(b)[:300]}")

    return check


def body_mentions(text: str) -> Check:
    return lambda b: need(text in blob(b), f"{text!r} not in body {blob(b)[:300]}")


def status_ok(b: Any) -> str | None:
    return need(
        isinstance(b, dict) and ("files" in blob(b) or "languages" in blob(b)), f"no file counts in {blob(b)[:300]}"
    )


def doc_hit(fact_path: str, answer: str) -> Check:
    def check(b: Any) -> str | None:
        rows = items(b, "results", "hits")
        if not rows:
            return f"no results; body {blob(b)[:300]}"
        mine = [r for r in rows if fact_path in blob(r)]
        if not mine:
            return f"{fact_path} not among {len(rows)} results: {[r.get('path') or r.get('source') for r in rows]}"
        return need(
            any(answer.lower() in blob(r).lower() for r in mine),
            f"{fact_path} hit lacks {answer!r}: {blob(mine[0])[:300]}",
        )

    return check


def doc_absent(fact_path: str) -> Check:
    return lambda b: need(
        fact_path not in blob(items(b, "results", "hits")), f"{fact_path} returned despite a filter excluding it"
    )


def cases() -> list[Case]:
    c = [
        Case("status", "index_status", {}, ["status"], status_ok),
        Case("find_files listing", "find_files", {}, ["code", "find"], file_listed("src/lib.rs")),
        Case(
            "find_files query",
            "find_files",
            {"query": "settings"},
            ["code", "find", "settings"],
            file_listed("src/settings.py"),
        ),
        Case(
            "code_outline",
            "code_outline",
            {"path": "src/lib.rs"},
            ["code", "outline", "src/lib.rs"],
            outline_has("compute_total"),
        ),
        Case(
            "code_symbols",
            "code_symbols",
            {"name": "compute_total"},
            ["code", "symbols", "compute_total"],
            any_row_mentions(("results", "symbols", "hits"), "src/lib.rs"),
        ),
        Case(
            "code_symbols fuzzy",
            "code_symbols",
            {"name": "cmptotal", "fuzzy": True},
            ["code", "symbols", "cmptotal", "--fuzzy"],
            any_row_mentions(("results", "symbols", "hits"), "compute_total"),
        ),
        Case(
            "code_read symbol",
            "code_read",
            {"path": "src/lib.rs", "symbol": "compute_total"},
            ["code", "read", "src/lib.rs", "--symbol", "compute_total"],
            body_mentions("total_reading(&[&a, &b])"),
        ),
        Case(
            "code_read span",
            "code_read",
            {"path": "src/settings.py", "line": 7, "end_line": 9},
            ["code", "read", "src/settings.py", "--line", "7", "--end-line", "9"],
            body_mentions("def load_settings"),
        ),
        Case(
            "code_grep literal",
            "code_grep",
            {"pattern": "total_reading"},
            ["code", "grep", "total_reading"],
            any_row_mentions(("hits",), "total_reading", 2),
        ),
        Case(
            "code_grep regex",
            "code_grep",
            {"pattern": r"^def \w+", "regex": True, "language": "python"},
            ["code", "grep", r"^def \w+", "--regex", "--language", "python"],
            any_row_mentions(("hits",), "settings.py", 2),
        ),
        Case(
            "code_refs callers",
            "code_refs",
            {"name": "total_reading"},
            ["code", "refs", "total_reading"],
            any_row_mentions(("results", "hits", "callers"), "src/lib.rs"),
        ),
        Case(
            "code_refs implementations",
            "code_refs",
            {"name": "Gauge", "relation": "implementations"},
            ["code", "refs", "Gauge", "--relation", "implementations"],
            body_mentions("FixedGauge"),
        ),
        Case(
            "code_refs importers",
            "code_refs",
            {"name": "json", "relation": "importers"},
            ["code", "refs", "json", "--relation", "importers"],
            body_mentions("src/settings.py"),
        ),
        Case(
            "code_refs definition",
            "code_refs",
            {"name": "total_reading", "relation": "definition", "path": "src/lib.rs", "line": 28},
            ["code", "refs", "total_reading", "--relation", "definition", "--path", "src/lib.rs", "--line", "28"],
            body_mentions("src/lib.rs"),
        ),
        Case(
            "code_search",
            "code_search",
            {"query": "sum the readings of several gauges"},
            ["code", "search", "sum the readings of several gauges"],
            body_mentions("total_reading"),
            "code-search",
        ),
        Case("git_log recent", "git_log", {}, ["git", "log"], any_row_mentions(("commits",), "fix: add double_total")),
        Case(
            "git_log query",
            "git_log",
            {"query": "double_total"},
            ["git", "log", "--query", "double_total"],
            any_row_mentions(("commits",), "double_total"),
        ),
        Case(
            "git_log path",
            "git_log",
            {"path": "src/lib.rs"},
            ["git", "log", "--path", "src/lib.rs"],
            any_row_mentions(("commits",), "initial fixture"),
        ),
        Case(
            "git_log symbol",
            "git_log",
            {"path": "src/lib.rs", "symbol": "double_total"},
            ["git", "log", "--path", "src/lib.rs", "--symbol", "double_total"],
            body_mentions("double_total"),
        ),
        Case("git_log rev", "git_log", {"rev": "HEAD"}, ["git", "log", "--rev", "HEAD"], body_mentions("src/lib.rs")),
        Case(
            "git_blame span",
            "git_blame",
            {"path": "src/lib.rs", "line": 1, "end_line": 5},
            ["git", "blame", "src/lib.rs", "--line", "1", "--end-line", "5"],
            body_mentions("Hardening"),
        ),
        Case(
            "git_blame symbol",
            "git_blame",
            {"path": "src/lib.rs", "symbol": "double_total"},
            ["git", "blame", "src/lib.rs", "--symbol", "double_total"],
            body_mentions("Hardening"),
        ),
        Case("git_diff status", "git_diff", {}, ["git", "diff"], body_mentions("src/settings.py")),
        Case(
            "git_diff path",
            "git_diff",
            {"path": "src/settings.py"},
            ["git", "diff", "src/settings.py"],
            body_mentions("sweep_marker"),
        ),
        Case(
            "git_diff structural",
            "git_diff",
            {"path": "src/lib.rs", "structural": True, "from": "HEAD~1"},
            ["git", "diff", "src/lib.rs", "--structural", "--from", "HEAD~1"],
            body_mentions("double_total"),
        ),
        Case(
            "memory_save",
            "memory_save",
            {"key": "sweep/fact", "value": "the sweep stores the Ostrander checksum", "tags": ["sweep"]},
            ["memory", "put", "sweep/fact-cli", "the sweep stores the Ostrander checksum", "--tag", "sweep"],
            lambda b: need(isinstance(b, dict), f"no body: {b}"),
            "memory",
        ),
        Case(
            "memory_search key",
            "memory_search",
            {"key": "sweep/fact"},
            ["memory", "get", "sweep/fact"],
            body_mentions("Ostrander"),
            "memory",
        ),
        Case(
            "memory_search query",
            "memory_search",
            {"query": "Ostrander checksum"},
            ["memory", "search", "Ostrander checksum"],
            body_mentions("sweep/fact"),
            "memory",
        ),
        Case(
            "memory_search prefix",
            "memory_search",
            {"prefix": "sweep/"},
            ["memory", "list", "--prefix", "sweep/"],
            body_mentions("sweep/fact"),
            "memory",
        ),
    ]
    for fact in FACTS:
        stem = Path(fact.path).name
        c.append(
            Case(
                f"docs_search {stem}",
                "docs_search",
                {"query": fact.question, "limit": 5},
                ["memory", "documents", fact.question, "--limit", "5"],
                doc_hit(fact.path, fact.answer),
                "documents",
            )
        )
    pdf = next(f for f in FACTS if f.path.endswith(".pdf"))
    c += [
        Case(
            "docs_search mime filter",
            "docs_search",
            {"query": pdf.question, "mime_type": "application/pdf"},
            ["memory", "documents", pdf.question, "--mime-type", "application/pdf"],
            doc_hit(pdf.path, pdf.answer),
            "documents",
        ),
        Case(
            "docs_search mime excludes",
            "docs_search",
            {"query": pdf.question, "mime_type": "text/html"},
            ["memory", "documents", pdf.question, "--mime-type", "text/html"],
            doc_absent(pdf.path),
            "documents",
        ),
    ]
    c += [
        Case(
            "index_rescan paths",
            "index_rescan",
            {"paths": ["src/settings.py"]},
            ["rescan", "src/settings.py"],
            lambda b: need(b is not None, "no body"),
            None,
        ),
        Case("index_rescan all", "index_rescan", {}, ["rescan"], lambda b: need(b is not None, "no body"), None),
    ]
    return c


@dataclass
class Outcome:
    case: str
    surface: str
    status: str
    detail: str = ""
    secs: float = 0.0


def feature_missing_text(text: str) -> bool:
    low = text.lower()
    return "cargo feature" in low and "not compiled" in low


def run_mcp(sess: McpSession, case: Case) -> Outcome:
    started = time.monotonic()
    try:
        res = sess.call(case.tool, case.args, timeout=900)
    except McpError as exc:
        return Outcome(case.name, "mcp", "fail", f"transport: {exc}", time.monotonic() - started)
    if res.is_error and feature_missing_text(res.text):
        return Outcome(case.name, "mcp", "skip", f"feature `{case.feature}` not in this build", res.secs)
    if res.is_error:
        return Outcome(case.name, "mcp", "fail", f"isError: {res.text[:400]}", res.secs)
    problem = case.check(res.body if res.body is not None else {"text": res.text})
    return Outcome(case.name, "mcp", "fail" if problem else "pass", problem or "", res.secs)


def run_cli_case(binary: Path, repo: Path, env: dict[str, str], case: Case) -> Outcome:
    if case.cli is None:
        return Outcome(case.name, "cli", "skip", "no CLI command")
    started = time.monotonic()
    out = run_cli(binary, ["--json", *case.cli], repo, env, timeout=900)
    secs = time.monotonic() - started
    if out.returncode != 0:
        if feature_missing_text(out.stderr + out.stdout):
            return Outcome(case.name, "cli", "skip", f"feature `{case.feature}` not in this build", secs)
        return Outcome(case.name, "cli", "fail", f"exit {out.returncode}: {(out.stderr or out.stdout)[-400:]}", secs)
    try:
        body: Any = json.loads(out.stdout)
    except json.JSONDecodeError:
        body = {"text": out.stdout}
    problem = case.check(body)
    return Outcome(case.name, "cli", "fail" if problem else "pass", problem or "", secs)


def wait_for_documents(sess: McpSession, timeout: float) -> str:
    """Documents embed in the background after a scan; poll docs_search until one planted fact answers."""
    fact = FACTS[0]
    deadline = time.monotonic() + timeout
    last = ""
    while time.monotonic() < deadline:
        try:
            res = sess.call("docs_search", {"query": fact.question, "limit": 5}, timeout=600)
        except McpError as exc:
            return f"transport: {exc}"
        if res.is_error and feature_missing_text(res.text):
            return "feature missing"
        last = res.text[:200]
        if not res.is_error and fact.path in res.text:
            return "ready"
        time.sleep(5)
    return f"not ready after {timeout}s: {last}"


def main() -> int:  # noqa: PLR0915
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--binary", type=Path, required=True)
    ap.add_argument("--out", type=Path)
    ap.add_argument(
        "--docs-timeout", type=float, default=600, help="seconds to wait for documents to become searchable"
    )
    ap.add_argument(
        "--sabotage",
        action="store_true",
        help="rename compute_total and drop the planted Markdown before the scan: the sweep must fail",
    )
    args = ap.parse_args()
    binary = args.binary.resolve()
    out = args.out or default_out_root() / time.strftime("sweep-%Y%m%d-%H%M%S")
    out.mkdir(parents=True, exist_ok=True)
    iso = Isolation("sweep")
    sess_holder: list[McpSession] = []

    def cleanup() -> None:
        for s in sess_holder:
            s.kill()
        terminate_owned(iso.root)
        iso.cleanup()

    install_cleanup(cleanup)
    env = iso.env()
    repo = build_fixture(iso.root / "repo")
    (repo / "src" / "settings.py").write_text((repo / "src" / "settings.py").read_text() + "\nsweep_marker = 1\n")
    if args.sabotage:
        lib = repo / "src" / "lib.rs"
        lib.write_text(lib.read_text().replace("compute_total", "compute_sum"))
        (repo / "docs" / "handbook.md").unlink()
    version = binary_version(binary, iso)
    log(f"{version}: fixture at {repo}")

    scan = run_cli(binary, ["scan"], repo, env, timeout=1800)
    (out / "scan.log").write_text(scan.stdout + "\n--- stderr ---\n" + scan.stderr)
    outcomes: list[Outcome] = [
        Outcome(
            "initial scan",
            "cli",
            "pass" if scan.returncode == 0 else "fail",
            "" if scan.returncode == 0 else scan.stderr[-400:],
        )
    ]

    sess = McpSession(binary, repo, env, out / "serve-stderr.log", "sweep")
    sess_holder.append(sess)
    sess.initialize(timeout=300)
    docs_state = wait_for_documents(sess, args.docs_timeout)
    log(f"documents: {docs_state}")

    all_cases = cases()
    for case in all_cases:
        o = run_mcp(sess, case)
        outcomes.append(o)
        log(f"mcp {o.status:4} {case.name} {o.detail[:160]}")
    for case in all_cases:
        o = run_cli_case(binary, repo, env, case)
        outcomes.append(o)
        log(f"cli {o.status:4} {case.name} {o.detail[:160]}")

    tools_called = {c.tool for c in all_cases}
    uncovered = sorted(set(sess.tools) - tools_called - {"agents"})
    sess.close()
    procs = owned_processes(iso.root)

    passed = sum(o.status == "pass" for o in outcomes)
    failed = [o for o in outcomes if o.status == "fail"]
    skipped = [o for o in outcomes if o.status == "skip"]
    ok = passed > 0 and not failed and not uncovered
    lines = [
        f"# Functional sweep: {'PASS' if ok else 'FAIL'}",
        "",
        f"- binary: `{binary}` ({version})",
        f"- checks: {len(outcomes)}, passed {passed}, failed {len(failed)}, skipped {len(skipped)}",
        f"- documents readiness: {docs_state}",
        f"- advertised tools not exercised: {uncovered or 'none'}",
        f"- processes of the run alive at the end (daemon expected until cleanup): {len(procs)}",
        "",
        "| case | surface | result | secs | detail |",
        "|---|---|---|---|---|",
    ]
    for o in outcomes:
        lines.append(
            f"| {o.case} | {o.surface} | {o.status if o.status != 'fail' else '**fail**'} | {o.secs:.2f} | {o.detail[:300].replace('|', '/').replace(chr(10), ' ')} |"
        )
    report = "\n".join(lines) + "\n"
    (out / "sweep.report.md").write_text(report)
    sys.stdout.write(report)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
