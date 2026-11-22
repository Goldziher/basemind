"""Break a `docs` eval run down by filter selectivity and query variant.

`basemind admin eval` reports one aggregate per mode. This joins its per-task NDJSON with the task file and prints,
per group (all, selectivity, variant, gold file format), the task count, the failed calls, the means of recall@10, nDCG@10, hit@1
and MRR, and the p50 / p95 of the response tokens and the call latency, as a Markdown table. A task that failed counts as zero
in every mean.

    python3 benchmarks/eval/docs/summarize.py TASKS.jsonl RESULTS.ndjson [LABEL]
"""

import json
import sys
from collections import defaultdict
from pathlib import Path

# The least mean recall@10 over all tasks that keeps `docs-eval` green. A conservative placeholder, since
# no recorded run pins it: the owner sets it from a measured run.
MIN_RECALL_AT_10 = 0.5
METRICS = ("recall", "ndcg", "hit_at_1", "mrr")


def load(path: str) -> list[dict]:
    with Path(path).open(encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def percentile(values: list[float], share: float) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(share * len(ordered)))] if ordered else 0.0


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
        "| Group | Tasks | Failed | recall@10 | nDCG@10 | hit@1 | MRR | tokens p50 / p95 | ms p50 / p95 |",
        "|---|---|---|---|---|---|---|---|---|",
    ]
    for group in sorted(rows, key=lambda g: (g != "all", g)):
        results = rows[group]
        failed = sum(1 for r in results if not r.get("ok"))
        means = []
        for metric in METRICS:
            values = [(r.get("score") or {}).get(metric) or 0.0 for r in results]
            means.append(f"{sum(values) / len(values):.3f}")
        tokens = [float(r.get("tokens") or 0) for r in results]
        millis = [(r.get("elapsed_us") or 0) / 1000 for r in results]
        cost = (
            f"{percentile(tokens, 0.5):.0f} / {percentile(tokens, 0.95):.0f} | "
            f"{percentile(millis, 0.5):.0f} / {percentile(millis, 0.95):.0f}"
        )
        out.append(f"| {group} | {len(results)} | {failed} | " + " | ".join(means) + f" | {cost} |")
    sys.stdout.write("\n".join(out) + "\n")
    everything = rows.get("all", [])
    recalls = [(r.get("score") or {}).get("recall") or 0.0 for r in everything]
    mean = sum(recalls) / len(recalls) if recalls else 0.0
    if mean < MIN_RECALL_AT_10:
        sys.exit(f"recall@10 {mean:.3f} is below the floor {MIN_RECALL_AT_10} over {len(recalls)} tasks")


if __name__ == "__main__":
    main()
