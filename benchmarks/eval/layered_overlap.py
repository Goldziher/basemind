"""Check paired top-10 results recorded from the two search routes."""

import argparse
import json
import sys
from pathlib import Path

TIERS = ("code", "documents")
MIN_QUERIES = 200
TOP_K = 10
THRESHOLD = 0.95


Results = dict[str, dict[str, set[str]]]


def load_results(path: Path, route: str) -> Results:
    results = {tier: {} for tier in TIERS}
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        row = json.loads(line)
        tier = row["tier"]
        query = row["query"]
        hits = row["hits"]
        if row["route"] != route or row.get("degraded") is not False:
            raise ValueError(f"{path}:{number}: expected successful {route} route evidence")
        if not isinstance(query, str) or not query.strip() or query in results[tier]:
            raise ValueError(f"{path}:{number}: missing or duplicate query")
        if (
            not isinstance(hits, list)
            or len(hits) != TOP_K
            or any(not isinstance(hit, str) or not hit for hit in hits)
            or len(set(hits)) != TOP_K
        ):
            raise ValueError(f"{path}:{number}: expected ten distinct hit identities")
        results[tier][query] = set(hits)
    return results


def compare(standalone: Results, layered: Results) -> dict[str, dict[str, float | int]]:
    report = {}
    for tier in TIERS:
        baseline, candidate = standalone[tier], layered[tier]
        if baseline.keys() != candidate.keys() or len(baseline) < MIN_QUERIES:
            raise ValueError(f"{tier}: expected at least {MIN_QUERIES} exactly matched queries")
        overlap = sum(len(hits & candidate[query]) for query, hits in baseline.items())
        report[tier] = {"queries": len(baseline), "mean_overlap": overlap / (TOP_K * len(baseline))}
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("standalone", type=Path)
    parser.add_argument("layered", type=Path)
    args = parser.parse_args()
    try:
        report = compare(load_results(args.standalone, "standalone"), load_results(args.layered, "layered"))
    except (ValueError, TypeError, KeyError, OSError) as error:
        parser.exit(2, f"{error}\n")
    sys.stdout.write(json.dumps({"threshold": THRESHOLD, "tiers": report}, indent=2) + "\n")
    return int(any(row["mean_overlap"] < THRESHOLD for row in report.values()))


if __name__ == "__main__":
    raise SystemExit(main())
