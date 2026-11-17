"""Break a `docs` eval run down by filter selectivity and query variant.

`basemind admin eval` reports one aggregate per mode. This joins its per-task NDJSON with the task file and prints,
per group (all, selectivity, variant, gold file format), the task count, the failed calls and the means of recall@10, nDCG@10, hit@1 and MRR, as a
Markdown table. A task that failed counts as zero in every mean.

    python3 benchmarks/eval/docs/summarize.py TASKS.jsonl RESULTS.ndjson [LABEL]
"""

import json
import sys
from collections import defaultdict
from pathlib import Path

METRICS = ("recall", "ndcg", "hit_at_1", "mrr")


def load(path: str) -> list[dict]:
    with Path(path).open(encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def group_of(task: dict) -> list[str]:
    meta = task.get("meta", {})
    selectivity = meta.get("selectivity", "none")
    gold_format = task["gold"][0].rsplit(".", 1)[-1] if task.get("gold") else "?"
    return ["all", f"selectivity {selectivity}", f"variant {meta.get('variant', '?')}", f"gold .{gold_format}"]


def main() -> None:
    tasks_path, results_path = sys.argv[1], sys.argv[2]
    label = sys.argv[3] if len(sys.argv) > 3 else results_path
    tasks = {task["id"]: task for task in load(tasks_path)}
    rows: dict[str, list[dict]] = defaultdict(list)
    for result in load(results_path):
        for group in group_of(tasks.get(result["id"], {})):
            rows[group].append(result)
    out = [
        f"### {label}",
        "",
        "| Group | Tasks | Failed | recall@10 | nDCG@10 | hit@1 | MRR |",
        "|---|---|---|---|---|---|---|",
    ]
    for group in sorted(rows, key=lambda g: (g != "all", g)):
        results = rows[group]
        failed = sum(1 for r in results if not r.get("ok"))
        means = []
        for metric in METRICS:
            values = [(r.get("score") or {}).get(metric) or 0.0 for r in results]
            means.append(f"{sum(values) / len(values):.3f}")
        out.append(f"| {group} | {len(results)} | {failed} | " + " | ".join(means) + " |")
    sys.stdout.write("\n".join(out) + "\n")


if __name__ == "__main__":
    main()
