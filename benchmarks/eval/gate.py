"""Per-tool value-gate verdict across corpora, from the `<corpus>.report.json` / `<corpus>.ndjson` pairs
`run_gate.sh` writes.

    python3 benchmarks/eval/gate.py RESULTS_DIR

A tool passes when every one of its task kinds passes on every corpus. The per-kind verdict is the
one `basemind admin eval` computed by the kind's job (ADR-0031, `src/eval/gate.rs`); this script
only groups kinds by tool, so it repeats no threshold. A failing kind fails the tool; a kind with no
verdict (no tasks, tokens not counted as o200k, nothing to compare) leaves the tool without one. A
kind a report gates that `TOOLS` does not list, or lists under another tool than the report names,
is an error: it would otherwise drop out of the verdict.
"""

from __future__ import annotations

import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path

# ADR-0022's tool table plus the kinds this harness adds, each kind under the tool it drives
# (`EvalMode::tool_key`): the 0.34 analysis kinds count against the tool whose argument they
# exercise. Exempt tools have no shell equivalent.
TOOLS = {
    "code_symbols": ["symbols"],
    "code_outline": ["outline"],
    "code_read": ["read"],
    "code_grep": ["grep", "grep_changed", "read_around"],
    "code_search": [],
    "code_refs": ["references", "callers", "implementations", "dependents", "impact", "cycles"],
    "find_files": ["find", "find_large"],
    "docs_search": ["docs"],
    "git_log": ["git_search", "git_path", "git_symbol"],
    "git_blame": ["blame", "blame_symbol"],
    "git_diff": ["diff", "diff_structural"],
    "code_explain": ["explain"],
}
TOOL_OF = {kind: tool for tool, kinds in TOOLS.items() for kind in kinds}
EXEMPT = ["memory_save", "memory_search", "index_status", "index_rescan"]
NOT_RUN = {
    "code_search": "needs the `code-search` build and an embedding model download (network is limited to cloning)",
    "docs_search": "needs the `documents` build and an embedding model download (network is limited to cloning)",
}


CODE_INTEL_PARTS = {"code-intel-js", "code-intel-stack"}
# The task-id suffix `gen_tasks.sh` gives a language's tasks in a corpus of several languages.
EXT_LANGUAGE = {"rs": "rust", "py": "python", "ts": "typescript", "go": "go", "java": "java"}
NO_LANGUAGE = "(whole repository)"
# The structural kinds a language's corpora must report (#313: symbols, references and dependents on each
# repository), whatever the other corpora report. A language with no entry has no such gold yet.
REQUIRED_KINDS = {
    "rust": {"symbols", "references", "dependents"},
    "python": {"symbols", "references", "dependents"},
}


def load_languages(corpus_tsv: Path) -> dict[str, list[str]]:
    """Corpus name -> its languages, from the sixth column of `corpus.tsv`."""
    out: dict[str, list[str]] = {}
    for line in corpus_tsv.read_text().splitlines():
        if not line or line.startswith("#"):
            continue
        cols = line.split("\t")
        if len(cols) >= 6:
            out[cols[0]] = [lang for lang in cols[5].split(",") if lang]
    return out


def row_language(task_id: str, languages: list[str]) -> str:
    """The language a task belongs to: the corpus's only one, else the id's `-<ext>` suffix."""
    if len(languages) == 1:
        return languages[0]
    return EXT_LANGUAGE.get(task_id.rsplit("-", 1)[-1], NO_LANGUAGE)


