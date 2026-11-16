"""Per-tool ADR-0022 verdict across corpora, from the `<corpus>.report.json` / `<corpus>.ndjson` pairs
`run_gate.sh` writes.

    python3 benchmarks/eval/gate.py RESULTS_DIR

A tool passes when every one of its task kinds passes on every corpus (the per-kind verdict is the
one `basemind admin eval` computed: credited >= 0.7 of the kind's tasks, median baseline/tool
token ratio >= 1.5 over the credited ones, o200k tokens). A failing kind fails the tool; a kind
with no verdict (no tasks, or tokens not counted as o200k) leaves the tool without one.
"""

from __future__ import annotations

import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path

# ADR-0022's tool table plus the kinds this harness adds. Exempt tools have no shell equivalent.
TOOLS = {
    "code_symbols": ["symbols"],
    "code_outline": ["outline"],
    "code_read": ["read"],
    "code_grep": ["grep"],
    "code_search": [],
    "code_refs": ["references", "callers", "implementations", "dependents"],
    "find_files": ["find"],
    "docs_search": ["docs"],
    "git_log": ["git_search", "git_path", "git_symbol"],
    "git_blame": ["blame", "blame_symbol"],
    "git_diff": ["diff", "diff_structural"],
}
EXEMPT = ["memory_save", "memory_search", "index_status", "index_rescan"]
NOT_RUN = {
    "code_search": "needs the `code-search` build and an embedding model download (network is limited to cloning)",
    "docs_search": "needs the `documents` build and an embedding model download (network is limited to cloning)",
}


def load(results: Path) -> dict[str, tuple[dict, list[dict]]]:
    out = {}
    for rep in sorted(results.glob("*.report.json")):
        corpus = rep.name.removesuffix(".report.json")
        rows = [json.loads(line) for line in (results / f"{corpus}.ndjson").read_text().splitlines() if line]
        out[corpus] = (json.loads(rep.read_text()), rows)
    return out


def kind_row(report: dict, rows: list[dict], kind: str) -> dict | None:
    gate = report.get("gate", {}).get(kind)
    if gate is None:
        return None
    mode = report["modes"][kind]
    mine = [r for r in rows if r["mode"] == kind]
    base = [r["savings"]["baseline_tokens"] for r in mine if r.get("savings")]
    return {
        "tasks": gate["tasks"],
        "errors": mode["errors"],
        "credited": gate["credited"],
        "credited_fraction": gate["credited_fraction"],
        "median_ratio": gate["median_ratio"],
        "tool_recall": mode["mean_recall"],
        "tool_precision": mode["mean_precision"],
        "baseline_recall": mode.get("baseline_mean_recall"),
        "tool_tokens_median": statistics.median([r["tokens"] for r in mine]) if mine else None,
        "baseline_tokens_median": statistics.median(base) if base else None,
        "symbol_accuracy": mode.get("mean_annotation_accuracy"),
        "pass": gate["pass"],
        "reason": gate["reason"],
    }


def verdicts(data: dict[str, tuple[dict, list[dict]]]) -> dict:
    tools: dict[str, dict] = {}
    for tool, kinds in TOOLS.items():
        per: dict[str, dict] = defaultdict(dict)
        for corpus, (report, rows) in data.items():
            for kind in kinds:
                row = kind_row(report, rows, kind)
                if row is not None:
                    per[kind][corpus] = row
        passes = [row["pass"] for kind in kinds for row in per[kind].values()]
        # `kind@corpus` pairs with no result, as one string so gate.json stays free of arrays.
        missing = ", ".join(f"{k}@{corpus}" for k in kinds for corpus in sorted(data) if corpus not in per[k])
        if not kinds or not passes:
            verdict = "not run"
        elif any(p is False for p in passes):
            verdict = "FAIL"
        elif missing or any(p is None for p in passes):
            verdict = "no verdict"
        else:
            verdict = "PASS"
        tools[tool] = {"verdict": verdict, "kinds": per, "missing": missing}
    return tools


def fmt(x, spec: str = ".2f") -> str:
    return "-" if x is None else format(x, spec)


def markdown(data: dict, tools: dict) -> str:
    corpora = sorted(data)
    tokenizers = {c: data[c][0]["tokenizer"] for c in corpora}
    lines = [
        "# Value gate (ADR-0022)",
        "",
        f"Corpora: {', '.join(f'`{c}`' for c in corpora)}; tokenizer: "
        + ", ".join(f"{c} `{t}`" for c, t in tokenizers.items())
        + ". A kind passes at credited >= 0.70 and median >= 1.50x.",
        "",
        "| tool | verdict |",
        "|---|---|",
    ]
    for tool, v in tools.items():
        why = NOT_RUN.get(tool, "") if v["verdict"] == "not run" else ""
        lines.append(f"| `{tool}` | {v['verdict']}{' (' + why + ')' if why else ''} |")
    lines.append(f"| {', '.join(f'`{t}`' for t in EXEMPT)} | exempt (ADR-0022) |")
    lines += [
        "",
        "Per kind and corpus. `R` is mean recall against the gold, `P` mean precision; tokens are",
        "medians over all tasks of the kind; `x` is the median baseline/tool ratio over the credited tasks.",
        "",
        "| tool | kind | corpus | tasks | err | credited | tool R | tool P | baseline R | tool tok | baseline tok | x"
        " | symbol acc | verdict |",
        "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|",
    ]
    for tool, v in tools.items():
        for kind in TOOLS[tool]:
            for corpus in corpora:
                row = v["kinds"].get(kind, {}).get(corpus)
                if row is None:
                    continue
                verdict = {True: "PASS", False: "FAIL", None: "n/a"}[row["pass"]]
                if row["reason"]:
                    verdict += f" ({row['reason']})"
                lines.append(
                    f"| `{tool}` | {kind} | {corpus} | {row['tasks']} | {row['errors']} | {row['credited']}"
                    f" ({row['credited_fraction']:.2f}) | {fmt(row['tool_recall'], '.3f')}"
                    f" | {fmt(row['tool_precision'], '.3f')} | {fmt(row['baseline_recall'], '.3f')}"
                    f" | {fmt(row['tool_tokens_median'], '.0f')} | {fmt(row['baseline_tokens_median'], '.0f')}"
                    f" | {fmt(row['median_ratio'])} | {fmt(row['symbol_accuracy'], '.3f')} | {verdict} |"
                )
    return "\n".join(lines) + "\n"


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit("usage: gate.py RESULTS_DIR")
    results = Path(sys.argv[1])
    data = load(results)
    if not data:
        sys.exit(f"no *.report.json in {results}")
    tools = verdicts(data)
    (results / "gate.json").write_text(json.dumps(tools, indent=2, sort_keys=True) + "\n")
    sys.stdout.write(markdown(data, tools))


if __name__ == "__main__":
    main()
