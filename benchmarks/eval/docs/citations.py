"""Score the citations `docs_search` gives on an eval workspace, not only the path.

For every task whose gold names a section, run the query through the CLI and take the first hit on the
gold document. Its citation is checked against the task: the heading trail must name `gold_section`, a
PDF's page range must hold `gold_page`, and a Markdown gold's line range must overlap the lines of that
section in the source file. Prints one Markdown table; writes per-task rows to OUT.ndjson.

    python3 -I benchmarks/eval/docs/citations.py BASEMIND_BIN WORKSPACE TASKS.jsonl OUT.ndjson
"""

import json
import subprocess
import sys
from pathlib import Path


def section_lines(source: Path, section: str) -> tuple[int, int] | None:
    """1-based inclusive line span of the Markdown section whose heading names `section`."""
    lines = source.read_text(encoding="utf-8").splitlines()
    start = level = None
    for number, line in enumerate(lines, 1):
        stripped = line.lstrip()
        if not stripped.startswith("#"):
            continue
        depth = len(stripped) - len(stripped.lstrip("#"))
        if start is None and section.lower() in stripped.lower():
            start, level = number, depth
        elif start is not None and depth <= level:
            return start, number - 1
    return (start, len(lines)) if start is not None else None


def first_gold_hit(hits: list[dict], gold: str) -> tuple[int, dict] | None:
    for rank, hit in enumerate(hits, 1):
        if hit.get("path") == gold or gold in (hit.get("also_at") or []):
            return rank, hit
    return None


def main() -> None:
    binary, workspace, tasks_path, out_path = sys.argv[1], Path(sys.argv[2]), sys.argv[3], sys.argv[4]
    with Path(tasks_path).open(encoding="utf-8") as handle:
        tasks = [json.loads(line) for line in handle if line.strip()]
    rows = []
    for task in tasks:
        meta, gold = task.get("meta", {}), task["gold"][0]
        if "gold_section" not in meta:
            continue
        command = [binary, "--root", str(workspace), "--json", "memory", "documents", task["args"]["query"]]
        if task["args"].get("mime_type"):
            command += ["--mime-type", task["args"]["mime_type"]]
        command += ["--limit", "10"]
        done = subprocess.run(command, capture_output=True, text=True, check=False)
        try:
            hits = json.loads(done.stdout).get("hits", [])
        except json.JSONDecodeError:
            hits = []
        row = {"id": task["id"], "gold": gold, "found": False}
        found = first_gold_hit(hits, gold)
        if found:
            rank, hit = found
            cite = hit.get("cite") or {}
            trail = " > ".join(cite.get("section") or [])
            row.update(found=True, rank=rank, section=trail, cite=cite)
            row["section_ok"] = meta["gold_section"].lower() in trail.lower()
            if "gold_page" in meta:
                start, end = cite.get("page_start"), cite.get("page_end") or cite.get("page_start")
                row["page_ok"] = start is not None and start <= meta["gold_page"] <= end
            if gold.endswith(".md"):
                span = section_lines(workspace / gold, meta["gold_section"])
                start, end = cite.get("line_start"), cite.get("line_end")
                row["line_ok"] = bool(span and start and end and start <= span[1] and end >= span[0])
        rows.append(row)
    with Path(out_path).open("w", encoding="utf-8") as handle:
        for row in rows:
            handle.write(json.dumps(row) + "\n")

    def share(key: str) -> str:
        scored = [r[key] for r in rows if key in r]
        return f"{sum(scored)}/{len(scored)}" if scored else "-"

    found = [r for r in rows if r["found"]]
    top = sum(1 for r in found if r["rank"] == 1 and r.get("section_ok"))
    sys.stdout.write(
        "| Tasks with a gold section | Gold found in top 10 | Section right | Page right (PDF) "
        "| Lines in section (.md) | Top hit is the gold section |\n|---|---|---|---|---|---|\n"
        f"| {len(rows)} | {len(found)} | {share('section_ok')} | {share('page_ok')} | {share('line_ok')} "
        f"| {top}/{len(rows)} |\n"
    )


if __name__ == "__main__":
    main()