def recall_by_language(data: dict[str, tuple[dict, list[dict]]], languages: dict[str, list[str]]) -> dict:
    """language -> kind -> `{tasks, recall, baseline_recall}`: mean recall over every task of the
    kind on the corpora holding that language, an errored task counting 0. `baseline_recall` is over
    the tasks whose baseline was scored."""
    sums: dict[str, dict[str, dict[str, float]]] = defaultdict(lambda: defaultdict(lambda: defaultdict(float)))
    for corpus, (_, rows) in data.items():
        langs = languages.get(corpus, [])
        for row in rows:
            acc = sums[row_language(row.get("id", ""), langs) if langs else NO_LANGUAGE][row["mode"]]
            acc["tasks"] += 1
            acc["recall"] += (row.get("score") or {}).get("recall", 0.0)
            if row.get("baseline_score"):
                acc["baselines"] += 1
                acc["baseline_recall"] += row["baseline_score"]["recall"]
    return {
        lang: {
            kind: {
                "tasks": int(acc["tasks"]),
                "recall": acc["recall"] / acc["tasks"],
                "baseline_recall": acc["baseline_recall"] / acc["baselines"] if acc["baselines"] else None,
            }
            for kind, acc in sorted(kinds.items())
        }
        for lang, kinds in sorted(sums.items())
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
        "job": gate.get("job", "reducing"),
        "baseline_failed": gate.get("baseline_failed", 0),
        "tasks": gate["tasks"],
        "errors": mode["errors"],
        "credited": gate["credited"],
        "credited_fraction": gate["credited_fraction"],
        "median_ratio": gate["median_ratio"],
        # Over the tasks whose baseline was scored, as the recall comparison takes it.
        "tool_recall": mode["mean_recall"] if gate.get("tool_recall") is None else gate["tool_recall"],
        "tool_precision": mode["mean_precision"],
        "baseline_recall": gate.get("baseline_recall", mode.get("baseline_mean_recall")),
        "tool_tokens_median": statistics.median([r["tokens"] for r in mine]) if mine else None,
        "baseline_tokens_median": statistics.median(base) if base else None,
        "symbol_accuracy": mode.get("mean_annotation_accuracy"),
        "pass": gate["pass"],
        "reason": gate["reason"],
    }


def check_listed(data: dict[str, tuple[dict, list[dict]]]) -> None:
    """Every gated kind is listed in `TOOLS`, under the tool the report names for it."""
    for corpus, (report, _) in data.items():
        for kind, gate in report.get("gate", {}).items():
            if kind not in TOOL_OF:
                raise ValueError(f"{corpus}: kind `{kind}` is gated but listed under no tool in gate.py TOOLS")
            if gate.get("tool", TOOL_OF[kind]) != TOOL_OF[kind]:
                raise ValueError(
                    f"{corpus}: kind `{kind}` counts against `{gate['tool']}`, but TOOLS lists it under "
                    f"`{TOOL_OF[kind]}`"
                )


def verdicts(data: dict[str, tuple[dict, list[dict]]], languages: dict[str, list[str]] | None = None) -> dict:
    check_listed(data)
    languages = languages or {}
    group = {corpus: tuple(languages.get(corpus, ())) for corpus in data}
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
        missing = ", ".join(
            f"{k}@{corpus}"
            for k in kinds
            for corpus in sorted(data)
            if corpus not in per[k]
            and (
                any(group[other] == group[corpus] for other in per[k])
                or any(k in REQUIRED_KINDS.get(lang, ()) for lang in group[corpus])
            )
        )
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


def binary_line(data: dict, run: list[dict]) -> str:
    """The binary under test: the source commit `run_gate.sh` recorded, the build hash and features."""
    builds = {json.dumps(data[c][0].get("build", {}), sort_keys=True) for c in data}
    build = json.loads(builds.pop()) if len(builds) == 1 else {}
    commits = sorted({row.get("source_commit", "") for row in run} - {""})
    parts = [f"`basemind {build.get('version', '?')}`"]
    parts.append(f"source commit `{commits[0]}`" if len(commits) == 1 else "source commit not recorded")
    if build:
        parts.append(f"build `{build.get('build_id', '?')}`")
        features = build.get("features", [])
        shown = f"features `{','.join(features) or 'none'}`"
        # The binary reports `code-intel` as its two parts; say which build flag that was.
        if CODE_INTEL_PARTS <= set(features):
            flags = ["code-intel" if f == "code-intel-js" else f for f in features if f != "code-intel-stack"]
            shown += f" (build flags `{','.join(flags)}`)"
        parts.append(shown)
    else:
        parts.append("build not recorded" if not builds else "the corpora were run by different binaries")
    return "Binary: " + ", ".join(parts) + "."


