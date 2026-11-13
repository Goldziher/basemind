"""Capture paired semantic results from one explicitly isolated, prepared cache."""

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

from layered_overlap import MIN_QUERIES, THRESHOLD, TIERS, compare, load_results

ROUTE_EVENT = re.compile(
    r'semantic search route\s+tier="(code|documents)"\s+route="(standalone|layered)"\s+degraded=(true|false)'
)


def capture(prefix: list[Path | str], cache: Path, tier: str, query: str, route: str) -> dict:
    env = {
        **os.environ,
        "BASEMIND_DATA_HOME": str(cache),
        "BASEMIND_COMMS_DIR": str(cache / "capture-comms"),
        "BASEMIND_INDEX_LAYERED": str(int(route == "layered")),
        "RUST_LOG": "basemind::mcp::memory=debug",
        "NO_COLOR": "1",
    }
    command = ["code", "semantic", "--lane", "semantic"] if tier == "code" else ["memory", "documents"]
    process = subprocess.run(
        [*prefix, *command, "--limit", "10", "--", query],
        env=env,
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )
    evidence = ROUTE_EVENT.findall(process.stderr)
    if process.returncode or evidence != [(tier, route, "false")]:
        raise ValueError(f"{tier} {route}: failed CLI invocation or missing/unexpected route evidence")
    response = json.loads(process.stdout)
    if response.get("budgeted") or response.get("degraded_lanes"):
        raise ValueError(f"{tier} {route}: incomplete results")
    field = "chunk_id" if tier == "code" else "chunk_idx"
    hits = []
    for hit in response["hits"]:
        path, chunk = hit["path"], hit[field]
        valid_chunk = isinstance(chunk, str) and bool(chunk) if tier == "code" else type(chunk) is int and chunk >= 0
        if not isinstance(path, str) or not path or not valid_chunk:
            raise ValueError(f"{tier} {route}: invalid hit identity")
        hits.append(json.dumps([path, chunk], separators=(",", ":")))
    if len(hits) != 10 or len(set(hits)) != 10:
        raise ValueError(f"{tier} {route}: expected ten distinct hits")
    return {"tier": tier, "query": query, "route": route, "degraded": False, "hits": hits}


def read_queries(path: Path) -> dict[str, list[str]]:
    queries = json.loads(path.read_text(encoding="utf-8"))
    if set(queries) != set(TIERS):
        raise ValueError("expected code and documents query lists")
    for tier, values in queries.items():
        if (
            not isinstance(values, list)
            or len(values) < MIN_QUERIES
            or any(not isinstance(query, str) or not query.strip() for query in values)
            or len(set(values)) != len(values)
        ):
            raise ValueError(f"{tier}: expected at least {MIN_QUERIES} unique nonempty queries")
    return queries


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for argument in ("binary", "root", "cache", "queries", "output"):
        parser.add_argument(argument, type=Path)
    args = parser.parse_args()
    try:
        queries = read_queries(args.queries)
        cache = args.cache.resolve(strict=True)
        args.output.mkdir(mode=0o700)
        paths = [args.output / f"{route}.jsonl" for route in ("standalone", "layered")]
        for route, path in zip(("standalone", "layered"), paths, strict=True):
            with path.open("x", encoding="utf-8") as output:
                for tier in TIERS:
                    for query in queries[tier]:
                        row = capture(
                            [args.binary.resolve(), "--json", "--root", args.root.resolve()], cache, tier, query, route
                        )
                        output.write(json.dumps(row) + "\n")
        report = compare(load_results(paths[0], "standalone"), load_results(paths[1], "layered"))
    except (ValueError, TypeError, KeyError, AttributeError, OSError, subprocess.TimeoutExpired) as error:
        parser.exit(2, f"{error}\n")
    sys.stdout.write(json.dumps({"threshold": THRESHOLD, "tiers": report}, indent=2) + "\n")
    return int(any(tier["mean_overlap"] < THRESHOLD for tier in report.values()))


if __name__ == "__main__":
    raise SystemExit(main())