def load_run(results: Path) -> list[dict]:
    path = results / "run.tsv"
    if not path.exists():
        return []
    header, *rows = path.read_text().splitlines()
    names = header.split("\t")
    return [dict(zip(names, row.split("\t"), strict=False)) for row in rows if row]


def markdown(
    data: dict,
    tools: dict,
    run: list[dict] | None = None,
    languages: dict[str, list[str]] | None = None,
) -> str:
    corpora = sorted(data)
    tokenizers = {c: data[c][0]["tokenizer"] for c in corpora}
    lines = [
        "# Value gate (ADR-0022, by job as ADR-0031 amends it)",
        "",
        f"Corpora: {', '.join(f'`{c}`' for c in corpora)}; tokenizer: "
        + ", ".join(f"{c} `{t}`" for c, t in tokenizers.items())
        + ".",
        "",
        binary_line(data, run or []),
        "",
        "Every job needs a credited share. A reducing kind needs a median ratio; a locating kind a median",
        "floor and mean recall close to the baseline's (`grep` also a symbol accuracy); a correctness kind",
        "has no shell baseline and is judged on credited alone. The thresholds are `src/eval/gate.rs`'s, as",
        "each corpus's `<corpus>.md` prints them. A baseline that errored or printed nothing (`base fail`)",
        "scores recall 0 and is left out of the ratio.",
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
        "| tool | kind | job | corpus | tasks | err | credited | base fail | tool R | tool P | baseline R | tool tok"
        " | baseline tok | x | symbol acc | verdict |",
        "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|",
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
                    f"| `{tool}` | {kind} | {row['job']} | {corpus} | {row['tasks']} | {row['errors']}"
                    f" | {row['credited']} ({row['credited_fraction']:.2f}) | {row['baseline_failed']}"
                    f" | {fmt(row['tool_recall'], '.3f')}"
                    f" | {fmt(row['tool_precision'], '.3f')} | {fmt(row['baseline_recall'], '.3f')}"
                    f" | {fmt(row['tool_tokens_median'], '.0f')} | {fmt(row['baseline_tokens_median'], '.0f')}"
                    f" | {fmt(row['median_ratio'], '.4f')} | {fmt(row['symbol_accuracy'], '.3f')} | {verdict} |"
                )
    lines += [
        "",
        "Recall per language: mean recall of every task of the kind on the corpora of that language, an errored",
        "task counting 0; the baseline column is over the tasks whose shell baseline was scored.",
        "",
        "| language | kind | tasks | tool R | baseline R |",
        "|---|---|---|---|---|",
    ]
    for lang, kinds in recall_by_language(data, languages or {}).items():
        for kind, row in kinds.items():
            lines.append(
                f"| {lang} | {kind} | {row['tasks']} | {row['recall']:.3f} | {fmt(row['baseline_recall'], '.3f')} |"
            )
    return "\n".join(lines) + "\n"


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit("usage: gate.py RESULTS_DIR")
    results = Path(sys.argv[1])
    data = load(results)
    if not data:
        sys.exit(f"no *.report.json in {results}")
    languages = load_languages(Path(__file__).with_name("corpus.tsv"))
    try:
        tools = verdicts(data, languages)
    except ValueError as err:
        sys.exit(f"gate.py: {err}")
    (results / "gate.json").write_text(json.dumps(tools, indent=2, sort_keys=True) + "\n")
    sys.stdout.write(markdown(data, tools, load_run(results), languages))


if __name__ == "__main__":
    main()
